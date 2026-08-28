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
    StartCodex(TaskId),
    StartTerminal(TaskId),
    StopCodex(TaskId),
    FollowUp(TaskId, String),
    FinishCodex(TaskId),
    Approve(TaskId, crate::agent_task::ApprovalId),
    Deny(TaskId, crate::agent_task::ApprovalId),
    RunValidation(TaskId),
    Complete(TaskId),
    ReviewDiff(TaskId),
    Archive(TaskId),
    Select(TaskId),
    Close,
}

/// Fully prepared task registration produced by the background worktree
/// worker. The UI thread only registers it with the task manager.
pub(crate) struct PreparedTask {
    pub(crate) context: SemanticCommandContext,
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
    let worktree_root = dirs::data_local_dir()
        .ok_or_else(|| "cannot locate the per-user data directory".to_string())?
        .join("anvil")
        .join("agent-tasks");
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
    let token = uuid::Uuid::new_v4().simple().to_string();
    let task_name = format!("task-{token}");
    let branch = format!("anvil/{task_name}");
    let (sender, receiver) = mpsc::sync_channel(1);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let worker = std::thread::Builder::new()
        .name("anvil-task-worktree".to_string())
        .spawn(move || {
            let result = (|| {
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
                    context,
                    title,
                    provider,
                    worktree,
                })
            })();
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

#[cfg(test)]
mod tests {
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
}

/// Display bounds for the native stream text projection.
pub(crate) const MAX_NATIVE_STREAM_DISPLAY_BYTES: usize = 64 * 1024;
const MAX_NATIVE_ITEM_DISPLAY_BYTES: usize = 8 * 1024;

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

fn push_bounded(out: &mut String, text: &str) {
    let remaining = MAX_NATIVE_STREAM_DISPLAY_BYTES.saturating_sub(out.len());
    if remaining == 0 {
        return;
    }
    let shown = crate::review_text::visible_bounded(text, remaining);
    out.push_str(&shown);
}

/// Render one native session snapshot as bounded plain text for the panel's
/// stream view. Provider-controlled strings are already display-safe and
/// bounded by the worker; this projection re-bounds the composed whole and
/// never treats any of it as markup.
pub(crate) fn render_stream_text(
    snapshot: &crate::agent_task::CodexAppServerViewSnapshot,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("phase: {}", phase_label(snapshot.phase)));
    if let Some(error) = &snapshot.last_error {
        push_bounded(&mut out, &format!("\nerror: {error}"));
    }
    if snapshot.dropped_turns > 0 {
        out.push_str(&format!(
            "\n({} oldest turns elided by the retained-history budget)",
            snapshot.dropped_turns
        ));
    }
    for turn in snapshot.turn_history.iter() {
        out.push_str(&format!("\n\n— turn {} —", turn.ordinal));
        if let Some(feedback) = &turn.follow_up_feedback {
            push_bounded(&mut out, &format!("\n> {feedback}"));
        }
        if !turn.agent_text.is_empty() {
            out.push('\n');
            push_bounded(&mut out, &turn.agent_text);
            if turn.agent_text_truncated {
                out.push_str(" …");
            }
        }
        for command in &turn.commands {
            push_bounded(
                &mut out,
                &format!(
                    "\n$ {} [{}]{}",
                    command.command,
                    command.status,
                    if command.output_omitted {
                        " (output omitted)"
                    } else {
                        ""
                    }
                ),
            );
        }
        for change in &turn.file_changes {
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
        if turn.dropped_updates > 0 {
            out.push_str(&format!(
                "\n({} updates dropped by the live-turn budget)",
                turn.dropped_updates
            ));
        }
    }
    let shows_live_projection = !snapshot.agent_text.is_empty()
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
            if snapshot.agent_text_truncated {
                out.push_str(" …");
            }
        }
        for command in &snapshot.commands {
            push_bounded(
                &mut out,
                &format!("\n$ {} [{}]", command.command, command.status),
            );
            if !command.output.is_empty() {
                out.push('\n');
                push_bounded(
                    &mut out,
                    &crate::review_text::visible_bounded(
                        &command.output,
                        MAX_NATIVE_ITEM_DISPLAY_BYTES,
                    ),
                );
                if command.output_truncated {
                    out.push_str(" …");
                }
            }
        }
        for change in &snapshot.file_changes {
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
                push_bounded(&mut out, &format!("\n  {} {}", file.kind, file.path));
            }
        }
    }
    if out.len() >= MAX_NATIVE_STREAM_DISPLAY_BYTES {
        out.push_str("\n(stream display truncated)");
    }
    out
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
