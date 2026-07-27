//! Pane and tab controller state owned by the Relm4 application model.
//!
//! These types still hold GTK widgets and Relm4 controllers; the extraction is
//! an ownership boundary, not a second UI framework or an alternate state loop.

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::Controller;
use relm4::ComponentController;

use crate::config::{self, TerminalMode};
use crate::process;
use crate::terminal::{self, BlockTerminal, VteInput, VteTerminal};

/// Backend-neutral controller used by pane-management code.
pub(crate) enum TermCtl {
    Vte(Controller<VteTerminal>),
    Block(Controller<BlockTerminal>),
}

impl TermCtl {
    pub(crate) fn emit(&self, msg: VteInput) {
        match self {
            Self::Vte(controller) => controller.emit(msg),
            Self::Block(controller) => controller.emit(msg),
        }
    }

    pub(crate) fn widget(&self) -> gtk::Widget {
        match self {
            Self::Vte(controller) => controller.widget().clone().upcast(),
            Self::Block(controller) => controller.widget().clone().upcast(),
        }
    }

    /// Agent execution is intentionally Block-only and requires a clean,
    /// shell-integrated prompt with no existing user or programmatic input.
    pub(crate) fn can_accept_agent_command(&self) -> bool {
        match self {
            Self::Vte(_) => false,
            Self::Block(controller) => controller.model().can_accept_agent_command(),
        }
    }

    pub(crate) fn block_debug_info(&self) -> Option<crate::block_view::DebugInfo> {
        match self {
            Self::Vte(_) => None,
            Self::Block(controller) => Some(controller.model().debug_info()),
        }
    }
}

pub(crate) struct Pane {
    pub(crate) terminal: TermCtl,
    /// Status header plus the terminal. This — not `terminal.widget()` — is
    /// what the `gtk::Paned` split tree holds for this pane.
    pub(crate) frame: crate::pane_header::PaneFrame,
    pub(crate) id: u64,
    /// Latest OSC title this pane reported. Tabs already fold this into their
    /// own label, but a split tab needs it per pane.
    pub(crate) title: Option<String>,
    pub(crate) cwd: Option<String>,
    /// The reported cwd belongs to an ssh/mosh/container namespace. It remains
    /// useful as terminal/AI context but must not drive local filesystem work.
    pub(crate) cwd_external: bool,
    pub(crate) session_id: Option<String>,
    pub(crate) mode: TerminalMode,
    pub(crate) probe: terminal::PaneProbe,
}

impl Pane {
    /// The widget this pane occupies in the split tree.
    pub(crate) fn widget(&self) -> gtk::Widget {
        self.frame.widget()
    }

    pub(crate) fn local_cwd(&self) -> Option<&str> {
        (!self.cwd_external)
            .then_some(self.cwd.as_deref())
            .flatten()
    }

    /// A restorable command running in this pane, or `None` when the
    /// foreground process is the pane's normal shell.
    pub(crate) fn restorable_command(&self) -> Option<Vec<String>> {
        process::restorable_command(self.probe.pty_fd.get(), self.probe.shell_pid.get())
    }

    pub(crate) fn foreground_process(&self) -> Option<String> {
        process::foreground_process_name(self.probe.pty_fd.get(), self.probe.shell_pid.get())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnStatus {
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Clone)]
pub(crate) struct RemoteConn {
    pub(crate) host: config::RemoteHost,
    /// Stable identity of the pane that owns this connection. A tab can also
    /// contain local split panes, so tab-level metadata alone is ambiguous.
    pub(crate) pane_id: u64,
    pub(crate) status: ConnStatus,
    pub(crate) attempt: u32,
    pub(crate) spawn_at: std::time::Instant,
}

/// Saved tree position of the active pane while a tab is pane-zoomed.
pub(crate) struct ZoomState {
    pub(crate) tree_root: gtk::Widget,
    pub(crate) pane_widget: gtk::Widget,
    pub(crate) parent: gtk::Paned,
    pub(crate) was_start: bool,
}

pub(crate) struct Tab {
    pub(crate) holder: gtk::Box,
    pub(crate) panes: Vec<Pane>,
    pub(crate) active_pane: usize,
    pub(crate) title: String,
    pub(crate) custom_title: bool,
    pub(crate) bell: bool,
    pub(crate) activity: bool,
    pub(crate) marked: bool,
    pub(crate) pinned: bool,
    pub(crate) id: u64,
    pub(crate) zoom: Option<ZoomState>,
    pub(crate) remote: Option<RemoteConn>,
}
