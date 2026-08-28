//! Provider-neutral agent/task domain types.
//!
//! Ported wholesale from ember's `src/agent/` (via frost's de-coupled
//! `src/agent_task/`). The GTK tasks panel consumes only part of this
//! library-style surface today; the rest (queue stats, alternate approval
//! decisions, additional providers) is retained deliberately so the
//! frontends stay aligned and future slices need no core changes.
//!
//! Provider adapters and UI surfaces should depend on these types rather than
//! on a provider's wire protocol. Compatibility conversions into the current
//! `jterm_core` agent prompt live beside the domain model so any loss of
//! provenance is explicit and testable.

pub mod context;
pub mod diff;
pub mod driver;
pub mod drivers;
pub mod event;
pub mod launcher;
pub mod native;
pub mod pinned_dir;
pub mod runtime;
pub mod task;
pub mod validation;
pub mod worktree;

// The re-exports below are the module's curated API surface, kept aligned
// with ember's so the frontends can share adapters and documentation. The
// panel consumes only part of it today, so items without a current caller
// are explicitly allowed to stay.
#[allow(unused_imports)]
pub use context::{ContextError, SemanticCommandContext};
#[allow(unused_imports)]
pub use diff::{AgentDiffPanel, AgentDiffState, DiffRequestError};
#[allow(unused_imports)]
pub use driver::{
    AgentCancellation, AgentCommand, AgentDriver, AgentDriverError, AgentEventQueueLimits,
    AgentEventQueueStats, AgentEventReceiveError, AgentEventReceiver, AgentEventSendError,
    AgentEventSender, AgentEventSink, AgentPrompt, AgentStartRequest, ApprovalDecision,
};
#[allow(unused_imports)]
pub use drivers::{
    CodexAppServerApproval, CodexAppServerApprovalFileChange, CodexAppServerApprovalKind,
    CodexAppServerCommandView, CodexAppServerExitCause, CodexAppServerExitReport,
    CodexAppServerFileChange, CodexAppServerFileChangeView, CodexAppServerPhase,
    CodexAppServerProcessExit, CodexAppServerTurnCommandSummary, CodexAppServerTurnFileSummary,
    CodexAppServerTurnHistory, CodexAppServerViewSnapshot, CODEX_APP_SERVER_LIVE_TURN_MAX,
    CODEX_APP_SERVER_TURN_HISTORY_CAPACITY, CODEX_APP_SERVER_TURN_HISTORY_MAX_BYTES,
};
#[allow(unused_imports)]
pub use event::{
    AgentEvent, AgentEventEpoch, AgentEventError, AgentEventKind, AgentEventStream,
    AgentSessionOutcome, AgentTurnId, ApprovalId, InvalidNativeAgentSessionId,
    InvalidProviderSessionId, NativeAgentSessionId, ProviderSessionId,
};
#[allow(unused_imports)]
pub use launcher::{AgentLaunchError, AgentLaunchSpec};
#[allow(unused_imports)]
pub use native::{
    NativeCodexHomeError, NativePromptError, NativePromptPolicy, NativeWorkspaceError,
    NATIVE_AGENT_FOLLOW_UP_MAX_BYTES,
};
#[allow(unused_imports)]
pub use runtime::{
    AgentRuntimeCompletion, AgentRuntimeError, AgentRuntimeIssue, AgentRuntimeManager,
    AgentRuntimePollReport,
};
#[allow(unused_imports)]
pub use task::{
    AgentProvider, AgentTask, NewTask, TaskError, TaskId, TaskManager, TaskRuntimeKind, TaskSource,
    TaskStatus, TaskTerminalRole, TaskValidationState, TaskValidationStatus,
};
#[allow(unused_imports)]
pub use validation::{
    prepare_task_validation, PreparedTaskValidation, TaskValidationError, TaskValidationPath,
};
#[allow(unused_imports)]
pub use worktree::{
    CreateWorktreeRequest, ManagedWorktree, RetireOutcome, RetirePolicy, WorktreeError,
    WorktreeService,
};
