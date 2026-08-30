//! Owned semantic command context passed from terminal history to agent tasks.

use jterm_core::ai::BlockContext;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::SystemTime;

/// Compatibility budgets enforced by the pinned jagent prompt adapter.
/// Keeping them explicit here prevents it from silently eliding evidence
/// while Anvil still labels the attached command/context as complete.
pub const AGENT_BLOCK_COMMAND_PROMPT_BYTES: usize = 16 * 1024;
pub const AGENT_BLOCK_OUTPUT_PROMPT_BYTES: usize = 64 * 1024;
pub const AGENT_BLOCK_CWD_PROMPT_BYTES: usize = 4 * 1024;

/// The pinned compatibility context has no nullable exit status. Keep the
/// sentinel out of the semantic domain and explain it in the attached output
/// so the model cannot mistake it for a shell-reported process status.
pub const UNKNOWN_EXIT_STATUS_SENTINEL: i32 = -1;
pub const UNKNOWN_EXIT_STATUS_NOTE: &str =
    "[Anvil context: shell reported no exit status; -1 is a compatibility sentinel.]\n";

/// The exact preflight shared by the block menu and the Agent panel. A blank
/// untruncated command is a genuine background-output block; a missing command
/// whose producer set `command_truncated` is not silently reclassified.
/// `output_available == None` means a lightweight UI snapshot cannot know
/// whether the verified journal can recover an evicted live capture; the
/// backend always passes `Some` after authoritative live+journal resolution.
pub fn block_agent_context_disabled_reason(
    command: Option<&str>,
    command_exact: bool,
    command_truncated: bool,
    cwd: Option<&str>,
    output_available: Option<bool>,
) -> Option<&'static str> {
    let command = command.filter(|command| !command.trim().is_empty());
    if command_truncated {
        return Some("The shell omitted or truncated the command metadata");
    }
    let Some(command) = command else {
        return matches!(output_available, Some(false))
            .then_some("Captured block output is unavailable");
    };
    if !command_exact {
        return Some("Exact command metadata is required");
    }
    if command.len() > AGENT_BLOCK_COMMAND_PROMPT_BYTES {
        return Some("The exact command exceeds the Agent context limit");
    }
    let Some(cwd) = cwd.filter(|cwd| !cwd.trim().is_empty()) else {
        return Some("The command working directory is unavailable");
    };
    if cwd.len() > AGENT_BLOCK_CWD_PROMPT_BYTES {
        return Some("The command working directory exceeds the Agent context limit");
    }
    matches!(output_available, Some(false)).then_some("Captured block output is unavailable")
}

/// An owned snapshot of one semantic command execution.
///
/// The source identifiers remain stable when tabs and split panes are
/// reordered. Terminal buffer anchors deliberately do not appear here: a task
/// must keep its evidence after scrollback or the live
/// live command record has been evicted.
///
/// `command_exact` means the shell supplied the complete command as OSC 133
/// metadata. A display-reconstructed command can still be useful as untrusted
/// evidence, but must never authorize Retry. `command_truncated` means the
/// producer explicitly omitted or shortened the command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticCommandContext {
    pub source_session_id: String,
    pub source_execution_id: String,
    pub source_sequence: u64,

    /// Absolute shell executable selected for the source terminal. Validation
    /// refuses legacy snapshots without this identity rather than interpreting
    /// exact command text through a later hot-reloaded shell configuration.
    #[serde(default)]
    pub source_shell: Option<String>,

    pub command: Option<String>,
    pub command_exact: bool,
    pub command_truncated: bool,

    /// Directory in which the source command started.
    pub cwd: Option<String>,
    /// Directory reported after completion; may differ for a stateful `cd`.
    pub cwd_after: Option<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,

    /// Normalized rendered PTY text for the OSC 133 C..D range.
    pub output_text: String,
    /// False distinguishes an unavailable capture from genuine empty output.
    pub output_available: bool,
    pub output_truncated: bool,
    /// UTF-8 byte count before bounded head/tail capture.
    pub output_total_bytes: usize,

    pub started_at: Option<SystemTime>,
    pub finished_at: Option<SystemTime>,
}

/// Why a semantic snapshot cannot be represented by the current compatibility
/// agent prompt context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextError {
    MissingSourceSessionId,
    MissingSourceExecutionId,
    MissingCommand,
    CommandTruncated,
    CommandExceedsPromptBudget,
    CwdExceedsPromptBudget,
    OutputUnavailable,
}

impl fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingSourceSessionId => "semantic command context has no source session id",
            Self::MissingSourceExecutionId => "semantic command context has no source execution id",
            Self::MissingCommand => "semantic command context has no command text",
            Self::CommandTruncated => "semantic command context contains a truncated command",
            Self::CommandExceedsPromptBudget => {
                "semantic command exceeds the Agent prompt's exact-command budget"
            }
            Self::CwdExceedsPromptBudget => {
                "semantic command cwd exceeds the Agent prompt's workspace budget"
            }
            Self::OutputUnavailable => "semantic command context has no captured command output",
        })
    }
}

impl std::error::Error for ContextError {}

impl SemanticCommandContext {
    /// Convert to the compatibility context consumed by the current
    /// `jterm_core` agent prompt.
    ///
    /// This adapter is intentionally strict about unavailable evidence. In
    /// particular, an unreported exit status remains `None` in this semantic
    /// snapshot; only the compatibility value uses `-1`, accompanied by an
    /// explicit bounded note in its output. A failed output capture is never
    /// turned into genuine empty output. The compatibility type has no command
    /// provenance fields, so an explicitly truncated command is rejected
    /// rather than silently losing that fact. `command_exact == false` remains
    /// representable as untrusted evidence; callers must still require
    /// `command_exact` for Retry or any other execution-authorizing action.
    pub fn to_block_context(&self) -> Result<BlockContext, ContextError> {
        if self.source_session_id.is_empty() {
            return Err(ContextError::MissingSourceSessionId);
        }
        if self.source_execution_id.is_empty() {
            return Err(ContextError::MissingSourceExecutionId);
        }
        let command = self
            .command
            .as_ref()
            .filter(|command| !command.trim().is_empty())
            .ok_or(ContextError::MissingCommand)?;
        if self.command_truncated {
            return Err(ContextError::CommandTruncated);
        }
        if command.len() > AGENT_BLOCK_COMMAND_PROMPT_BYTES {
            return Err(ContextError::CommandExceedsPromptBudget);
        }
        if self
            .cwd
            .as_ref()
            .is_some_and(|cwd| cwd.len() > AGENT_BLOCK_CWD_PROMPT_BYTES)
        {
            return Err(ContextError::CwdExceedsPromptBudget);
        }
        if !self.output_available {
            return Err(ContextError::OutputUnavailable);
        }

        let (exit_code, output, compatibility_truncated) = match self.exit_code {
            Some(exit_code) => (exit_code, self.output_text.clone(), false),
            None => {
                let (output, clipped) = output_with_unknown_exit_note(&self.output_text);
                let source_budget =
                    AGENT_BLOCK_OUTPUT_PROMPT_BYTES.saturating_sub(UNKNOWN_EXIT_STATUS_NOTE.len());
                (
                    UNKNOWN_EXIT_STATUS_SENTINEL,
                    output,
                    clipped || self.output_total_bytes > source_budget,
                )
            }
        };

        Ok(BlockContext {
            cmd: command.clone(),
            output,
            cwd: self.cwd.clone(),
            exit_code,
            truncated: self.output_truncated
                || compatibility_truncated
                || self.output_text.len() > AGENT_BLOCK_OUTPUT_PROMPT_BYTES
                || self.output_total_bytes > AGENT_BLOCK_OUTPUT_PROMPT_BYTES,
        })
    }
}

/// Prefix the fixed unknown-status explanation while charging it to the
/// compatibility output budget. Retaining both ends mirrors jagent's prompt
/// elision and preserves late diagnostics without ever dropping the note.
fn output_with_unknown_exit_note(output: &str) -> (String, bool) {
    const ELISION_MARKER: &str = "\n\n… [bytes elided] …\n\n";

    let source_budget =
        AGENT_BLOCK_OUTPUT_PROMPT_BYTES.saturating_sub(UNKNOWN_EXIT_STATUS_NOTE.len());
    if output.len() <= source_budget {
        let mut bounded = String::with_capacity(UNKNOWN_EXIT_STATUS_NOTE.len() + output.len());
        bounded.push_str(UNKNOWN_EXIT_STATUS_NOTE);
        bounded.push_str(output);
        return (bounded, false);
    }

    let retained_budget = source_budget.saturating_sub(ELISION_MARKER.len());
    let head_budget = retained_budget / 2;
    let tail_budget = retained_budget.saturating_sub(head_budget);
    let mut head_end = head_budget.min(output.len());
    while head_end > 0 && !output.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = output.len().saturating_sub(tail_budget);
    while tail_start < output.len() && !output.is_char_boundary(tail_start) {
        tail_start += 1;
    }

    let mut bounded = String::with_capacity(AGENT_BLOCK_OUTPUT_PROMPT_BYTES);
    bounded.push_str(UNKNOWN_EXIT_STATUS_NOTE);
    bounded.push_str(&output[..head_end]);
    bounded.push_str(ELISION_MARKER);
    bounded.push_str(&output[tail_start..]);
    debug_assert!(bounded.len() <= AGENT_BLOCK_OUTPUT_PROMPT_BYTES);
    (bounded, true)
}

impl TryFrom<&SemanticCommandContext> for BlockContext {
    type Error = ContextError;

    fn try_from(context: &SemanticCommandContext) -> Result<Self, Self::Error> {
        context.to_block_context()
    }
}

/// 宽松适配器（与 ember/frost 家族共享词汇）：任何已完成块都能作为不可信
/// 证据附加到当前 Agent 对话，不像 [`SemanticCommandContext::to_block_context`]
/// 那样要求精确命令元数据或可用 cwd。输出生成两级有界：实时捕获本身受
/// `MAX_COMPLETED_COMMAND_OUTPUT_BYTES` 约束，这里再经
/// [`jterm_core::ai::truncate_for_context`] 只保留头/尾各行窗口（核心同时
/// 施加 64 KiB 字节预算）。未上报 exit status 时使用兼容性哨兵 -1 并在
/// 输出前加上解释性注记，绝不把 Unknown 变成伪成功；捕获不可用用固定占位
/// 文本并标记 truncated，不与真实空输出混淆。
pub fn ad_hoc_block_context(context: &SemanticCommandContext) -> BlockContext {
    /// 占位文本与 ember/frost 家族逐字保持一致（共享词汇）。
    const OUTPUT_UNAVAILABLE_NOTE: &str =
        "[output unavailable: retained snapshot and scrollback rows were evicted]";

    let (raw, capture_truncated) = if context.output_available {
        (context.output_text.as_str(), context.output_truncated)
    } else {
        (OUTPUT_UNAVAILABLE_NOTE, true)
    };
    let prepared = if context.exit_code.is_none() {
        format!("{UNKNOWN_EXIT_STATUS_NOTE}{raw}")
    } else {
        raw.to_string()
    };
    let bounded = jterm_core::ai::truncate_for_context(&prepared, 80);
    let elided = bounded != prepared;
    BlockContext {
        cmd: context.command.clone().unwrap_or_default(),
        output: bounded,
        cwd: context.cwd.clone(),
        exit_code: context.exit_code.unwrap_or(UNKNOWN_EXIT_STATUS_SENTINEL),
        truncated: context.command_truncated || capture_truncated || elided,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn context() -> SemanticCommandContext {
        SemanticCommandContext {
            source_session_id: "session-stable".to_string(),
            source_execution_id: "execution-stable".to_string(),
            // Sequence zero is the first valid local record and must not be
            // mistaken for a missing identity.
            source_sequence: 0,
            source_shell: Some("/bin/bash".to_string()),
            command: Some("cargo test".to_string()),
            command_exact: true,
            command_truncated: false,
            cwd: Some("/workspace/anvil".to_string()),
            cwd_after: Some("/workspace/anvil".to_string()),
            exit_code: Some(101),
            duration_ms: Some(8420),
            output_text: "failures:\n    terminal::test".to_string(),
            output_available: true,
            output_truncated: true,
            output_total_bytes: 300_000,
            started_at: Some(UNIX_EPOCH + Duration::from_secs(10)),
            finished_at: Some(UNIX_EPOCH + Duration::from_secs(18)),
        }
    }

    #[test]
    fn conversion_preserves_prompt_fields_without_mutating_domain_provenance() {
        let semantic = context();
        let block = semantic.to_block_context().expect("complete context");

        assert_eq!(block.cmd, "cargo test");
        assert_eq!(block.output, "failures:\n    terminal::test");
        assert_eq!(block.cwd.as_deref(), Some("/workspace/anvil"));
        assert_eq!(block.exit_code, 101);
        assert!(block.truncated);

        assert_eq!(semantic.source_sequence, 0);
        assert_eq!(semantic.output_total_bytes, 300_000);
        assert!(semantic.command_exact);
        assert_eq!(semantic.cwd_after.as_deref(), Some("/workspace/anvil"));
    }

    #[test]
    fn legacy_context_without_source_shell_deserializes_fail_closed() {
        let mut serialized = serde_json::to_value(context()).unwrap();
        serialized.as_object_mut().unwrap().remove("source_shell");

        let restored: SemanticCommandContext = serde_json::from_value(serialized).unwrap();

        assert!(restored.source_shell.is_none());
    }

    #[test]
    fn missing_or_blank_command_is_an_explicit_error() {
        let mut semantic = context();
        semantic.command = None;
        assert_eq!(
            semantic.to_block_context(),
            Err(ContextError::MissingCommand)
        );

        semantic.command = Some(" \t\n".to_string());
        assert_eq!(
            semantic.to_block_context(),
            Err(ContextError::MissingCommand)
        );
    }

    #[test]
    fn missing_exit_code_uses_explained_bounded_compatibility_sentinel() {
        let mut semantic = context();
        semantic.exit_code = None;
        semantic.output_truncated = false;
        semantic.output_text = "diagnostic\n".to_string();
        semantic.output_total_bytes = semantic.output_text.len();

        let block = semantic
            .to_block_context()
            .expect("unknown status is analyzable");
        assert_eq!(block.exit_code, UNKNOWN_EXIT_STATUS_SENTINEL);
        assert!(block.output.starts_with(UNKNOWN_EXIT_STATUS_NOTE));
        assert!(block.output.ends_with("diagnostic\n"));
        assert!(!block.truncated);
        assert_eq!(
            semantic.exit_code, None,
            "semantic provenance stays unknown"
        );
    }

    #[test]
    fn unknown_exit_note_is_counted_in_output_budget_and_truncation() {
        let mut semantic = context();
        semantic.exit_code = None;
        semantic.output_truncated = false;
        let source_budget = AGENT_BLOCK_OUTPUT_PROMPT_BYTES - UNKNOWN_EXIT_STATUS_NOTE.len();
        semantic.output_text = "x".repeat(source_budget);
        semantic.output_total_bytes = semantic.output_text.len();

        let exact = semantic.to_block_context().unwrap();
        assert_eq!(exact.output.len(), AGENT_BLOCK_OUTPUT_PROMPT_BYTES);
        assert!(!exact.truncated);

        semantic.output_text.push('x');
        semantic.output_total_bytes += 1;
        let clipped = semantic.to_block_context().unwrap();
        assert!(clipped.output.len() <= AGENT_BLOCK_OUTPUT_PROMPT_BYTES);
        assert!(clipped.output.starts_with(UNKNOWN_EXIT_STATUS_NOTE));
        assert!(clipped.output.contains("bytes elided"));
        assert!(clipped.truncated);
    }

    #[test]
    fn unavailable_output_is_not_conflated_with_real_empty_output() {
        let mut unavailable = context();
        unavailable.output_available = false;
        unavailable.output_text.clear();
        unavailable.output_total_bytes = 0;
        assert_eq!(
            unavailable.to_block_context(),
            Err(ContextError::OutputUnavailable)
        );

        let mut empty = context();
        empty.output_text.clear();
        empty.output_truncated = false;
        empty.output_total_bytes = 0;
        let block = empty
            .to_block_context()
            .expect("captured empty output is valid evidence");
        assert!(block.output.is_empty());
        assert!(!block.truncated);
    }

    #[test]
    fn truncated_command_is_rejected_because_compat_context_cannot_mark_it() {
        let mut semantic = context();
        semantic.command_exact = false;
        semantic.command_truncated = true;

        assert_eq!(
            semantic.to_block_context(),
            Err(ContextError::CommandTruncated)
        );
    }

    #[test]
    fn prompt_budgets_never_silently_elide_exact_command_or_cwd() {
        let mut semantic = context();
        semantic.command = Some("x".repeat(AGENT_BLOCK_COMMAND_PROMPT_BYTES));
        assert!(semantic.to_block_context().is_ok());
        semantic.command = Some("x".repeat(AGENT_BLOCK_COMMAND_PROMPT_BYTES + 1));
        assert_eq!(
            semantic.to_block_context(),
            Err(ContextError::CommandExceedsPromptBudget)
        );

        semantic.command = Some("cargo test".to_string());
        semantic.cwd = Some("/".repeat(AGENT_BLOCK_CWD_PROMPT_BYTES + 1));
        assert_eq!(
            semantic.to_block_context(),
            Err(ContextError::CwdExceedsPromptBudget)
        );
    }

    #[test]
    fn prompt_level_output_elision_is_reported_as_truncation() {
        let mut semantic = context();
        semantic.output_truncated = false;
        semantic.output_text = "x".repeat(AGENT_BLOCK_OUTPUT_PROMPT_BYTES);
        semantic.output_total_bytes = semantic.output_text.len();
        assert!(!semantic.to_block_context().unwrap().truncated);

        semantic.output_text.push('x');
        semantic.output_total_bytes += 1;
        assert!(semantic.to_block_context().unwrap().truncated);

        semantic.output_text.truncate(128);
        semantic.output_total_bytes = AGENT_BLOCK_OUTPUT_PROMPT_BYTES + 1;
        assert!(semantic.to_block_context().unwrap().truncated);
    }

    #[test]
    fn display_reconstructed_command_remains_untrusted_analyzable_evidence() {
        let mut semantic = context();
        semantic.command_exact = false;

        let block = semantic
            .to_block_context()
            .expect("inexact display text is safe as user-role evidence");
        assert_eq!(block.cmd, "cargo test");
    }

    #[test]
    fn source_identity_must_be_present_even_though_compat_type_drops_it() {
        let mut semantic = context();
        semantic.source_session_id.clear();
        assert_eq!(
            semantic.to_block_context(),
            Err(ContextError::MissingSourceSessionId)
        );

        semantic.source_session_id = "session-stable".to_string();
        semantic.source_execution_id.clear();
        assert_eq!(
            BlockContext::try_from(&semantic),
            Err(ContextError::MissingSourceExecutionId)
        );
    }

    #[test]
    fn agent_preflight_distinguishes_background_from_omitted_command_metadata() {
        assert_eq!(
            block_agent_context_disabled_reason(None, false, false, None, Some(true)),
            None,
            "captured background output needs no invented command or cwd"
        );
        assert_eq!(
            block_agent_context_disabled_reason(None, false, true, None, Some(true)),
            Some("The shell omitted or truncated the command metadata")
        );
        assert_eq!(
            block_agent_context_disabled_reason(Some("echo ok"), true, false, None, Some(true)),
            Some("The command working directory is unavailable")
        );
        assert_eq!(
            block_agent_context_disabled_reason(
                Some("echo ok"),
                true,
                false,
                Some("/tmp"),
                Some(false),
            ),
            Some("Captured block output is unavailable")
        );
    }

    #[test]
    fn agent_preflight_mirrors_command_and_cwd_prompt_budgets() {
        let command_at_limit = "x".repeat(AGENT_BLOCK_COMMAND_PROMPT_BYTES);
        let cwd_at_limit = "/".repeat(AGENT_BLOCK_CWD_PROMPT_BYTES);
        assert_eq!(
            block_agent_context_disabled_reason(
                Some(&command_at_limit),
                true,
                false,
                Some(&cwd_at_limit),
                Some(true),
            ),
            None
        );
        let command_over = format!("{command_at_limit}x");
        assert_eq!(
            block_agent_context_disabled_reason(
                Some(&command_over),
                true,
                false,
                Some("/tmp"),
                Some(true),
            ),
            Some("The exact command exceeds the Agent context limit")
        );
        let cwd_over = format!("{cwd_at_limit}x");
        assert_eq!(
            block_agent_context_disabled_reason(
                Some("echo ok"),
                true,
                false,
                Some(&cwd_over),
                Some(true),
            ),
            Some("The command working directory exceeds the Agent context limit")
        );
    }

    #[test]
    fn ad_hoc_context_passes_an_ordinary_block_through() {
        let block = ad_hoc_block_context(&context());
        assert_eq!(block.cmd, "cargo test");
        assert_eq!(block.output, "failures:\n    terminal::test");
        assert_eq!(block.cwd.as_deref(), Some("/workspace/anvil"));
        assert_eq!(block.exit_code, 101);
        // 捕获本身已被标记 truncated（capture truncation），适配器原样转发。
        assert!(block.truncated);
    }

    #[test]
    fn ad_hoc_context_marks_unknown_status_with_the_explained_sentinel() {
        let mut semantic = context();
        semantic.exit_code = None;
        semantic.output_truncated = false;

        let block = ad_hoc_block_context(&semantic);
        assert_eq!(block.exit_code, UNKNOWN_EXIT_STATUS_SENTINEL);
        assert!(block.output.starts_with(UNKNOWN_EXIT_STATUS_NOTE));
        assert!(block.output.ends_with("failures:\n    terminal::test"));
        assert!(!block.truncated);
    }

    #[test]
    fn ad_hoc_context_never_conflates_unavailable_output_with_empty_output() {
        let mut semantic = context();
        semantic.output_available = false;
        semantic.output_text.clear();
        semantic.output_total_bytes = 0;
        semantic.output_truncated = false;

        let block = ad_hoc_block_context(&semantic);
        assert_eq!(
            block.output,
            "[output unavailable: retained snapshot and scrollback rows were evicted]"
        );
        assert!(block.truncated);

        // 家族共享的分层：未上报状态的注记同样加在占位文本之前。
        semantic.exit_code = None;
        let block = ad_hoc_block_context(&semantic);
        assert!(block.output.starts_with(UNKNOWN_EXIT_STATUS_NOTE));
        assert!(block.output.contains("output unavailable"));
        assert!(block.truncated);
    }

    #[test]
    fn ad_hoc_context_bounds_long_output_to_a_head_tail_window() {
        let mut semantic = context();
        semantic.output_truncated = false;
        semantic.output_text = (0..500)
            .map(|line| format!("line-{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        semantic.output_total_bytes = semantic.output_text.len();

        let block = ad_hoc_block_context(&semantic);
        assert!(block.output.contains("lines elided"), "{}", block.output);
        assert!(block.output.starts_with("line-0\n"));
        assert!(block.output.ends_with("line-499"));
        assert!(block.truncated);
    }

    #[test]
    fn ad_hoc_context_keeps_background_blocks_attachable() {
        let mut semantic = context();
        semantic.command = None;
        semantic.command_exact = false;
        semantic.cwd = None;
        semantic.exit_code = None;
        // 严格适配器拒绝同一份证据（MissingCommand）。
        assert_eq!(
            semantic.to_block_context(),
            Err(ContextError::MissingCommand)
        );

        let block = ad_hoc_block_context(&semantic);
        assert_eq!(block.cmd, "");
        assert_eq!(block.cwd, None);
        assert_eq!(block.exit_code, UNKNOWN_EXIT_STATUS_SENTINEL);
        assert!(block.output.starts_with(UNKNOWN_EXIT_STATUS_NOTE));
    }

    #[test]
    fn ad_hoc_context_propagates_producer_declared_truncation() {
        let mut semantic = context();
        semantic.command_truncated = true;
        semantic.output_truncated = false;
        // 严格适配器拒绝截断命令；宽松路径保留证据但绝不洗掉截断标记。
        assert_eq!(
            semantic.to_block_context(),
            Err(ContextError::CommandTruncated)
        );

        let block = ad_hoc_block_context(&semantic);
        assert_eq!(block.cmd, "cargo test");
        assert!(block.truncated);
    }
}
