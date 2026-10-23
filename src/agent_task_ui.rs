//! GTK-side state and helpers for the agent Tasks panel.
//!
//! The provider-neutral domain (task reducer, native runtime, worktree
//! containment, validation preflight) lives in [`crate::agent_task`]. This
//! module owns only the pieces the Relm4 update/view loop needs: panel view
//! projections, background worktree creation, the prompt-consent projection
//! of the user configuration, and the validation terminal's argv/environment
//! contract. Everything here is either pure or structured so the pure parts
//! are headless-testable. Ported from ember's `app/tasks.rs` and frost's
//! `agent_task_ui.rs`.

#[path = "task_composer.rs"]
pub(crate) mod task_composer;
pub(crate) use task_composer::TaskProjectRequest;

/// Resolve the current binding without interpreting its opaque session identity.
pub(crate) fn task_terminal_binding(
    task: &crate::agent_task::AgentTask,
    role: crate::agent_task::TaskTerminalRole,
) -> Option<&str> {
    match role {
        crate::agent_task::TaskTerminalRole::Agent => task.terminal_session_id.as_deref(),
        crate::agent_task::TaskTerminalRole::Validation => {
            task.validation.terminal_session_id.as_deref()
        }
    }
}

/// Resolve against live identities, never against an old tab or pane index.
pub(crate) fn resolve_task_terminal<T>(
    binding: Option<&str>,
    terminals: impl IntoIterator<Item = (String, T)>,
) -> Option<T> {
    let binding = binding?;
    terminals
        .into_iter()
        .find_map(|(identity, target)| (identity == binding).then_some(target))
}

pub(crate) fn task_focus_allowed(
    visible: bool,
    selected: Option<TaskId>,
    requested: TaskId,
) -> bool {
    visible && selected == Some(requested)
}

/// A display-only reason, selected from authoritative task/validation details.
pub(crate) fn task_reason(
    status: crate::agent_task::TaskStatus,
    validation: crate::agent_task::TaskValidationStatus,
    status_detail: Option<&str>,
    validation_detail: Option<&str>,
) -> Option<String> {
    use crate::agent_task::{TaskStatus, TaskValidationStatus};
    let nonempty = |text: &&str| !text.trim().is_empty();
    let validation_reason = matches!(
        validation,
        TaskValidationStatus::Failed | TaskValidationStatus::Inconclusive
    )
    .then_some(validation_detail)
    .flatten()
    .filter(nonempty);
    let task_reason = matches!(
        status,
        TaskStatus::Failed | TaskStatus::WaitingForHuman | TaskStatus::WaitingForApproval
    )
    .then_some(status_detail)
    .flatten()
    .filter(nonempty);
    validation_reason
        .or(task_reason)
        .map(|text| jterm_core::review_input::safe_inline_display(text.trim(), 1024))
}

use crate::agent_task::{
    AgentProvider, ManagedWorktree, NativePromptPolicy, SemanticCommandContext, TaskId,
    WorktreeService, CODEX_APP_SERVER_LIVE_TURN_MAX, NATIVE_AGENT_FOLLOW_UP_MAX_BYTES,
};
use crate::config::Config;
use crate::review_text::visible_bounded;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;

/// Display bound for one task title in list rows.
pub(crate) const MAX_TASK_TITLE_DISPLAY_BYTES: usize = 112;
/// Display bound for branch names, status details, and validation details.
pub(crate) const MAX_TASK_DETAIL_DISPLAY_BYTES: usize = 256;

/// Consent projection for any native provider prompt. `share_command_context`
/// requires both the AI master switch and the explicit command-context
/// sharing opt-in; secret redaction follows the user's AI redaction policy.
pub(crate) fn prompt_policy(config: &Config) -> NativePromptPolicy {
    NativePromptPolicy {
        share_command_context: config.ai_enabled && config.ai_share_command_context,
        redact_secrets: config.ai_redact_secrets,
    }
}

/// Stable string identity for one task terminal at the task boundary.
///
/// Task metadata outlives tab/pane positions, so the reducer keys terminal
/// bindings on this string rather than a pane index. The grammar matches the
/// family-shared jsh session-id rule (alphanumeric, `-`, `_`).
pub(crate) fn terminal_session_id(pane_id: u64) -> String {
    format!("anvil-{}-{pane_id}", std::process::id())
}

/// A follow-up turn may be sent only when it carries visible text, stays
/// inside the native byte budget, and the live session has turn headroom.
pub(crate) fn native_follow_up_can_send(text: &str, completed_turns: usize) -> bool {
    !text
        .trim_matches(|character| matches!(character, ' ' | '\n' | '\t'))
        .is_empty()
        && text.len() <= NATIVE_AGENT_FOLLOW_UP_MAX_BYTES
        && completed_turns < CODEX_APP_SERVER_LIVE_TURN_MAX
}

/// Actions the GTK panel stages. Execution resolves the task again at the
/// application layer, so a concurrent terminal exit or tab removal cannot
/// redirect an action to an unrelated task. Mirrors ember's
/// `TaskSidebarAction`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TaskPanelAction {
    /// Create a task from the active pane's selected block evidence.
    CreateFromBlock,
    ComposeProject,
    CreateProject(TaskProjectRequest),
    StartNative(TaskId),
    StartTerminal(TaskId),
    FocusTerminal(TaskId),
    FocusValidation(TaskId),
    StopCodex(TaskId),
    FollowUp(TaskId, String),
    FinishCodex(TaskId),
    Approve(TaskId, crate::agent_task::ApprovalId),
    Deny(TaskId, crate::agent_task::ApprovalId),
    RunValidation(TaskId),
    Complete(TaskId),
    AcceptReviewed(TaskId),
    ReviewDiff(TaskId),
    Archive(TaskId),
    Select(TaskId),
    Close,
}

/// Read-only details use the shared bounded projection and contain no inferred history.
pub(crate) fn task_inspector_text(task: &crate::agent_task::AgentTask) -> String {
    let inspector = crate::agent_task::TaskInspector::from_task(task);
    let mut text = inspector
        .fields
        .into_iter()
        .map(|field| format!("{}\n{}", field.label, field.value))
        .collect::<Vec<_>>()
        .join("\n\n");
    if let Some(goal) = inspector.goal {
        text.push_str("\n\nGoal\n");
        text.push_str(&goal);
    }
    if task.brief.is_some() {
        text.push_str("\n\nTerminal brief\n");
        text.push_str(&format!("On Start terminal, read {} inside the task worktree. Sharing consent is required before the brief is written.", crate::agent_task::pty_task_brief_relative()));
    }
    text
}

/// Deliver an authored goal only after the existing explicit sharing gate allows it.
pub(crate) fn prepare_authored_terminal(
    task: &crate::agent_task::AgentTask,
    policy: NativePromptPolicy,
    launch: crate::agent_task::AgentLaunchSpec,
) -> Result<crate::agent_task::AgentLaunchSpec, String> {
    if task.brief.is_none() {
        return Ok(launch);
    }
    let brief =
        crate::agent_task::write_pty_task_brief(task, policy).map_err(|error| error.to_string())?;
    if brief.is_none() {
        return Err(
            "Enable AI features and command-context sharing before starting this authored task"
                .into(),
        );
    }
    Ok(launch.with_pty_seed_prompt(&format!(
        "Read {} and carry out the task goal inside this isolated worktree.",
        crate::agent_task::pty_task_brief_relative()
    )))
}

/// Fully prepared task registration produced by the background worktree
/// worker. The UI thread only registers it with the task manager.
pub(crate) struct PreparedTask {
    pub(crate) project_request: Option<TaskProjectRequest>,
    pub(crate) context: Option<SemanticCommandContext>,
    pub(crate) brief: Option<crate::agent_task::TaskBrief>,
    pub(crate) title: String,
    pub(crate) provider: AgentProvider,
    pub(crate) worktree: ManagedWorktree,
}

/// In-flight isolated-worktree creation for one new task.
///
/// Git operations run on a bounded worker thread so the UI never blocks; the
/// cancel flag lets panel teardown ask the worker to stop early. Dropping the
/// receiver without registering the result leaves the created worktree to the
/// managed root's ordinary cleanup.
pub(crate) struct PendingTaskCreation {
    pub(crate) receiver: Receiver<Result<PreparedTask, String>>,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for PendingTaskCreation {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Start creating one isolated task worktree off the UI thread.
///
/// The source command's recorded cwd anchors the repository lookup; the
/// worker resolves the repository root, creates an `anvil/task-<token>`
/// branch worktree under the per-user data directory, and returns everything
/// the UI thread needs to register the task atomically.
pub(crate) fn begin_worktree_creation(
    context: SemanticCommandContext,
    provider: AgentProvider,
) -> Result<PendingTaskCreation, String> {
    let cwd = context
        .cwd
        .as_deref()
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "source command has no working directory".to_string())?;
    let command = context.command.as_deref().unwrap_or("failed command");
    let title = format!(
        "Fix {}",
        visible_bounded(command, MAX_TASK_TITLE_DISPLAY_BYTES)
    );
    begin_task_worktree_creation(cwd, title, provider, Some(context), None, None)
}

pub(crate) fn begin_project_worktree_creation(
    request: TaskProjectRequest,
) -> Result<PendingTaskCreation, String> {
    let draft = request.draft.clone();
    draft.validate()?;
    if request.cancelled() {
        return Err("Task creation was cancelled".into());
    }
    begin_task_worktree_creation(
        draft.directory,
        draft.title,
        draft.provider,
        None,
        Some(draft.brief),
        Some(request),
    )
}

fn begin_task_worktree_creation(
    cwd: PathBuf,
    title: String,
    provider: AgentProvider,
    context: Option<SemanticCommandContext>,
    brief: Option<crate::agent_task::TaskBrief>,
    project_request: Option<TaskProjectRequest>,
) -> Result<PendingTaskCreation, String> {
    let worktree_root = dirs::data_local_dir()
        .ok_or_else(|| "cannot locate the per-user data directory".to_string())?
        .join("anvil")
        .join("agent-tasks");
    let token = uuid::Uuid::new_v4().simple().to_string();
    let task_name = format!("task-{token}");
    let branch = format!("anvil/{task_name}");
    let (sender, receiver) = mpsc::sync_channel(1);
    let cancel = project_request
        .as_ref()
        .map(TaskProjectRequest::worktree_cancel_flag)
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let worker_cancel = Arc::clone(&cancel);
    let worker = std::thread::Builder::new()
        .name("anvil-task-worktree".to_string())
        .spawn(move || {
            let result: Result<PreparedTask, String> = (|| {
                let service = WorktreeService::new(worktree_root)
                    .map_err(|error| error.to_string())?
                    .with_cancel_flag(worker_cancel);
                let repository = service
                    .resolve_repository_root(&cwd)
                    .map_err(|error| error.to_string())?;
                let request = crate::agent_task::CreateWorktreeRequest::new(
                    repository, task_name, branch, "HEAD",
                );
                let worktree = service
                    .create(&request)
                    .map_err(|error| error.to_string())?;
                Ok(PreparedTask {
                    project_request: project_request.clone(),
                    context,
                    brief,
                    title,
                    provider,
                    worktree,
                })
            })();
            if let (Some(request), Err(error)) = (&project_request, &result) {
                request.finish(Err(error.clone()));
            }
            let _ = sender.send(result);
        })
        .map_err(|error| format!("could not start task worktree worker: {error}"))?;
    Ok(PendingTaskCreation {
        receiver,
        cancel,
        worker: Some(worker),
    })
}

/// True when `path` names an interactive jsh build, including
/// version-suffixed binaries. Anything resolving to another basename is not
/// treated as jsh.
fn is_interactive_jsh(path: &std::path::Path) -> bool {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let Some(name) = resolved.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name == "jsh" || name.starts_with("jsh-") || name.starts_with("jsh.")
}

/// Resolve the shell captured by the source pane and return an explicit argv
/// for a single user-approved validation command.
///
/// Validation runs in a fresh process inside the task worktree. Passing the
/// command as one argv element (rather than interpolating it into a wrapper
/// script) preserves its exact shell syntax and avoids a second quoting
/// language. Command mode deliberately is not login mode: a login profile may
/// change directory after the PTY has entered the validated worktree, causing
/// the command to run against unrelated files. Supported shells also receive
/// their no-rc flag; unknown shell families fail closed because their
/// non-interactive startup contract is not known.
pub(crate) fn validation_command_argv(
    source_shell: Option<&str>,
    command: &str,
) -> Result<Vec<String>, String> {
    use std::ffi::OsStr;
    use std::path::Path;

    let source_shell = source_shell
        .filter(|shell| !shell.is_empty())
        .ok_or_else(|| "Validation source shell identity is missing".to_string())?;
    let shell = jterm_core::host::resolve_configured_program(source_shell, None)
        .ok_or_else(|| format!("Validation source shell is no longer executable: {source_shell}"))?
        .to_string_lossy()
        .into_owned();
    if is_interactive_jsh(Path::new(&shell)) {
        return Ok(vec![
            shell,
            "--norc".to_string(),
            "-c".to_string(),
            command.to_string(),
        ]);
    }
    let resolved = std::fs::canonicalize(&shell).unwrap_or_else(|_| Path::new(&shell).into());
    let family = resolved
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut argv = vec![shell];
    match family.as_str() {
        "bash" => argv.extend(["--noprofile".to_string(), "--norc".to_string()]),
        "zsh" => argv.push("-f".to_string()),
        "fish" => argv.push("--no-config".to_string()),
        "sh" | "dash" | "ksh" | "ksh93" | "mksh" => {}
        _ => {
            return Err(format!(
                "Unsupported source shell for isolated validation: {}",
                resolved.display()
            ));
        }
    }
    argv.extend(["-c".to_string(), command.to_string()]);
    Ok(argv)
}

/// Environment overrides that keep a validation child from sourcing startup
/// files even when the shell family is only partially covered by argv flags.
pub(crate) const VALIDATION_ENV_OVERRIDES: [(&str, &str); 3] = [
    ("BASH_ENV", "/dev/null"),
    ("ENV", "/dev/null"),
    ("ZDOTDIR", "/dev/null"),
];

/// Everything the panel needs to know about one task row, resolved once at
/// the application layer so the view never reaches into the domain model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskRowSnapshot {
    pub(crate) id: TaskId,
    pub(crate) title: String,
    pub(crate) provider: AgentProvider,
    pub(crate) status: crate::agent_task::TaskStatus,
    pub(crate) runtime_kind: crate::agent_task::TaskRuntimeKind,
    pub(crate) branch: String,
    pub(crate) has_agent_terminal: bool,
    pub(crate) has_validation_terminal: bool,
    pub(crate) has_active_agent_stream: bool,
    pub(crate) native_preparing: bool,
    pub(crate) validation_status: crate::agent_task::TaskValidationStatus,
    pub(crate) validation_attempt: u64,
    pub(crate) needs_attention: bool,
    pub(crate) status_detail: Option<String>,
    pub(crate) validation_detail: Option<String>,
}

impl TaskRowSnapshot {
    pub(crate) fn is_running(&self) -> bool {
        self.status.is_running()
            || self.validation_status == crate::agent_task::TaskValidationStatus::Running
            || self.has_active_agent_stream
            || self.native_preparing
    }

    fn group_rank(&self) -> u8 {
        if self.needs_attention {
            0
        } else if self.is_running() {
            1
        } else {
            2
        }
    }
}

/// Dashboard ordering: attention first, then running, then everything else;
/// newest first inside each group. Stable across transient status flaps
/// because the tiebreaker is the task's own update timestamp.
pub(crate) fn order_task_rows(rows: &mut [TaskRowSnapshot], updated_at_ms: impl Fn(TaskId) -> u64) {
    rows.sort_by_key(|row| (row.group_rank(), std::cmp::Reverse(updated_at_ms(row.id))));
}

/// One-line status summary for a row, bounded for display.
pub(crate) fn row_status_line(row: &TaskRowSnapshot) -> String {
    let base = if row.native_preparing {
        "preparing native Codex".to_string()
    } else {
        row.status.label().to_string()
    };
    let validation = match row.validation_status {
        crate::agent_task::TaskValidationStatus::NotRun => String::new(),
        status => format!(" · validation {}", status.label()),
    };
    visible_bounded(
        &format!("{base}{validation}"),
        MAX_TASK_DETAIL_DISPLAY_BYTES,
    )
}

/// Separate content changes from identity/order changes. Content-only updates
/// preserve the selected widget and must not emit a programmatic selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ListRefreshPlan {
    pub(crate) update_rows: bool,
    pub(crate) reconcile_order: bool,
    pub(crate) apply_selection: bool,
    pub(crate) select_index: Option<usize>,
}

pub(crate) fn plan_list_refresh(
    rendered_rows: &[TaskRowSnapshot],
    rendered_selected: Option<TaskId>,
    sync_rows: &[TaskRowSnapshot],
    sync_selected: Option<TaskId>,
) -> ListRefreshPlan {
    let update_rows = rendered_rows != sync_rows;
    let reconcile_order = !rendered_rows
        .iter()
        .map(|row| row.id)
        .eq(sync_rows.iter().map(|row| row.id));
    let select_index = sync_selected.and_then(|id| sync_rows.iter().position(|row| row.id == id));
    ListRefreshPlan {
        update_rows,
        reconcile_order,
        apply_selection: rendered_selected != sync_selected || reconcile_order,
        select_index,
    }
}

/// Display bounds for the native stream text projection.
pub(crate) const MAX_NATIVE_STREAM_DISPLAY_BYTES: usize = 64 * 1024;
const MAX_NATIVE_ITEM_DISPLAY_BYTES: usize = 8 * 1024;
const MAX_NATIVE_STREAM_STATUS_BYTES: usize = 1024;
const MAX_NATIVE_STREAM_PANE_BYTES: usize =
    (MAX_NATIVE_STREAM_DISPLAY_BYTES - MAX_NATIVE_STREAM_STATUS_BYTES) / 2;

/// A compact, pinned summary remains visible while either text pane scrolls.
/// Its reserved budget is part of the same total as the two text buffers.
pub(crate) fn render_stream_status(
    snapshot: &crate::agent_task::CodexAppServerViewSnapshot,
) -> String {
    let turn = snapshot.displayed_turn_ordinal.map_or_else(
        || "latest turn".to_string(),
        |ordinal| format!("turn {ordinal}"),
    );
    let mut status = format!("phase: {} · {turn}", phase_label(snapshot.phase));
    if let Some(error) = &snapshot.last_error {
        status.push_str("\nLatest error (preview): ");
        status.push_str(&jterm_core::review_input::safe_inline_display(error, 512));
    }
    debug_assert!(status.len() <= MAX_NATIVE_STREAM_STATUS_BYTES);
    status
}

fn phase_label(phase: crate::agent_task::CodexAppServerPhase) -> &'static str {
    use crate::agent_task::CodexAppServerPhase::*;
    match phase {
        Created => "created",
        Spawning => "spawning",
        Initializing => "initializing",
        StartingThread => "starting thread",
        StartingTurn => "starting turn",
        Ready => "ready",
        Running => "running",
        WaitingForApproval => "waiting for approval",
        Cancelling => "cancelling",
        Stopping => "stopping",
        Ended => "ended",
        Failed => "failed",
    }
}

/// Every fragment, including headings and truncation notices, shares one
/// UTF-8 byte budget. Stream text preserves structural newlines/tabs while
/// neutralizing the same controls and visual spoofing as other review text.
#[derive(Default)]
struct StreamDisplay {
    text: String,
    truncated: bool,
}

impl StreamDisplay {
    fn push_str(&mut self, text: &str) {
        if self.truncated || text.is_empty() {
            return;
        }
        let remaining = MAX_NATIVE_STREAM_PANE_BYTES.saturating_sub(self.text.len());
        // Four look-ahead bytes cover any UTF-8 scalar. Even if the shared
        // helper adds its ellipsis, overflow still leaves more than `remaining`
        // bytes, including when a scalar cannot fit the final one or two bytes.
        let shown = jterm_core::review_input::safe_multiline_display(text, remaining + 4);
        let mut end = shown.len().min(remaining);
        self.truncated = shown.len() > remaining;
        while !shown.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&shown[..end]);
    }

    fn push(&mut self, character: char) {
        self.push_str(character.encode_utf8(&mut [0; 4]));
    }

    fn finish(mut self) -> String {
        if self.truncated {
            const NOTICE: &str = "\n(stream display truncated)";
            let mut end = self
                .text
                .len()
                .min(MAX_NATIVE_STREAM_PANE_BYTES - NOTICE.len());
            while !self.text.is_char_boundary(end) {
                end -= 1;
            }
            self.text.truncate(end);
            self.text.push_str(NOTICE);
        }
        self.text
    }
}

fn push_bounded(out: &mut StreamDisplay, text: &str) {
    let shown =
        jterm_core::review_input::safe_multiline_display(text, MAX_NATIVE_ITEM_DISPLAY_BYTES + 4);
    let mut end = shown.len().min(MAX_NATIVE_ITEM_DISPLAY_BYTES);
    while !shown.is_char_boundary(end) {
        end -= 1;
    }
    out.push_str(&shown[..end]);
    if shown.len() > end {
        out.push_str("\n(item display truncated)");
    }
}

/// Render one native session snapshot as bounded plain text for the panel's
/// stream view. Provider-controlled strings are already display-safe and
/// bounded by the worker; this projection re-bounds the composed whole and
/// never treats any of it as markup.
/// The current pane and one history page share the unchanged 64 KiB total
/// with the reserved pinned status summary. Paging borrows a single retained turn; it never clones logs.
pub(crate) fn stream_history_index(
    snapshot: &crate::agent_task::CodexAppServerViewSnapshot,
    ordinal: Option<usize>,
) -> Option<usize> {
    let turns = &snapshot.turn_history;
    if turns.is_empty() {
        return None;
    }
    Some(ordinal.map_or(turns.len() - 1, |ordinal| {
        turns
            .iter()
            .position(|turn| turn.ordinal >= ordinal)
            .unwrap_or(turns.len() - 1)
    }))
}

pub(crate) fn render_stream_history(
    snapshot: &crate::agent_task::CodexAppServerViewSnapshot,
    ordinal: Option<usize>,
) -> String {
    let mut out = StreamDisplay::default();
    let index = stream_history_index(snapshot, ordinal);
    if snapshot.dropped_turns > 0 {
        out.push_str(&format!(
            "\n({} oldest turns elided by the retained-history budget)",
            snapshot.dropped_turns
        ));
    }
    if let Some(index) = index {
        out.push_str(&format!(
            "\nRetained history: turn {}. Other retained turns are available with Older/Newer.",
            snapshot.turn_history[index].ordinal
        ));
    } else if snapshot.dropped_turns == 0 {
        return String::new();
    }
    if let Some(turn) = index.and_then(|index| snapshot.turn_history.get(index)) {
        out.push_str(&format!("\n\n— turn {} —", turn.ordinal));
        if turn.dropped_updates > 0 {
            out.push_str(&format!(
                "\n({} updates lost to the live-turn retention budget)",
                turn.dropped_updates
            ));
        }
        if turn.agent_text_truncated
            || turn.commands.iter().any(|command| command.output_omitted)
            || turn
                .file_changes
                .iter()
                .any(|change| change.changes_truncated || change.path_truncated)
        {
            out.push_str("\n(text, output or file details omitted by retention budget)");
        }
        if let Some(feedback) = &turn.follow_up_feedback {
            push_bounded(&mut out, &format!("\n> {feedback}"));
        }
        if !turn.agent_text.is_empty() {
            out.push('\n');
            push_bounded(&mut out, &turn.agent_text);
        }
        for command in &turn.commands {
            if out.truncated {
                break;
            }
            push_bounded(
                &mut out,
                &format!(
                    "\n$ {} [{}]{}",
                    command.command,
                    command.status,
                    if command.output_omitted {
                        " (output omitted from retained history)"
                    } else {
                        ""
                    }
                ),
            );
        }
        for change in &turn.file_changes {
            if out.truncated {
                break;
            }
            push_bounded(
                &mut out,
                &format!(
                    "\n~ {} {} ({} change{})",
                    change.status,
                    change.path.as_deref().unwrap_or("(path elided)"),
                    change.change_count,
                    if change.change_count == 1 { "" } else { "s" }
                ),
            );
        }
    }
    out.finish()
}

pub(crate) fn render_stream_text(
    snapshot: &crate::agent_task::CodexAppServerViewSnapshot,
) -> String {
    let mut out = StreamDisplay::default();
    out.push_str(&format!("phase: {}", phase_label(snapshot.phase)));
    if snapshot.dropped_updates > 0 {
        out.push_str(&format!(
            "\n({} updates lost to the live-turn retention budget)",
            snapshot.dropped_updates
        ));
    }
    if snapshot.agent_text_truncated
        || snapshot
            .commands
            .iter()
            .any(|command| command.output_truncated)
        || snapshot
            .file_changes
            .iter()
            .any(|change| change.changes_truncated)
    {
        out.push_str("\n(text, output or file details omitted by retention budget)");
    }
    if let Some(error) = &snapshot.last_error {
        out.push_str("\nerror: ");
        push_bounded(&mut out, error);
    }

    let shows_live_projection = snapshot.displayed_turn_ordinal.is_some()
        || !snapshot.agent_text.is_empty()
        || !snapshot.commands.is_empty()
        || !snapshot.file_changes.is_empty();
    if shows_live_projection {
        let heading = match snapshot.displayed_turn_ordinal {
            Some(ordinal) => format!("\n\n— turn {ordinal} (latest) —"),
            None => "\n\n— latest —".to_string(),
        };
        out.push_str(&heading);
        if let Some(feedback) = &snapshot.displayed_follow_up_feedback {
            push_bounded(&mut out, &format!("\n> {feedback}"));
        }
        if !snapshot.agent_text.is_empty() {
            out.push('\n');
            push_bounded(&mut out, &snapshot.agent_text);
        }
        for command in &snapshot.commands {
            if out.truncated {
                break;
            }
            push_bounded(
                &mut out,
                &format!("\n$ {} [{}]", command.command, command.status),
            );
            if !command.output.is_empty() {
                out.push('\n');
                push_bounded(&mut out, &command.output);
                if command.output_truncated {
                    out.push_str(" (text omitted by retention budget)");
                }
            }
        }
        for change in &snapshot.file_changes {
            if out.truncated {
                break;
            }
            push_bounded(
                &mut out,
                &format!(
                    "\n~ {} ({} file item{})",
                    change.status,
                    change.changes.len(),
                    if change.changes.len() == 1 { "" } else { "s" }
                ),
            );
            for file in &change.changes {
                if out.truncated {
                    break;
                }
                push_bounded(&mut out, &format!("\n  {} {}", file.kind, file.path));
            }
        }
    }
    out.finish()
}

/// One pending approval rendered for the action area: stable identity plus a
/// bounded, display-safe summary line.
pub(crate) fn approval_summary(approval: &crate::agent_task::CodexAppServerApproval) -> String {
    let detail = match approval.kind {
        crate::agent_task::CodexAppServerApprovalKind::Command => approval
            .command
            .as_deref()
            .unwrap_or("(command elided)")
            .to_string(),
        crate::agent_task::CodexAppServerApprovalKind::FileChange => {
            if approval.file_paths.is_empty() {
                "(paths elided)".to_string()
            } else {
                approval.file_paths.join(", ")
            }
        }
    };
    let reason = approval
        .reason
        .as_deref()
        .filter(|reason| !reason.trim().is_empty())
        .map(|reason| format!(" — {reason}"))
        .unwrap_or_default();
    crate::review_text::visible_bounded(
        &format!(
            "{}: {detail}{reason}",
            match approval.kind {
                crate::agent_task::CodexAppServerApprovalKind::Command => "run",
                crate::agent_task::CodexAppServerApprovalKind::FileChange => "edit",
            }
        ),
        MAX_TASK_DETAIL_DISPLAY_BYTES,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn task_reason_prefers_validation_and_drops_stale_or_empty_detail() {
        use crate::agent_task::{TaskStatus as S, TaskValidationStatus as V};
        assert_eq!(
            super::task_reason(
                S::Failed,
                V::Failed,
                Some("task failed"),
                Some("tests failed")
            )
            .as_deref(),
            Some("tests failed")
        );
        assert_eq!(
            super::task_reason(
                S::Failed,
                V::Inconclusive,
                Some("task failed"),
                Some("no exit status")
            )
            .as_deref(),
            Some("no exit status")
        );
        assert_eq!(
            super::task_reason(S::Failed, V::Failed, Some("task failed"), Some("  ")).as_deref(),
            Some("task failed")
        );
        for status in [S::WaitingForHuman, S::WaitingForApproval] {
            assert_eq!(
                super::task_reason(status, V::NotRun, Some("needs a decision"), None).as_deref(),
                Some("needs a decision")
            );
        }
        assert_eq!(
            super::task_reason(
                S::Completed,
                V::Passed,
                Some("old failure"),
                Some("old tests")
            ),
            None
        );
        assert_eq!(
            super::task_reason(S::Failed, V::NotRun, Some(" \n\t "), None),
            None
        );
        assert_eq!(super::task_reason(S::Failed, V::NotRun, None, None), None);
        let unsafe_detail = format!("bad\u{1b}\n\u{202e}{}", "界".repeat(800));
        let reason = super::task_reason(S::Failed, V::NotRun, Some(&unsafe_detail), None).unwrap();
        assert!(reason.len() <= 1024);
        assert!(!reason.chars().any(char::is_control));
        assert!(!reason.contains('\u{202e}'));
        assert_eq!(
            super::task_reason(S::Failed, V::NotRun, Some("updated"), None).as_deref(),
            Some("updated")
        );
    }

    #[test]
    fn task_terminal_resolution_survives_reorder_and_rejects_replacement() {
        use crate::agent_task::{AgentProvider, NewTask, TaskManager, TaskTerminalRole};
        let mut manager = TaskManager::new();
        let mut ids = Vec::new();
        for title in ["first", "second"] {
            ids.push(
                manager
                    .create(NewTask {
                        title: title.into(),
                        provider: AgentProvider::Codex,
                        repo_root: "/fixture".into(),
                        worktree_path: "/fixture/worktree".into(),
                        branch: "test/focus".into(),
                        base_commit: "a".repeat(40),
                        source_context: None,
                    })
                    .unwrap(),
            );
        }
        manager
            .bind_terminal_session(ids[0], "terminal-first".into())
            .unwrap();
        manager
            .bind_terminal_session(ids[1], "terminal-second".into())
            .unwrap();
        manager
            .update_status(ids[1], crate::agent_task::TaskStatus::ReadyForReview, None)
            .unwrap();
        manager
            .bind_validation_session(ids[1], "validation-second".into())
            .unwrap();
        manager.handle_terminal_session_exit("validation-second", Some(0));
        let before = manager.tasks().to_vec();
        let binding =
            super::task_terminal_binding(manager.get(ids[1]).unwrap(), TaskTerminalRole::Agent);
        assert_eq!(
            super::resolve_task_terminal(
                binding,
                [
                    ("terminal-first".into(), 11),
                    ("terminal-second".into(), 22)
                ]
            ),
            Some(22)
        );
        assert_eq!(
            super::resolve_task_terminal(
                binding,
                [
                    ("terminal-second".into(), 22),
                    ("terminal-first".into(), 11)
                ]
            ),
            Some(22)
        );
        assert_eq!(
            super::resolve_task_terminal(
                binding,
                [("replacement".into(), 22), ("terminal-first".into(), 11)]
            ),
            None
        );
        let validation = super::task_terminal_binding(
            manager.get(ids[1]).unwrap(),
            TaskTerminalRole::Validation,
        );
        assert_eq!(
            super::resolve_task_terminal(validation, [("validation-second".into(), 33)]),
            Some(33),
            "finished validation output stays focusable"
        );
        assert!(super::task_focus_allowed(true, Some(ids[1]), ids[1]));
        assert!(!super::task_focus_allowed(false, Some(ids[1]), ids[1]));
        assert!(!super::task_focus_allowed(true, Some(ids[0]), ids[1]));
        assert_eq!(format!("{:?}", manager.tasks()), format!("{before:?}"));
    }

    use super::*;
    use crate::agent_task::{TaskStatus, TaskValidationStatus};

    #[test]
    fn prompt_policy_requires_both_ai_and_sharing_consent() {
        let mut config = Config::safe_defaults();
        assert!(!prompt_policy(&config).share_command_context);
        config.ai_enabled = true;
        assert!(!prompt_policy(&config).share_command_context);
        config.ai_share_command_context = true;
        assert!(prompt_policy(&config).share_command_context);
        assert!(prompt_policy(&config).redact_secrets);
        config.ai_redact_secrets = false;
        assert!(!prompt_policy(&config).redact_secrets);
    }

    #[test]
    fn terminal_session_ids_match_the_jsh_grammar_and_distinguish_panes() {
        let first = terminal_session_id(1);
        let second = terminal_session_id(2);
        assert!(jterm_core::execution_journal::is_valid_jsh_session_id(
            &first
        ));
        assert!(jterm_core::execution_journal::is_valid_jsh_session_id(
            &second
        ));
        assert_ne!(first, second);
    }

    #[test]
    fn follow_up_gate_bounds_text_and_turn_count() {
        assert!(!native_follow_up_can_send("", 0));
        assert!(!native_follow_up_can_send("  \n\t ", 0));
        assert!(native_follow_up_can_send("please adjust the fix", 0));
        assert!(!native_follow_up_can_send(
            "x".repeat(NATIVE_AGENT_FOLLOW_UP_MAX_BYTES + 1).as_str(),
            0
        ));
        assert!(native_follow_up_can_send(
            "x".repeat(NATIVE_AGENT_FOLLOW_UP_MAX_BYTES).as_str(),
            CODEX_APP_SERVER_LIVE_TURN_MAX - 1
        ));
        assert!(!native_follow_up_can_send(
            "ok",
            CODEX_APP_SERVER_LIVE_TURN_MAX
        ));
    }

    #[test]
    fn validation_argv_uses_non_login_no_rc_command_mode() {
        let bash = validation_command_argv(Some("/bin/bash"), "cargo test")
            .expect("bash is a supported validation shell");
        assert_eq!(&bash[1..], ["--noprofile", "--norc", "-c", "cargo test"]);

        let sh = validation_command_argv(Some("/bin/sh"), "cargo test")
            .expect("sh is a supported validation shell");
        assert_eq!(&sh[1..], ["-c", "cargo test"]);

        assert!(validation_command_argv(None, "cargo test").is_err());
        assert!(validation_command_argv(Some(""), "cargo test").is_err());
        assert!(validation_command_argv(Some("/nonexistent-shell"), "cargo test").is_err());
    }

    fn row(status: TaskStatus, needs_attention: bool) -> TaskRowSnapshot {
        TaskRowSnapshot {
            id: TaskId::new(),
            title: "task".to_string(),
            provider: AgentProvider::Codex,
            status,
            runtime_kind: crate::agent_task::TaskRuntimeKind::Unassigned,
            branch: "anvil/task".to_string(),
            has_agent_terminal: false,
            has_validation_terminal: false,
            has_active_agent_stream: false,
            native_preparing: false,
            validation_status: TaskValidationStatus::NotRun,
            validation_attempt: 0,
            needs_attention,
            status_detail: None,
            validation_detail: None,
        }
    }

    #[test]
    fn dashboard_orders_attention_then_running_then_finished() {
        let waiting = row(TaskStatus::WaitingForApproval, true);
        let mut running = row(TaskStatus::Working, false);
        running.has_active_agent_stream = true;
        let done = row(TaskStatus::Completed, false);
        let done_id = done.id;
        let mut rows = vec![done, running, waiting];
        order_task_rows(&mut rows, |_| 0);
        assert_eq!(rows[2].id, done_id, "finished sorts last");
        assert!(rows[0].needs_attention, "attention sorts first");
    }

    #[test]
    fn status_line_joins_task_and_validation_state() {
        let mut row = row(TaskStatus::ReadyForReview, false);
        assert_eq!(row_status_line(&row), "Ready for review");
        row.validation_status = TaskValidationStatus::Passed;
        assert_eq!(
            row_status_line(&row),
            "Ready for review · validation Passed"
        );
        row.native_preparing = true;
        assert!(row_status_line(&row).starts_with("preparing native Codex"));
    }

    #[test]
    fn list_refresh_plan_is_a_noop_for_an_identical_push() {
        let rows = vec![
            row(TaskStatus::Created, false),
            row(TaskStatus::Working, false),
        ];
        let plan = plan_list_refresh(&rows, Some(rows[1].id), &rows, Some(rows[1].id));
        assert_eq!(
            plan,
            ListRefreshPlan {
                update_rows: false,
                reconcile_order: false,
                apply_selection: false,
                select_index: Some(1),
            },
            "an unchanged Sync must leave the list widget untouched",
        );
    }

    #[test]
    fn list_refresh_plan_preserves_selection_after_a_row_change() {
        let old_rows = vec![row(TaskStatus::Created, false)];
        let selected = old_rows[0].id;
        let mut new_rows = old_rows.clone();
        new_rows[0].status = TaskStatus::Working;
        let plan = plan_list_refresh(&old_rows, Some(selected), &new_rows, Some(selected));
        assert!(plan.update_rows);
        assert!(!plan.apply_selection, "existing rows keep selection");
        assert!(!plan.reconcile_order);
        assert_eq!(plan.select_index, Some(0));
    }

    #[test]
    fn list_refresh_plan_tracks_selection_changes_without_rebuilding() {
        let rows = vec![
            row(TaskStatus::Created, false),
            row(TaskStatus::Completed, false),
        ];
        let plan = plan_list_refresh(&rows, None, &rows, Some(rows[1].id));
        assert!(!plan.update_rows);
        assert!(plan.apply_selection);
        assert_eq!(plan.select_index, Some(1));

        let plan = plan_list_refresh(&rows, Some(rows[1].id), &rows, None);
        assert!(!plan.update_rows);
        assert!(plan.apply_selection, "clearing the selection is a change");
        assert_eq!(plan.select_index, None);
    }

    #[test]
    fn list_refresh_plan_drops_selection_for_a_task_without_a_row() {
        let rows = vec![row(TaskStatus::Created, false)];
        let ghost = TaskId::new();
        let plan = plan_list_refresh(&rows, Some(ghost), &rows, Some(ghost));
        assert!(!plan.update_rows);
        assert!(!plan.apply_selection, "no row can show it, nothing to do");
        assert_eq!(plan.select_index, None);

        let new_rows = vec![row(TaskStatus::Created, false)];
        let plan = plan_list_refresh(&rows, Some(ghost), &new_rows, Some(ghost));
        assert!(plan.update_rows);
        assert!(plan.apply_selection);
        assert!(plan.reconcile_order);
        assert_eq!(plan.select_index, None);
    }
    #[test]
    fn keyed_plan_distinguishes_content_order_and_removed_selection() {
        for count in [100, 1000] {
            let rows: Vec<_> = (0..count)
                .map(|_| row(TaskStatus::Working, false))
                .collect();
            let selected = rows[count / 2].id;
            let mut changed = rows.clone();
            changed[3].title = "same title, distinct identity".into();
            changed[7].status = TaskStatus::Failed;
            let plan = plan_list_refresh(&rows, Some(selected), &changed, Some(selected));
            assert!(plan.update_rows);
            assert!(!plan.reconcile_order);
            assert!(!plan.apply_selection);
            changed.rotate_right(1);
            let plan = plan_list_refresh(&rows, Some(selected), &changed, Some(selected));
            assert!(plan.reconcile_order && plan.apply_selection);
            assert_eq!(changed[plan.select_index.unwrap()].id, selected);
            changed.retain(|row| row.id != selected);
            let plan = plan_list_refresh(&rows, Some(selected), &changed, Some(selected));
            assert!(plan.reconcile_order && plan.apply_selection);
            assert_eq!(plan.select_index, None, "never select a substitute task");
        }
    }

    fn stream_snapshot() -> crate::agent_task::CodexAppServerViewSnapshot {
        crate::agent_task::CodexAppServerViewSnapshot::default()
    }

    fn stream_history(ordinal: usize) -> crate::agent_task::CodexAppServerTurnHistory {
        crate::agent_task::CodexAppServerTurnHistory {
            ordinal,
            local_turn_id: crate::agent_task::AgentTurnId::new(),
            follow_up_feedback: Some("please\ncontinue".to_string()),
            agent_text: String::new(),
            agent_text_truncated: false,
            commands: Vec::new(),
            file_changes: Vec::new(),
            dropped_updates: 0,
        }
    }

    #[test]
    fn stream_render_empty_snapshot_is_just_phase() {
        assert_eq!(render_stream_text(&stream_snapshot()), "phase: created");
    }

    #[test]
    fn stream_render_preserves_structural_lines_and_tabs() {
        let mut snapshot = stream_snapshot();
        snapshot.last_error = Some("first\nsecond".to_string());
        snapshot.agent_text = "hello\n\tworld".to_string();
        snapshot.displayed_follow_up_feedback = Some("adjust\nthis".to_string());
        snapshot.turn_history = vec![stream_history(1)].into();
        snapshot
            .commands
            .push(crate::agent_task::CodexAppServerCommandView {
                item_id: "command".to_string(),
                command: "echo hello".to_string(),
                cwd: "/tmp".to_string(),
                status: "completed".to_string(),
                output: "one\ntwo\tthree".to_string(),
                output_truncated: false,
            });
        let rendered = render_stream_text(&snapshot);
        assert!(rendered.contains("\nerror: first\nsecond"), "{rendered:?}");
        assert!(render_stream_history(&snapshot, None).contains("\n> please\ncontinue"));
        assert!(rendered.contains("\n> adjust\nthis"));
        assert!(rendered.contains("hello\n\tworld"));
        assert!(rendered.contains("\n$ echo hello [completed]\none\ntwo\tthree"));
    }

    #[test]
    fn stream_render_neutralizes_controls_and_visual_spoofing() {
        let mut snapshot = stream_snapshot();
        snapshot.agent_text = "left\u{1b}\u{202e}\u{200b}\u{00a0}\rright".to_string();
        let rendered = render_stream_text(&snapshot);
        assert!(rendered.contains("left"));
        assert!(rendered.contains("right"));
        for unsafe_char in ['\u{1b}', '\u{202e}', '\u{200b}', '\u{00a0}', '\r'] {
            assert!(!rendered.contains(unsafe_char));
        }
        assert!(rendered.len() <= MAX_NATIVE_STREAM_DISPLAY_BYTES);
    }

    #[test]
    fn stream_render_bounds_full_error_plus_history_headings_and_suffixes() {
        let mut snapshot = stream_snapshot();
        snapshot.last_error = Some("x".repeat(MAX_NATIVE_STREAM_DISPLAY_BYTES));
        snapshot.turn_history = (1..=8)
            .map(|ordinal| {
                let mut turn = stream_history(ordinal);
                turn.agent_text = "body".to_string();
                turn.agent_text_truncated = true;
                turn.dropped_updates = 10;
                turn
            })
            .collect::<Vec<_>>()
            .into();
        snapshot.dropped_turns = 2;
        snapshot.agent_text = "latest".to_string();
        snapshot.agent_text_truncated = true;
        let rendered = render_stream_text(&snapshot);
        assert!(
            rendered.len() <= MAX_NATIVE_STREAM_DISPLAY_BYTES,
            "{} bytes",
            rendered.len()
        );
        assert!(rendered.contains("display truncated"));
    }

    #[test]
    fn stream_render_bounds_unicode_and_expanding_controls_at_every_edge() {
        for scalar in ['界', '🙂', '\u{1b}', '\u{202e}'] {
            for edge in 0..=7 {
                let mut snapshot = stream_snapshot();
                snapshot.agent_text = format!(
                    "{}{}",
                    "x".repeat(MAX_NATIVE_STREAM_DISPLAY_BYTES - 32 + edge),
                    scalar.to_string().repeat(40)
                );
                let rendered = render_stream_text(&snapshot);
                assert!(
                    rendered.len() <= MAX_NATIVE_STREAM_DISPLAY_BYTES,
                    "{scalar:?}/{edge}: {} bytes",
                    rendered.len()
                );
                assert!(rendered.contains("display truncated"));
                assert!(rendered.is_char_boundary(rendered.len()));
            }
        }
    }

    #[test]
    fn stream_pages_reserve_current_and_error_budgets() {
        let mut snapshot = stream_snapshot();
        snapshot.displayed_turn_ordinal = Some(101);
        snapshot.last_error = Some(format!("latest error {}", "x".repeat(100_000)));
        snapshot.agent_text = "CURRENT PROGRESS".into();
        snapshot.turn_history = (1..=100)
            .map(|ordinal| {
                let mut turn = stream_history(ordinal);
                turn.agent_text = format!("history-{ordinal} {}", "界".repeat(30_000));
                turn
            })
            .collect::<Vec<_>>()
            .into();
        let current = render_stream_text(&snapshot);
        assert!(current.contains("phase: created"));
        assert!(current.contains("latest error"));
        assert!(current.contains("turn 101 (latest)"));
        assert!(current.contains("CURRENT PROGRESS"));
        for ordinal in 1..=100 {
            let history = render_stream_history(&snapshot, Some(ordinal));
            assert!(history.contains(&format!("history-{ordinal} ")));
            assert!(history.contains("item display truncated"));
            assert!(
                render_stream_status(&snapshot).len() + current.len() + history.len()
                    <= MAX_NATIVE_STREAM_DISPLAY_BYTES
            );
        }
    }

    #[test]
    fn stream_combined_budget_includes_error_metadata_and_notices() {
        let mut snapshot = stream_snapshot();
        snapshot.dropped_updates = 19;
        let huge = "界\u{1b}\u{202e}".repeat(40_000);
        snapshot.last_error = Some(huge.clone());
        snapshot.displayed_follow_up_feedback = Some(huge.clone());
        snapshot.displayed_turn_ordinal = Some(2);
        snapshot.agent_text = huge.clone();
        let mut turn = stream_history(1);
        turn.dropped_updates = 23;
        turn.agent_text = huge.clone();
        for _ in 0..20 {
            snapshot
                .commands
                .push(crate::agent_task::CodexAppServerCommandView {
                    item_id: String::new(),
                    command: huge.clone(),
                    cwd: String::new(),
                    status: huge.clone(),
                    output: huge.clone(),
                    output_truncated: true,
                });
            turn.commands
                .push(crate::agent_task::CodexAppServerTurnCommandSummary {
                    command: huge.clone(),
                    status: huge.clone(),
                    output_omitted: true,
                });
        }
        snapshot.turn_history = vec![turn].into();
        snapshot.dropped_turns = usize::MAX;
        let current = render_stream_text(&snapshot);
        let history = render_stream_history(&snapshot, None);
        assert!(current.contains("turn 2 (latest)"));
        assert!(current.starts_with("phase: created"));
        assert!(current.contains("\nerror: "));
        assert!(current.ends_with("(stream display truncated)"));
        assert!(history.ends_with("(stream display truncated)"));
        assert!(
            render_stream_status(&snapshot).len() + current.len() + history.len()
                <= MAX_NATIVE_STREAM_DISPLAY_BYTES
        );
        assert!(history.contains("retained-history budget"));
        assert!(current.contains("19 updates lost"));
        assert!(history.contains("23 updates lost"));
        for unsafe_char in ['\u{1b}', '\u{202e}'] {
            assert!(!current.contains(unsafe_char) && !history.contains(unsafe_char));
        }
    }

    #[test]
    fn stream_live_dropped_updates_always_have_a_retention_notice() {
        let mut snapshot = stream_snapshot();
        snapshot.dropped_updates = 19;
        assert!(render_stream_text(&snapshot)
            .contains("19 updates lost to the live-turn retention budget"));
    }

    #[test]
    fn stream_saturated_history_cannot_hide_dropped_updates() {
        let mut snapshot = stream_snapshot();
        let mut turn = stream_history(1);
        turn.dropped_updates = 19;
        turn.commands = (0..20)
            .map(|_| crate::agent_task::CodexAppServerTurnCommandSummary {
                command: "x".repeat(MAX_NATIVE_ITEM_DISPLAY_BYTES),
                status: "done".into(),
                output_omitted: true,
            })
            .collect();
        snapshot.turn_history = vec![turn].into();
        let page = render_stream_history(&snapshot, None);
        assert!(page.ends_with("(stream display truncated)"));
        assert!(page.contains("19 updates lost to the live-turn retention budget"));
    }

    #[test]
    fn stream_retention_loss_survives_saturated_panes_and_status_is_bounded() {
        let mut snapshot = stream_snapshot();
        snapshot.phase = crate::agent_task::CodexAppServerPhase::Running;
        snapshot.displayed_turn_ordinal = Some(99);
        snapshot.last_error = Some("LATEST\n\u{1b}\u{202e}界".repeat(10000));
        snapshot.agent_text = "x".repeat(100000);
        snapshot.agent_text_truncated = true;
        snapshot.dropped_updates = 123;
        let mut turn = stream_history(98);
        turn.agent_text = snapshot.agent_text.clone();
        turn.dropped_updates = 456;
        turn.agent_text_truncated = true;
        snapshot.turn_history = vec![turn].into();
        let current = render_stream_text(&snapshot);
        let history = render_stream_history(&snapshot, None);
        assert!(current.contains("123 updates lost to the live-turn retention budget"));
        assert!(history.contains("456 updates lost to the live-turn retention budget"));
        assert!(current.contains("omitted by retention budget"));
        assert!(history.contains("omitted by retention budget"));
        assert!(
            current.contains("item display truncated")
                && history.contains("item display truncated")
        );
        let status = render_stream_status(&snapshot);
        assert!(status.starts_with("phase: running · turn 99"));
        assert!(status.contains("Latest error (preview): LATEST"));
        assert!(status.len() <= MAX_NATIVE_STREAM_STATUS_BYTES);
        assert_eq!(status.matches('\n').count(), 1);
        assert!(!status.contains('\u{1b}') && !status.contains('\u{202e}'));
        assert!(status.len() + current.len() + history.len() <= MAX_NATIVE_STREAM_DISPLAY_BYTES);
    }

    #[test]
    fn stream_history_cursor_survives_append_and_eviction() {
        let mut snapshot = stream_snapshot();
        assert_eq!(stream_history_index(&snapshot, None), None);
        snapshot.turn_history = (3..=6).map(stream_history).collect::<Vec<_>>().into();
        assert_eq!(stream_history_index(&snapshot, None), Some(3));
        assert_eq!(stream_history_index(&snapshot, Some(4)), Some(1));
        snapshot.turn_history = (4..=7).map(stream_history).collect::<Vec<_>>().into();
        assert_eq!(stream_history_index(&snapshot, Some(4)), Some(0));
        assert_eq!(stream_history_index(&snapshot, Some(3)), Some(0));
        snapshot.dropped_turns = 3;
        let history = render_stream_history(&snapshot, Some(3));
        assert!(history.contains("3 oldest turns elided by the retained-history budget"));
        assert!(history.contains("turn 4"));
        assert!(!history.contains("display truncated"));
    }

    #[test]
    fn stream_history_page_neutralizes_controls_and_bounds_expansion() {
        let mut snapshot = stream_snapshot();
        let mut turn = stream_history(1);
        turn.agent_text = "left\u{1b}\u{202e}\u{200b}\u{00a0}\rright".repeat(9000);
        snapshot.turn_history = vec![turn].into();
        let history = render_stream_history(&snapshot, None);
        assert!(history.contains("left"));
        for unsafe_char in ['\u{1b}', '\u{202e}', '\u{200b}', '\u{00a0}', '\r'] {
            assert!(!history.contains(unsafe_char));
        }
        assert!(history.contains("item display truncated"));
        assert!(history.len() <= MAX_NATIVE_STREAM_DISPLAY_BYTES / 2);
    }

    #[test]
    fn stream_empty_current_turn_keeps_its_heading() {
        let mut snapshot = stream_snapshot();
        snapshot.displayed_turn_ordinal = Some(7);
        assert!(render_stream_text(&snapshot).contains("turn 7 (latest)"));
    }

    #[test]
    fn stream_render_item_budget_preserves_multiline_output() {
        let mut snapshot = stream_snapshot();
        snapshot
            .commands
            .push(crate::agent_task::CodexAppServerCommandView {
                item_id: "command".to_string(),
                command: "echo".to_string(),
                cwd: "/tmp".to_string(),
                status: "done".to_string(),
                output: format!("first\n{}", "界".repeat(MAX_NATIVE_ITEM_DISPLAY_BYTES)),
                output_truncated: true,
            });
        let rendered = render_stream_text(&snapshot);
        assert!(rendered.contains("\nfirst\n"));
        assert!(rendered.len() < MAX_NATIVE_ITEM_DISPLAY_BYTES + 256);
        assert!(rendered.contains("item display truncated"));
        assert!(rendered.contains("retention budget"));
    }
}

/// A bounded, redacted title and a stable ID prefix; never use prompt/output text.
pub(crate) fn feedback_identity(manager: &crate::agent_task::TaskManager, id: TaskId) -> String {
    let full = id.to_string();
    let peers: Vec<_> = manager
        .tasks()
        .iter()
        .filter(|task| task.id != id)
        .map(|task| task.id.to_string())
        .collect();
    let length = feedback_id_prefix_length(&full, &peers);
    let title = manager
        .get(id)
        .map(|task| feedback_title(&task.title))
        .unwrap_or_else(|| "Task no longer available".into());
    format!("{title} [{}]", &full[..length])
}

fn feedback_id_prefix_length(full: &str, peers: &[String]) -> usize {
    let mut length = 8;
    while length < full.len() && peers.iter().any(|peer| peer.starts_with(&full[..length])) {
        length = (length + 4).min(full.len());
    }
    length
}

fn feedback_title(title: &str) -> String {
    // Redact before bounding so a secret cannot evade detection at the cut point.
    let redacted = jterm_core::redact::redact_secrets(title);
    visible_bounded(
        &redacted.split_whitespace().collect::<Vec<_>>().join(" "),
        64,
    )
}

#[cfg(test)]
pub(crate) fn task_completion_feedback(
    manager: &crate::agent_task::TaskManager,
    completions: impl IntoIterator<Item = (TaskId, crate::agent_task::AgentSessionOutcome)>,
) -> Option<String> {
    use crate::agent_task::AgentSessionOutcome;
    let completions: Vec<_> = completions.into_iter().collect();
    let (id, outcome) = *completions.first()?;
    if completions.len() > 1 {
        let failed = completions
            .iter()
            .filter(|(_, outcome)| *outcome == AgentSessionOutcome::Failed)
            .count();
        let cancelled = completions
            .iter()
            .filter(|(_, outcome)| *outcome == AgentSessionOutcome::Cancelled)
            .count();
        return Some(format!("{} agent sessions stopped: {} clean, {cancelled} cancelled, {failed} failed; open Tasks for details", completions.len(), completions.len() - failed - cancelled));
    }
    let outcome = match outcome {
        AgentSessionOutcome::Clean => {
            "Agent session stopped cleanly; review changes, then validate"
        }
        AgentSessionOutcome::Cancelled => "Agent session cancelled and stopped",
        AgentSessionOutcome::Failed => "Agent session failed; open Tasks for details",
    };
    Some(format!("{}: {outcome}", feedback_identity(manager, id)))
}

#[cfg(test)]
pub(crate) fn task_validation_feedback(
    manager: &crate::agent_task::TaskManager,
    id: TaskId,
    session: &str,
) -> Option<String> {
    use crate::agent_task::TaskValidationStatus;
    let task = manager.get(id)?;
    let validation = &task.validation;
    if validation.terminal_session_id.as_deref() != Some(session) {
        return None;
    }
    let outcome = match validation.status {
        TaskValidationStatus::NotRun | TaskValidationStatus::Running => return None,
        TaskValidationStatus::Passed => "passed".into(),
        TaskValidationStatus::Failed => validation
            .exit_code
            .map(|code| format!("failed (exit {code})"))
            .unwrap_or_else(|| "failed; open Tasks for details".into()),
        TaskValidationStatus::Cancelled => "cancelled".into(),
        TaskValidationStatus::Inconclusive => {
            "ended without a confirmed result; open Tasks for details".into()
        }
    };
    Some(format!(
        "{}: Validation #{} {outcome}",
        feedback_identity(manager, id),
        validation.attempt
    ))
}

/// Report-only runtime errors are not necessarily retained in TaskManager.
/// Keep their redacted explanation in visible, accessible feedback. The sole
/// explicit announcement uses only a fixed safe summary. Merge per task with a result from the same poll, avoiding an
/// Alert toast plus an explicit announcement for the same event.
pub(crate) fn merge_task_issue_feedback(
    messages: &mut Vec<(TaskId, String)>,
    issues: &[crate::agent_task::AgentRuntimeIssue],
) -> std::collections::HashMap<TaskId, String> {
    let mut details: std::collections::HashMap<TaskId, Vec<String>> =
        std::collections::HashMap::new();
    let mut order = Vec::new();
    for issue in issues {
        let redacted = jterm_core::redact::redact_secrets(&issue.detail);
        let detail = visible_bounded(
            &redacted.split_whitespace().collect::<Vec<_>>().join(" "),
            256,
        );
        if !details.contains_key(&issue.task_id) {
            order.push(issue.task_id);
        }
        let task_details = details.entry(issue.task_id).or_default();
        if !task_details.contains(&detail) {
            task_details.push(detail);
        }
    }
    let mut accessible_details = std::collections::HashMap::new();
    for id in order {
        let detail = visible_bounded(&details.remove(&id).unwrap().join("; "), 512);
        accessible_details.insert(id, detail);
        let issue = "Native Agent runtime issue; details in Tasks";
        if let Some((_, message)) = messages.iter_mut().find(|(task_id, _)| *task_id == id) {
            message.push_str("; ");
            message.push_str(issue);
        } else {
            messages.push((id, issue.into()));
        }
    }
    accessible_details
}

/// Single production delivery path, also exercised by the isolated AT-SPI
/// fixture. Detail is visible/accessibility-readable but never announced.
pub(crate) fn deliver_task_feedback(
    window: &relm4::gtk::Widget,
    label: &relm4::gtk::Label,
    message: &str,
    detail: Option<&str>,
) {
    use relm4::gtk::prelude::AccessibleExt;
    let visible = detail.map_or_else(
        || message.to_string(),
        |detail| format!("{message}. {detail}"),
    );
    show_task_feedback(label, &visible);
    window.announce(message, relm4::gtk::AccessibleAnnouncementPriority::Medium);
}

/// Passive visible feedback with the natural static-text accessible role.
/// AdwToastWidget has Alert semantics, so task result delivery must not use it
/// alongside explicit polite window announcements (two speech owners).
pub(crate) fn build_task_feedback() -> relm4::gtk::Label {
    use relm4::gtk::{self, prelude::*};
    let label = gtk::Label::new(None);
    label.set_use_markup(false);
    label.set_focusable(false);
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_lines(2);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_margin_start(12);
    label.set_margin_end(12);
    label.set_margin_top(4);
    label.set_margin_bottom(4);
    label.set_visible(false);
    label
}

/// Only visible text changes here; the application owns semantic announcements.
/// No task selection, pane activation, focus, or accessible role is modified.
pub(crate) fn show_task_feedback(label: &relm4::gtk::Label, message: &str) {
    use relm4::gtk::prelude::*;
    label.set_text(message);
    label.set_visible(true);
}

#[cfg(test)]
mod feedback_tests {
    use super::*;
    use crate::agent_task::{AgentSessionOutcome, NewTask, TaskManager, TaskStatus};

    fn add_task(manager: &mut TaskManager, title: &str) -> TaskId {
        manager
            .create(NewTask {
                title: title.into(),
                provider: AgentProvider::Codex,
                repo_root: "/fixture".into(),
                worktree_path: "/fixture/worktree".into(),
                branch: "test/feedback".into(),
                base_commit: "a".repeat(40),
                source_context: None,
            })
            .unwrap()
    }

    #[test]
    fn completion_feedback_identifies_same_title_tasks_and_missing_task() {
        let mut manager = TaskManager::new();
        let a = add_task(&mut manager, "Same title");
        let b = add_task(&mut manager, "Same title");
        let message =
            |id| task_completion_feedback(&manager, [(id, AgentSessionOutcome::Failed)]).unwrap();
        assert_ne!(message(a), message(b));
        assert!(message(a).contains(&a.to_string()[..8]));
        assert!(message(b).contains(&b.to_string()[..8]));
        let missing = TaskId::new();
        assert!(message(missing).contains("Task no longer available"));
        assert!(message(missing).contains(&missing.to_string()[..8]));
        assert!(task_completion_feedback(&manager, []).is_none());
        let batch = task_completion_feedback(
            &manager,
            [
                (a, AgentSessionOutcome::Clean),
                (b, AgentSessionOutcome::Cancelled),
                (a, AgentSessionOutcome::Failed),
            ],
        )
        .unwrap();
        assert_eq!(
            batch,
            "3 agent sessions stopped: 1 clean, 1 cancelled, 1 failed; open Tasks for details"
        );
        assert!(!batch.contains("3 tasks"));
    }

    #[test]
    fn feedback_id_prefix_extends_for_live_collisions() {
        let a = "12345678-1111-4111-8111-111111111111";
        let b = "12345678-2222-4222-8222-222222222222";
        assert_eq!(feedback_id_prefix_length(a, &[]), 8);
        let length = feedback_id_prefix_length(a, &[b.into()]);
        assert_eq!(length, 12);
        assert_ne!(&a[..length], &b[..length]);
    }

    #[test]
    fn feedback_title_is_redacted_single_line_and_utf8_bounded() {
        let secret = format!("sk-{}", "a".repeat(32));
        assert!(!feedback_title(&format!("Fix {secret}")).contains(&secret));
        let hostile = feedback_title("one\n\t\u{202e}two\u{2066}\u{1b}[31m");
        assert!(!hostile.chars().any(char::is_control));
        assert!(!hostile.contains('\u{202e}'));
        assert!(!hostile.contains('\u{2066}'));
        assert!(hostile.contains("one "));
        let long = feedback_title(&"界🙂".repeat(80));
        assert!(long.len() <= 64);
        assert!(long.is_char_boundary(long.len()));
        assert!(long.ends_with('…'));
    }

    #[test]
    fn validation_feedback_uses_bound_attempt_and_stored_result() {
        let mut manager = TaskManager::new();
        let a = add_task(&mut manager, "Same title");
        let b = add_task(&mut manager, "Same title");
        for (id, session) in [(a, "agent-a"), (b, "agent-b")] {
            manager.bind_terminal_session(id, session.into()).unwrap();
            manager
                .update_status(id, TaskStatus::ReadyForReview, None)
                .unwrap();
        }
        manager
            .bind_validation_session(a, "validation-a".into())
            .unwrap();
        manager
            .bind_validation_session(b, "validation-b".into())
            .unwrap();
        assert!(task_validation_feedback(&manager, a, "validation-a").is_none());
        manager.handle_terminal_session_exit("validation-a", Some(7));
        assert_eq!(
            manager.handle_terminal_session_closed("validation-a"),
            Some(a)
        );
        let failed = task_validation_feedback(&manager, a, "validation-a").unwrap();
        assert!(failed.contains(&a.to_string()[..8]));
        assert!(failed.ends_with("Validation #1 failed (exit 7)"));
        assert!(task_validation_feedback(&manager, b, "validation-a").is_none());
        manager
            .bind_validation_session(a, "validation-a-retry".into())
            .unwrap();
        assert!(manager
            .handle_terminal_session_exit("validation-a", Some(0))
            .is_none());
        assert!(task_validation_feedback(&manager, a, "validation-a").is_none());
        assert!(task_validation_feedback(&manager, a, "validation-a-retry").is_none());
        manager.handle_terminal_session_exit("validation-a-retry", Some(0));
        assert_eq!(
            manager.handle_terminal_session_closed("validation-a-retry"),
            Some(a)
        );
        assert!(task_validation_feedback(&manager, b, "validation-b").is_none());
        assert!(task_validation_feedback(&manager, a, "validation-a-retry")
            .unwrap()
            .ends_with("Validation #2 passed"));
        assert!(task_validation_feedback(&TaskManager::new(), a, "validation-a-retry").is_none());
    }

    #[test]
    #[ignore = "requires an isolated GTK process and display"]
    fn task_feedback_keeps_input_draft_and_focus() {
        use adw::prelude::*;
        use relm4::adw;
        use relm4::gtk;
        use relm4::gtk::glib;

        adw::init().expect("GTK display");
        let overlay = adw::ToastOverlay::new();
        let input = gtk::Entry::new();
        input.set_text("unfinished input");
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let feedback = build_task_feedback();
        content.append(&input);
        content.append(&feedback);
        overlay.set_child(Some(&content));
        let window = gtk::Window::new();
        window.set_default_size(640, 240);
        window.set_child(Some(&overlay));
        window.present();
        let settle = || {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(150);
            while std::time::Instant::now() < until {
                while glib::MainContext::default().iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        settle();
        assert!(input.grab_focus());
        let owner = gtk::prelude::GtkWindowExt::focus(&window).expect("input focus owner");
        let mut manager = TaskManager::new();
        let a = add_task(&mut manager, "<b>Same title</b>");
        let b = add_task(&mut manager, "<b>Same title</b>");
        fn displays(widget: &gtk::Widget, text: &str) -> bool {
            if let Some(label) = widget.downcast_ref::<gtk::Label>() {
                if label.is_mapped() && label.text() == text {
                    return true;
                }
            }
            let mut child = widget.first_child();
            while let Some(current) = child {
                if displays(&current, text) {
                    return true;
                }
                child = current.next_sibling();
            }
            false
        }
        for id in [a, b] {
            let message =
                task_completion_feedback(&manager, [(id, AgentSessionOutcome::Failed)]).unwrap();
            show_task_feedback(&feedback, &message);
            settle();
            assert_eq!(
                gtk::prelude::GtkWindowExt::focus(&window),
                Some(owner.clone())
            );
            assert_eq!(input.text(), "unfinished input");
            if id == a {
                assert!(
                    displays(overlay.upcast_ref(), &message),
                    "the passive task feedback is rendered"
                );
            }
        }
        assert!(overlay.is_mapped());
        window.close();
    }
}

#[cfg(test)]
mod issue_feedback_tests {
    use super::*;
    use crate::agent_task::AgentRuntimeIssue;

    #[test]
    fn report_only_issue_has_fixed_announcement_and_accessible_safe_detail() {
        let id = TaskId::new();
        let secret = format!("sk-{}", "a".repeat(32));
        let issue = AgentRuntimeIssue {
            task_id: id,
            detail: format!("Preparation failed for {secret}\ncheck configuration"),
        };
        let mut messages = Vec::new();
        let details = merge_task_issue_feedback(&mut messages, &[issue.clone(), issue]);
        assert_eq!(
            messages,
            vec![(id, "Native Agent runtime issue; details in Tasks".into())]
        );
        assert!(details[&id].contains("Preparation failed"));
        assert!(!details[&id].contains(&secret));
        assert!(!details[&id].contains('\n'));
        assert!(!messages[0].1.contains("Preparation failed"));
    }

    #[test]
    fn issue_and_terminal_transition_share_one_owner_without_erasing_other_tasks() {
        let a = TaskId::new();
        let b = TaskId::new();
        let mut messages = vec![(a, "Task failed".into())];
        let details = merge_task_issue_feedback(
            &mut messages,
            &[
                AgentRuntimeIssue {
                    task_id: a,
                    detail: "transport stopped".into(),
                },
                AgentRuntimeIssue {
                    task_id: b,
                    detail: "preparation policy changed".into(),
                },
            ],
        );
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0].1,
            "Task failed; Native Agent runtime issue; details in Tasks"
        );
        assert_eq!(details[&a], "transport stopped");
        assert_eq!(details[&b], "preparation policy changed");
        assert!(merge_task_issue_feedback(&mut Vec::new(), &[]).is_empty());
    }
}
