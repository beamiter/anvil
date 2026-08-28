//! Provider adapters for the native Agent runtime.

pub mod codex_app_server;
pub mod fake;

pub use codex_app_server::{
    CodexAppServerApproval, CodexAppServerApprovalFileChange, CodexAppServerApprovalKind,
    CodexAppServerCommandView, CodexAppServerExitCause, CodexAppServerExitReport,
    CodexAppServerFileChange, CodexAppServerFileChangeView, CodexAppServerPhase,
    CodexAppServerProcessExit, CodexAppServerTurnCommandSummary, CodexAppServerTurnFileSummary,
    CodexAppServerTurnHistory, CodexAppServerViewSnapshot, CODEX_APP_SERVER_LIVE_TURN_MAX,
    CODEX_APP_SERVER_TURN_HISTORY_CAPACITY, CODEX_APP_SERVER_TURN_HISTORY_MAX_BYTES,
};
#[allow(unused_imports)] // test-support adapter, exercised by driver tests
pub use fake::{FakeAgentDriver, FakeAgentEvent, FakeAgentProgress};
