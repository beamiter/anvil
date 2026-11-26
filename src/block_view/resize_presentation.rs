use super::BlockState;
use crate::pty::OwnedPty;
use gtk::prelude::*;
use gtk::{glib, subclass::prelude::*};
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

const RESIZE_SETTLE: Duration = Duration::from_millis(160);
const REPAINT_QUIET: Duration = Duration::from_millis(120);
const PRESENTATION_LIMIT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
struct Repaint {
    publish_at: Instant,
    limit: Instant,
}

impl Repaint {
    fn new(now: Instant) -> Self {
        Self {
            publish_at: now + REPAINT_QUIET,
            limit: now + PRESENTATION_LIMIT,
        }
    }

    fn note_output(&mut self, now: Instant) {
        self.publish_at = (now + REPAINT_QUIET).min(self.limit);
    }

    fn finished(self, now: Instant, synchronized: bool) -> bool {
        now >= self.limit || (!synchronized && now >= self.publish_at)
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ResizeSurface {
        pub(super) frame: RefCell<Option<gtk::gsk::RenderNode>>,
        pub(super) repaint: Cell<Option<Repaint>>,
        pub(super) synchronized_at: Cell<Option<Instant>>,
        pub(super) timer_running: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ResizeSurface {
        const NAME: &'static str = "AnvilTerminalResizeSurface";
        type Type = super::ResizeSurface;
        type ParentType = gtk::Fixed;
    }

    impl ObjectImpl for ResizeSurface {}
    impl FixedImpl for ResizeSurface {}
    impl WidgetImpl for ResizeSurface {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            if !self.obj().held() {
                let live = gtk::Snapshot::new();
                self.parent_snapshot(&live);
                self.frame.replace(live.to_node());
            }
            if let Some(frame) = self.frame.borrow().as_ref() {
                snapshot.append_node(frame);
            }
        }
    }
}

glib::wrapper! {
    pub struct ResizeSurface(ObjectSubclass<imp::ResizeSurface>)
        @extends gtk::Fixed, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ResizeSurface {
    pub(super) fn new() -> Self {
        glib::Object::new()
    }

    fn held(&self) -> bool {
        self.imp().repaint.get().is_some() || self.imp().synchronized_at.get().is_some()
    }

    fn begin_resize(&self, now: Instant) {
        self.imp().repaint.set(Some(Repaint::new(now)));
        self.schedule_presentation();
    }

    fn note_output(&self, now: Instant) {
        if let Some(mut repaint) = self.imp().repaint.get() {
            repaint.note_output(now);
            self.imp().repaint.set(Some(repaint));
        }
    }

    fn set_synchronized(&self, enabled: bool, now: Instant) {
        let synchronized_at = &self.imp().synchronized_at;
        if enabled {
            if synchronized_at.get().is_none() {
                synchronized_at.set(Some(now));
                self.schedule_presentation();
            }
        } else if synchronized_at.take().is_some() && !self.held() {
            self.queue_draw();
        }
    }

    fn schedule_presentation(&self) {
        if self.imp().timer_running.replace(true) {
            return;
        }
        let surface = self.downgrade();
        glib::timeout_add_local(Duration::from_millis(25), move || {
            let Some(surface) = surface.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let now = Instant::now();
            let imp = surface.imp();
            if imp
                .synchronized_at
                .get()
                .is_some_and(|start| now >= start + PRESENTATION_LIMIT)
            {
                imp.synchronized_at.set(None);
            }
            if imp
                .repaint
                .get()
                .is_some_and(|repaint| repaint.finished(now, imp.synchronized_at.get().is_some()))
            {
                imp.repaint.set(None);
            }
            if surface.held() {
                return glib::ControlFlow::Continue;
            }
            imp.timer_running.set(false);
            surface.queue_draw();
            glib::ControlFlow::Break
        });
    }
}

#[cfg(test)]
#[test]
fn resize_repaint_outlasts_a_quiet_synchronized_chunk() {
    let now = Instant::now();
    let mut repaint = Repaint::new(now);
    let next_chunk = now + REPAINT_QUIET * 2;
    assert!(!repaint.finished(next_chunk, true));
    repaint.note_output(next_chunk);
    assert!(!repaint.finished(next_chunk, false));
    assert!(repaint.finished(next_chunk + REPAINT_QUIET, false));
    assert!(repaint.finished(now + PRESENTATION_LIMIT, true));
}

/// Only layout-driven changes for a live inline CLI wait for a stable size.
/// Synchronous shell transitions can still publish before the child's first
/// read. The VTE keeps ingesting output and answering protocol queries during
/// the repaint hold; snapshotting is the only thing deferred.
pub(super) struct InlineResize {
    surface: glib::WeakRef<ResizeSurface>,
    block_state: Rc<Cell<BlockState>>,
    running_command: Rc<RefCell<String>>,
    direct_cli: bool,
    pending: Cell<Option<PendingResize>>,
}

#[derive(Clone, Copy)]
struct PendingResize {
    grid: (u16, u16),
    pane: (i32, i32),
    window: (i32, i32),
    send_at: Instant,
}

impl InlineResize {
    pub(super) fn new(
        surface: &ResizeSurface,
        block_state: Rc<Cell<BlockState>>,
        running_command: Rc<RefCell<String>>,
        direct_cli: bool,
    ) -> Self {
        Self {
            surface: surface.downgrade(),
            block_state,
            running_command,
            direct_cli,
            pending: Cell::new(None),
        }
    }

    fn active(&self, pty: &OwnedPty) -> bool {
        let state = self.block_state.get();
        state != BlockState::AltScreen
            && (self.direct_cli
                || (state == BlockState::CollectingOutput
                    && self
                        .running_command
                        .try_borrow()
                        .is_ok_and(|command| crate::agent_cli::is_inline_session(&command)))
                || jterm_core::process::tty_foreground_pgid(pty.master_fd_raw())
                    .filter(|pid| *pid != pty.pid_i32())
                    .and_then(jterm_core::process::process_comm)
                    .as_deref()
                    .is_some_and(crate::immersive_command::reading_agent_command))
    }

    /// A deferred sample keeps the frame-clock observer running. It samples
    /// the absolute geometry again before sending, so a newer size replaces it.
    #[cfg(any(anvil_vte_oracle, anvil_legacy_renderer))]
    pub(super) fn defer_layout(
        &self,
        pty: &OwnedPty,
        sent: (u16, u16),
        sample: (u16, u16),
        pane: (i32, i32),
        terminal: &vte4::Terminal,
    ) -> bool {
        if sent == (0, 0) || sample == sent || sample.0 == 0 || sample.1 == 0 || !self.active(pty) {
            self.pending.set(None);
            return false;
        }
        // GTK applies pane width and height in different allocations. A
        // stable VTE sample under a moving native window is still provisional.
        let window = terminal
            .native()
            .and_then(|native| native.surface())
            .map_or((0, 0), |surface| (surface.width(), surface.height()));
        let now = Instant::now();
        if let Some(pending) = self.pending.get() {
            if (pending.grid, pending.pane, pending.window) == (sample, pane, window) {
                return now < pending.send_at;
            }
        }
        self.pending.set(Some(PendingResize {
            grid: sample,
            pane,
            window,
            send_at: now + RESIZE_SETTLE,
        }));
        true
    }

    pub(super) fn resized(&self, pty: &OwnedPty) {
        self.pending.set(None);
        if self.active(pty) {
            if let Some(surface) = self.surface.upgrade() {
                surface.begin_resize(Instant::now());
            }
        }
    }

    pub(super) fn note_output(&self) {
        if let Some(surface) = self.surface.upgrade() {
            surface.note_output(Instant::now());
        }
    }

    pub(super) fn set_synchronized(&self, enabled: bool) {
        if let Some(surface) = self.surface.upgrade() {
            surface.set_synchronized(enabled, Instant::now());
        }
    }
}
