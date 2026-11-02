//! Motion for a still-running inline CLI's native VTE history. Coordinates are
//! logical pixels; VTE's public adjustment remains in terminal rows.
use super::scroll::ScrollDebouncer;
use gtk::prelude::*;
use gtk::{glib, ScrolledWindow};
use jterm_core::transcript_canvas::CanvasMotion;
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::{Terminal, TerminalExt};

#[derive(Clone)]
pub(super) struct NativeTranscriptScroll {
    state: Rc<Cell<super::BlockState>>,
    direct_cli: bool,
    motion: Rc<RefCell<CanvasMotion>>,
    tick: Rc<RefCell<Option<gtk::TickCallbackId>>>,
    expected: Rc<Cell<Option<f64>>>,
    row_height: Rc<Cell<f64>>,
}

impl NativeTranscriptScroll {
    pub(super) fn new(state: Rc<Cell<super::BlockState>>, direct_cli: bool) -> Self {
        Self {
            state,
            direct_cli,
            motion: Rc::new(RefCell::new(CanvasMotion::default())),
            tick: Rc::new(RefCell::new(None)),
            expected: Rc::new(Cell::new(None)),
            row_height: Rc::new(Cell::new(0.0)),
        }
    }

    pub(super) fn owns_session(&self, command: &str) -> bool {
        match self.state.get() {
            super::BlockState::CollectingOutput => {
                self.direct_cli || crate::agent_cli::is_inline_session(command)
            }
            super::BlockState::Idle | super::BlockState::RawFallback => self.direct_cli,
            _ => false,
        }
    }

    fn can_animate(&self) -> bool {
        self.state.get() == super::BlockState::CollectingOutput
            || (self.direct_cli
                && matches!(
                    self.state.get(),
                    super::BlockState::Idle | super::BlockState::RawFallback
                ))
    }

    pub(super) fn install(&self, terminal: &Terminal) {
        let this = self.clone();
        terminal.connect_unmap(move |_| this.cancel());
        // Scrollbar drag and searches always interrupt queued wheel motion.
        // Observe changes on the next frame, outside VTE's signal stack.
    }

    pub(super) fn cancel(&self) {
        let tick = self.tick.borrow_mut().take();
        if let Some(tick) = tick {
            tick.remove();
        }
        self.motion.borrow_mut().cancel();
        self.expected.set(None);
    }

    /// Return only the distance beyond the native history's edge. Use the
    /// queued destination for consumption so a second wheel event extends the
    /// same trajectory instead of replaying its still-unpresented distance.
    pub(super) fn scroll_pixels(&self, terminal: &Terminal, delta: f64, direct: bool) -> f64 {
        if !delta.is_finite() || delta == 0.0 {
            return 0.0;
        }
        let Some(adjustment) = terminal.vadjustment() else {
            return delta;
        };
        let height = (terminal.char_height() as f64).max(1.0);
        if self.row_height.replace(height) != height {
            self.cancel();
        }
        let bottom = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
        let reduced = !terminal.settings().is_gtk_enable_animations();
        let remaining = {
            let mut motion = self.motion.borrow_mut();
            motion.set_bounds(adjustment.lower() * height, bottom * height);
            motion.set_reduced_motion(reduced);
            if self
                .expected
                .get()
                .is_none_or(|expected| (expected - adjustment.value()).abs() > 1e-7)
            {
                motion.jump_to(adjustment.value() * height);
            }
            apply_input(&mut motion, delta, direct, adjustment.value() * height)
        };
        // Depart the live edge in the input turn. Otherwise streaming output
        // can auto-follow before the first nonzero frame and cancel the user's
        // scroll-away intent as though it were a scrollbar drag.
        let leaving_live =
            delta < 0.0 && !direct && !reduced && (adjustment.value() - bottom).abs() < 1e-7;
        if leaving_live {
            self.motion.borrow_mut().advance(1.0 / 120.0);
        }
        self.expected.set(Some(adjustment.value()));
        if direct || reduced || leaving_live || !terminal.is_mapped() {
            if !terminal.is_mapped() {
                let target = self.motion.borrow().target();
                self.motion.borrow_mut().jump_to(target);
            }
            adjustment.set_value(self.motion.borrow().position() / height);
            self.expected.set(Some(adjustment.value()));
        }
        self.start_clock(terminal);
        if remaining.abs() < 1e-7 {
            0.0
        } else {
            remaining
        }
    }

    pub(super) fn wheel(
        &self,
        terminal: &Terminal,
        outer: &ScrolledWindow,
        canvas: &ScrollDebouncer,
        delta: f64,
        surface: bool,
    ) {
        let adjustment = outer.vadjustment();
        let bottom = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
        if adjustment.value() < bottom - 0.5 {
            self.cancel();
            canvas.scroll_wheel(outer, delta, surface);
            return;
        }
        let pixels = if surface { delta } else { delta * 54.0 };
        let remaining = self.scroll_pixels(terminal, pixels, surface);
        if remaining != 0.0 {
            if surface {
                canvas.scroll_wheel(outer, remaining, true);
            } else {
                canvas.scroll_pixels(outer, remaining);
            }
        }
    }

    pub(super) fn page(
        &self,
        terminal: &Terminal,
        outer: &ScrolledWindow,
        canvas: &ScrollDebouncer,
        down: bool,
    ) {
        let height = (terminal.char_height() as f64).max(1.0);
        let adjustment = outer.vadjustment();
        let bottom = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
        let direction = if down { 1.0 } else { -1.0 };
        // Once the reader has moved into previous command cards, page that
        // document by its own viewport. The live CLI may be fully offscreen.
        if adjustment.value() < bottom - 0.5 {
            self.cancel();
            canvas.scroll_pixels(
                outer,
                page_distance(adjustment.page_size(), height) * direction,
            );
            return;
        }
        // The outer viewport also includes the card header and dock. Intersect
        // every ancestor clip with the actual terminal allocation; otherwise a
        // page can skip transcript lines hidden beneath that chrome. Reserve
        // the live CLI's final editor/status row before retaining two lines.
        let page = (visible_terminal_height(terminal, outer) - height).max(height);
        let delta = page_distance(page, height) * direction;
        let remaining = self.scroll_pixels(terminal, delta, false);
        if remaining != 0.0 {
            canvas.scroll_pixels(outer, remaining);
        }
    }

    fn start_clock(&self, terminal: &Terminal) {
        if self.tick.borrow().is_some()
            || !terminal.is_mapped()
            || !self.motion.borrow().is_animating()
        {
            return;
        }
        let this = self.clone();
        let last = Cell::new(None::<i64>);
        let id = terminal.add_tick_callback(move |terminal, clock| {
            let finish = || {
                this.tick.borrow_mut().take();
                this.motion.borrow_mut().cancel();
                this.expected.set(None);
                glib::ControlFlow::Break
            };
            let Some(adjustment) = terminal.vadjustment() else {
                return finish();
            };
            if !terminal.is_mapped()
                || !this.can_animate()
                || this
                    .expected
                    .get()
                    .is_none_or(|expected| (expected - adjustment.value()).abs() > 1e-7)
            {
                return finish();
            }
            if !native_history_available(terminal) {
                return finish();
            }
            let height = (terminal.char_height() as f64).max(1.0);
            if this.row_height.get() != height {
                return finish();
            }
            let bottom = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
            let now = clock.frame_time();
            let dt = last
                .replace(Some(now))
                .map_or(0.0, |before| (now - before) as f64 / 1_000_000.0);
            let (position, animating) = {
                let mut motion = this.motion.borrow_mut();
                motion.set_bounds(adjustment.lower() * height, bottom * height);
                motion.set_reduced_motion(!terminal.settings().is_gtk_enable_animations());
                motion.advance(dt);
                (motion.position(), motion.is_animating())
            };
            adjustment.set_value(position / height);
            this.expected.set(Some(adjustment.value()));
            if animating {
                glib::ControlFlow::Continue
            } else {
                finish()
            }
        });
        *self.tick.borrow_mut() = Some(id);
    }
}

/// Conventional VTE task/resume panes have no shell lifecycle reducer. Enable
/// only from exact CLI launch argv or the provider-owned task/resume callsite.
pub(crate) fn install_inline_cli_reading(terminal: &Terminal) {
    if std::env::var_os("JTERM_AGENT_SCROLLBACK").as_deref() == Some(std::ffi::OsStr::new("0"))
        || terminal.has_css_class("inline-agent-reading")
    {
        return;
    }
    terminal.add_css_class("inline-agent-reading");
    terminal.set_scroll_on_output(false);
    let state = Rc::new(Cell::new(super::BlockState::RawFallback));
    let native = NativeTranscriptScroll::new(state, true);
    native.install(terminal);
    let this = native.clone();
    terminal.connect_child_exited(move |_, _| this.cancel());
    let this = native.clone();
    terminal.connect_contents_changed(move |terminal| {
        if !native_history_available(terminal) {
            this.cancel();
        }
    });
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let this = native.clone();
    let weak = terminal.downgrade();
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        use gtk::gdk::{Key, ModifierType};
        if modifiers.intersects(
            ModifierType::CONTROL_MASK
                | ModifierType::SHIFT_MASK
                | ModifierType::ALT_MASK
                | ModifierType::SUPER_MASK
                | ModifierType::META_MASK,
        ) || !matches!(
            key,
            Key::Page_Up | Key::Page_Down | Key::KP_Page_Up | Key::KP_Page_Down
        ) {
            return glib::Propagation::Proceed;
        }
        let Some(terminal) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        // VTE exposes no normal-screen history range in an alternate screen.
        // Leave those Page keys to the CLI transcript/editor, including its
        // native alternate-scroll fallback. Never manufacture application keys.
        if !native_history_available(&terminal) {
            this.cancel();
            return glib::Propagation::Proceed;
        }
        let height = (terminal.char_height() as f64).max(1.0);
        let viewport = (terminal.row_count().max(1) as f64 - 1.0).max(1.0) * height;
        let down = matches!(key, Key::Page_Down | Key::KP_Page_Down);
        this.scroll_pixels(
            &terminal,
            page_distance(viewport, height) * if down { 1.0 } else { -1.0 },
            false,
        );
        glib::Propagation::Stop
    });
    terminal.add_controller(keys);
    let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
    let this = native.clone();
    wheel.connect_scroll_begin(move |_| this.cancel());
    let weak = terminal.downgrade();
    wheel.connect_scroll(move |controller, _, delta| {
        let Some(terminal) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        if !native_history_available(&terminal) {
            native.cancel();
            return glib::Propagation::Proceed;
        }
        let surface = controller.unit() == gtk::gdk::ScrollUnit::Surface;
        native.scroll_pixels(
            &terminal,
            if surface { delta } else { delta * 54.0 },
            surface,
        );
        glib::Propagation::Stop
    });
    terminal.add_controller(wheel);
}

fn native_history_available(terminal: &Terminal) -> bool {
    terminal.vadjustment().is_some_and(|adjustment| {
        adjustment.upper() - adjustment.lower() > adjustment.page_size() + 1e-7
    })
}

fn visible_terminal_height(terminal: &Terminal, outer: &ScrolledWindow) -> f64 {
    let mut top = 0.0_f64;
    let mut bottom = outer.vadjustment().page_size().max(1.0);
    let mut widget: gtk::Widget = terminal.clone().upcast();
    loop {
        if let Some(bounds) = widget.compute_bounds(outer) {
            top = top.max(f64::from(bounds.y()));
            bottom = bottom.min(f64::from(bounds.y() + bounds.height()));
        }
        let Some(parent) = widget.parent() else {
            break;
        };
        if parent == outer.clone().upcast::<gtk::Widget>() {
            break;
        }
        widget = parent;
    }
    (bottom - top).max(1.0)
}

/// Consumption is measured from the pending destination in the same direction,
/// and from the presented position on reversal. The caller can hand only the
/// unconsumed distance to the containing document.
fn apply_input(motion: &mut CanvasMotion, delta: f64, direct: bool, observed: f64) -> f64 {
    let before = if direct || delta.signum() != (motion.target() - motion.position()).signum() {
        motion.position()
    } else {
        motion.target()
    };
    if direct {
        motion.jump_to(observed + delta);
    } else {
        motion.scroll_by(delta);
    }
    let consumed = (motion.target() - before).clamp(delta.min(0.0), delta.max(0.0));
    delta - consumed
}

fn page_distance(viewport: f64, row_height: f64) -> f64 {
    (viewport - 2.0 * row_height)
        .max(row_height)
        .min(viewport.max(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_overlap_two_readable_lines_even_at_different_font_sizes() {
        for height in [12.0, 17.5, 24.0, 32.0] {
            assert_eq!(page_distance(30.0 * height, height), 28.0 * height);
            assert_eq!(page_distance(height, height), height);
        }
    }
    fn motion(position: f64) -> CanvasMotion {
        let mut motion = CanvasMotion::default();
        motion.set_bounds(0.0, 1000.0);
        motion.jump_to(position);
        motion
    }

    #[test]
    fn rapid_wheels_consume_queued_distance_once_and_handoff_only_the_remainder() {
        let mut motion = motion(900.0);
        assert_eq!(apply_input(&mut motion, 54.0, false, 900.0), 0.0);
        assert_eq!(apply_input(&mut motion, 54.0, false, 900.0), 8.0);
        assert_eq!(motion.position(), 900.0);
        assert_eq!(motion.target(), 1000.0);
    }

    #[test]
    fn reversing_pages_uses_the_frame_on_screen_and_preserves_pixel_distance() {
        let mut motion = motion(500.0);
        apply_input(&mut motion, 300.0, false, 500.0);
        motion.advance(1.0 / 60.0);
        let seen = motion.position();
        assert!(seen > 500.0 && seen < 800.0);
        assert_eq!(apply_input(&mut motion, -54.0, false, seen), 0.0);
        assert!((motion.target() - (seen - 54.0)).abs() < 1e-9);
    }

    #[test]
    fn precision_input_interrupts_pending_page_and_keeps_its_fractional_position() {
        let mut motion = motion(500.0);
        apply_input(&mut motion, -300.0, false, 500.0);
        assert_eq!(apply_input(&mut motion, -0.375, true, 500.0), 0.0);
        assert_eq!(motion.position(), 499.625);
        assert!(!motion.is_animating());
    }

    #[test]
    fn output_expands_bounds_without_retargeting_an_inflight_reading_page() {
        let mut motion = motion(900.0);
        apply_input(&mut motion, -300.0, false, 900.0);
        motion.advance(1.0 / 60.0);
        let seen = motion.position();
        motion.set_bounds(0.0, 1200.0);
        assert_eq!(motion.position(), seen);
        assert_eq!(motion.target(), 600.0);
    }
    #[test]
    fn direct_cli_launch_keeps_reading_without_shell_markers_and_yields_on_takeover() {
        use super::super::BlockState;
        let state = Rc::new(Cell::new(BlockState::Idle));
        let direct = NativeTranscriptScroll::new(state.clone(), true);
        let shell = NativeTranscriptScroll::new(state.clone(), false);
        for owner in [BlockState::Idle, BlockState::RawFallback] {
            state.set(owner);
            assert!(direct.owns_session(""));
            assert!(direct.can_animate());
            assert!(!shell.owns_session("codex"));
        }
        for owner in [
            BlockState::AltScreen,
            BlockState::AwaitingCommand,
            BlockState::PostCommand,
        ] {
            state.set(owner);
            assert!(!direct.owns_session("codex"));
            assert!(!direct.can_animate());
        }
    }
    #[test]
    #[ignore = "requires a display; run this test alone under Xvfb"]
    fn conventional_vte_pages_locally_preserves_streaming_reader_and_yields_to_alt_screen() {
        gtk::init().expect("GTK display for native VTE reading acceptance");
        let terminal = Terminal::new();
        terminal.set_scrollback_lines(1000);
        terminal.set_font(Some(&gtk::pango::FontDescription::from_string(
            "Monospace 13",
        )));
        let controllers = terminal.observe_controllers();
        let before = (0..controllers.n_items())
            .filter_map(|index| controllers.item(index))
            .collect::<Vec<_>>();
        install_inline_cli_reading(&terminal);
        let installed = controllers.n_items();
        install_inline_cli_reading(&terminal);
        assert_eq!(
            controllers.n_items(),
            installed,
            "managed/direct opt-ins share one controller"
        );
        let key = (0..installed)
            .filter_map(|index| controllers.item(index))
            .filter(|controller| !before.contains(controller))
            .find_map(|controller| controller.downcast::<gtk::EventControllerKey>().ok())
            .unwrap();
        let grid = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        terminal.set_hexpand(true);
        terminal.set_vexpand(true);
        grid.append(&terminal);
        grid.append(&gtk::Scrollbar::new(
            gtk::Orientation::Vertical,
            terminal.vadjustment().as_ref(),
        ));
        let window = gtk::Window::builder()
            .default_width(640)
            .default_height(420)
            .child(&grid)
            .build();
        window.present();
        let pump = |millis: u64| {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(millis);
            while std::time::Instant::now() < until {
                while glib::MainContext::default().iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        pump(120);
        let rows = (0..220)
            .map(|row| format!("ROW {row:04} native session history\r\n"))
            .collect::<String>();
        terminal.feed(rows.as_bytes());
        pump(120);
        let adjustment = terminal.vadjustment().unwrap();
        let bottom = adjustment.upper() - adjustment.page_size();
        assert!(native_history_available(&terminal));
        let page = || {
            key.emit_by_name::<bool>(
                "key-pressed",
                &[
                    &gtk::gdk::Key::Page_Up,
                    &0u32,
                    &gtk::gdk::ModifierType::empty(),
                ],
            )
        };
        assert!(
            page(),
            "bare paging belongs to the known inline CLI history"
        );
        terminal.feed(b"output racing the first animation frame\r\n");
        pump(500);
        assert!(
            adjustment.value() < bottom - 2.0,
            "streaming cannot reclaim scroll-away intent"
        );
        let anchor = adjustment.value();
        terminal.feed(b"later output\r\nmore later output\r\n");
        pump(120);
        assert!(
            (adjustment.value() - anchor).abs() < 1e-7,
            "new output preserves reader"
        );
        assert!(page());
        let dragged = (adjustment.value() - 3.25).max(adjustment.lower());
        adjustment.set_value(dragged);
        pump(500);
        assert!(
            (adjustment.value() - dragged).abs() < 1e-7,
            "scrollbar interrupts pending page"
        );
        terminal.feed(b"\x1b[?1049h");
        pump(120);
        assert!(
            !native_history_available(&terminal),
            "alternate screen has no main history range"
        );
        assert!(!page(), "alternate-screen editor owns its Page keys");
        terminal.feed(b"\x1b[?1049l");
        pump(120);
        assert!(native_history_available(&terminal));
        window.close();
        pump(30);
    }
}
