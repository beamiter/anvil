//! Chat is a layout of the existing terminal, never a second editor or PTY.
use super::{ActiveBlock, BlockState};
use gtk::glib;
use gtk::prelude::*;
use jterm_core::chat_mode::{resolve_surface, ChatSessionState, ChatSurface};
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::{Terminal, TerminalExt};

pub(super) const CONTROLS_HEIGHT: i32 = 28;

/// Keep room for the companion while letting wrapped input and completion
/// menus grow through the same shell-owned measurements as Block mode.
pub(super) fn composer_rows(input: i64, drawn: i64, viewport: i64) -> i64 {
    input.max(drawn).clamp(1, viewport.max(1))
}

/// Rows the Block composer always keeps when idle: a prompt plus one command
/// line leaves no visible room to type into, so the editor reserves three full
/// lines of text (fewer only when the whole pane is shorter than that).
pub(super) const BLOCK_INPUT_MIN_ROWS: i64 = 3;

/// Block's docked input: the same shell-owned growth as [`composer_rows`],
/// never below [`BLOCK_INPUT_MIN_ROWS`]. Chat keeps its one-row composer.
pub(super) fn block_composer_rows(input: i64, drawn: i64, viewport: i64) -> i64 {
    let viewport = viewport.max(1);
    input
        .max(drawn)
        .clamp(BLOCK_INPUT_MIN_ROWS.min(viewport), viewport)
}

/// Read the lifecycle owner's *current* state. In particular, an OSC marker
/// seen earlier in this session is not evidence of a ready prompt now.
pub(super) fn surface(state: BlockState) -> ChatSurface {
    resolve_surface(ChatSessionState {
        shell_integration: matches!(
            state,
            BlockState::AwaitingCommand | BlockState::CollectingOutput
        ),
        prompt_ready: state == BlockState::AwaitingCommand,
        command_running: state == BlockState::CollectingOutput,
        alternate_screen: state == BlockState::AltScreen,
    })
}

/// Presentation continuity never grants terminal input authority.
fn presentation_surface(previous: ChatSurface, state: BlockState) -> ChatSurface {
    // A confirmed prompt is sufficient: a fast C/D pair can be coalesced
    // before any layout gets to display Running. Native ownership states
    // always go through the real-state resolver and retire this continuity.
    if matches!(previous, ChatSurface::Prompt | ChatSurface::Running) {
        match state {
            BlockState::PostCommand => return ChatSurface::Running,
            BlockState::CollectingPrompt => return ChatSurface::Prompt,
            _ => {}
        }
    }
    surface(state)
}

/// A docked zero-height insertion anchor sits exactly at the history's bottom
/// edge. The original Block test deliberately requires the top of a real live
/// card inside the viewport; applying that strict inset to a point locks Chat
/// in a false "scrolled up" state after every completion.
pub(super) fn anchor_at_bottom(y: f64, viewport_height: f64, docked: bool) -> bool {
    if docked {
        y <= viewport_height + 1.0
    } else {
        y < viewport_height - 4.0
    }
}

// Measured control chrome; never derive the PTY budget from history allocation.
fn footer_height(widget: &gtk::Widget) -> Option<i32> {
    if widget.has_css_class("chat-controls") {
        return Some(
            widget
                .measure(gtk::Orientation::Vertical, -1)
                .1
                .max(CONTROLS_HEIGHT),
        );
    }
    // The composer is last, so do not scan finished history cards to find it.
    let mut child = widget.last_child();
    while let Some(current) = child {
        if let Some(height) = footer_height(&current) {
            return Some(height);
        }
        child = current.prev_sibling();
    }
    None
}

/// Ascend from the history scroll, which never moves, rather than from the
/// terminal, whose clip moves between transcript and composer. The canvas is
/// stable while the composer changes height; using history page_size here
/// would repeatedly shrink the PTY as each new composer allocation settles.
pub(super) fn canvas_height(scroll: &gtk::ScrolledWindow) -> Option<i32> {
    let mut widget = scroll.parent();
    while let Some(current) = widget {
        if current.has_css_class("chat-canvas") {
            let chrome = if current.has_css_class("chat-native-surface") {
                0
            } else {
                // Reserve the same measured footer and history minimum in
                // Prompt and Running. GtkBox must still allocate the scroller's
                // minimum (including scrollbar chrome) when the editor is full.
                // These are size requests, never the shrinking history page_size.
                footer_height(&current).unwrap_or(CONTROLS_HEIGHT)
                    + 14
                    + scroll.measure(gtk::Orientation::Vertical, -1).0
            };
            return Some((current.height() - chrome).max(1));
        }
        widget = current.parent();
    }
    None
}

/// One real CommandStart callback advances this identity, including two starts
/// received in one held byte batch. This is deliberately independent of UI ticks.
#[derive(Default)]
pub(super) struct RunningCommand {
    presentation: Cell<Option<ChatSurface>>,
    generation: Cell<u64>,
    displayed: Cell<Option<u64>>,
    started: Cell<Option<std::time::Instant>>,
}

impl RunningCommand {
    fn presentation_surface(&self, state: BlockState) -> ChatSurface {
        let target = presentation_surface(
            self.presentation.get().unwrap_or(ChatSurface::Passthrough),
            state,
        );
        self.presentation.set(Some(target));
        target
    }

    pub(super) fn reset_presentation(&self) {
        self.presentation.set(None);
        self.displayed.set(None);
    }

    pub(super) fn started(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.started.set(Some(std::time::Instant::now()));
    }

    pub(super) fn update(&self, state: BlockState, hint: &gtk::Label) {
        // Only a real running command can be the target of Stop.
        self.displayed
            .set((state == BlockState::CollectingOutput).then(|| self.generation.get()));
        // The compact footer may ellipsize in narrow panes. Keep its complete
        // plain-text hint discoverable without adding another row or changing
        // terminal allocation.
        if hint.width_chars() != 1 {
            hint.set_width_chars(1);
        }
        if self.presentation_surface(state) == ChatSurface::Running {
            let seconds = self
                .started
                .get()
                .map(|start| start.elapsed().as_secs())
                .unwrap_or(0);
            hint.set_text(&format!(
                "Running · {}",
                super::unified_chrome::format_elapsed_clock(seconds)
            ));
        } else {
            self.displayed.set(None);
            hint.set_text("Shell · Tab completes · ↑ history");
        }
        if hint.tooltip_text().as_deref() != Some(hint.text().as_str()) {
            hint.set_tooltip_text(Some(hint.text().as_str()));
        }
    }
}

pub(super) struct LayoutWidgets<'a> {
    pub transcript: &'a gtk::Overlay,
    pub dock: &'a gtk::Box,
    pub controls: &'a gtk::Box,
    pub frame: &'a gtk::Overlay,
    pub prompt_scroll: &'a gtk::ScrolledWindow,
    pub send: &'a gtk::Button,
    pub stop: &'a gtk::Button,
    pub hint: &'a gtk::Label,
    pub running: &'a Rc<RunningCommand>,
}

/// The holder stays in block_list forever: finished cards and notices insert
/// before it and scroll anchoring measures it. Move only its internal clip.
pub(super) fn layout(
    active: &Rc<RefCell<ActiveBlock>>,
    state: &Rc<Cell<BlockState>>,
    widgets: LayoutWidgets<'_>,
    block_layout: Rc<dyn Fn()>,
) -> Rc<dyn Fn()> {
    let active = Rc::downgrade(active);
    let state = state.clone();
    let transcript = widgets.transcript.downgrade();
    let dock = widgets.dock.downgrade();
    let controls = widgets.controls.downgrade();
    let frame = widgets.frame.downgrade();
    let prompt_scroll = widgets.prompt_scroll.downgrade();
    let send = widgets.send.downgrade();
    let stop = widgets.stop.downgrade();
    let hint = widgets.hint.downgrade();
    let running = widgets.running.clone();
    Rc::new(move || {
        let (
            Some(active),
            Some(transcript),
            Some(dock),
            Some(controls),
            Some(frame),
            Some(prompt_scroll),
            Some(send),
            Some(stop),
            Some(hint),
        ) = (
            active.upgrade(),
            transcript.upgrade(),
            dock.upgrade(),
            controls.upgrade(),
            frame.upgrade(),
            prompt_scroll.upgrade(),
            send.upgrade(),
            stop.upgrade(),
            hint.upgrade(),
        )
        else {
            return;
        };
        let target = running.presentation_surface(state.get());
        let (holder, clip, terminal) = {
            let active = active.borrow();
            (
                active.widget().clone(),
                active.live_clip(),
                active.active_vte.clone(),
            )
        };
        let in_transcript = target == ChatSurface::Running;
        if !in_transcript {
            if let Some(header) = holder
                .first_child()
                .filter(|child| child.has_css_class("block-reference-header"))
            {
                header.set_visible(false);
            }
        }
        let prompt = target.composer_docked();
        let native = matches!(target, ChatSurface::Passthrough | ChatSurface::Fullscreen);
        let Some(canvas) = frame.child().and_downcast::<gtk::Box>() else {
            return;
        };
        let focus_root = terminal.root();
        let focused = focus_root
            .as_ref()
            .and_then(|root| root.focus())
            .is_some_and(|owner| owner == *terminal.upcast_ref::<gtk::Widget>());
        let destination = if in_transcript { &holder } else { &dock };
        let moving_dock = native != (dock.parent().as_ref() == Some(frame.upcast_ref()));
        let moving_clip = clip.parent().as_ref() != Some(destination.upcast_ref());
        // VTE rebinds its IM client during reparenting. A logically focused
        // terminal can otherwise keep GTK focus without giving the rebound
        // fcitx context a fresh focus-in. End that ownership before unrooting,
        // then restore it once the new host is visible. Background peers keep
        // their focus, including when this window is inactive.
        if focused && (moving_dock || moving_clip) {
            if let Some(root) = &focus_root {
                root.set_focus(gtk::Widget::NONE);
            }
        }
        let mut moved = false;
        // Native takeover must not make its old grid the window's minimum size.
        // Prompt's External scroller likewise has a bounded minimum, so a long
        // multiline editor can shrink before the resize observer updates rows.
        if native && dock.parent().as_ref() != Some(frame.upcast_ref()) {
            prompt_scroll.set_child(gtk::Widget::NONE);
            frame.add_overlay(&dock);
            frame.set_measure_overlay(&dock, false);
            frame.set_clip_overlay(&dock, true);
            moved = true;
        } else if !native && dock.parent().as_ref() == Some(frame.upcast_ref()) {
            frame.remove_overlay(&dock);
            prompt_scroll.set_child(Some(&dock));
            moved = true;
        }
        if clip.parent().as_ref() != Some(destination.upcast_ref()) {
            if let Some(parent) = clip.parent().and_downcast::<gtk::Box>() {
                parent.remove(&clip);
            }
            if in_transcript {
                if let Some(header) = holder
                    .first_child()
                    .filter(|child| child.has_css_class("block-reference-header"))
                {
                    destination.insert_child_after(&clip, Some(&header));
                } else {
                    destination.prepend(&clip);
                }
            } else {
                destination.prepend(&clip);
            }
            moved = true;
        }
        if native {
            canvas.add_css_class("chat-native-surface");
        } else {
            canvas.remove_css_class("chat-native-surface");
        }
        transcript.set_visible(!native);
        prompt_scroll.set_visible(!native);
        dock.set_visible(true);
        dock.set_vexpand(native);
        controls.set_visible(prompt || in_transcript);
        send.set_visible(prompt);
        stop.set_visible(in_transcript);
        send.set_sensitive(state.get() == BlockState::AwaitingCommand);
        stop.set_sensitive(state.get() == BlockState::CollectingOutput);
        running.update(state.get(), &hint);
        controls.set_tooltip_text(Some(hint.text().as_str()));
        if prompt || in_transcript {
            // A running command changes terminal ownership, not the shared
            // minimum input/tool frame or its viewport insets.
            dock.add_css_class("chat-composer");
        } else {
            dock.remove_css_class("chat-composer");
        }
        if in_transcript {
            holder.remove_css_class("chat-empty-anchor");
        } else {
            holder.add_css_class("chat-empty-anchor");
        }
        block_layout();
        if native {
            if let Some(scroll) = transcript.child().and_downcast::<gtk::ScrolledWindow>() {
                if let Some(rows) = super::viewport_rows_for(&terminal, &scroll) {
                    terminal.set_size(terminal.column_count().max(1), rows);
                    active
                        .borrow()
                        .set_live_geometry(terminal.char_height() as i32, rows, rows);
                }
            }
        }
        if !in_transcript {
            holder.set_height_request(0);
        }
        // Restore only the focus this pane owned, after the new host is visible.
        if moved && focused {
            terminal.grab_focus();
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StopIntent {
    generation: u64,
    foreground: i32,
}

fn stop_intent(
    state: BlockState,
    running: &RunningCommand,
    pty: &crate::pty::OwnedPty,
) -> Option<StopIntent> {
    if surface(state) != ChatSurface::Running {
        return None;
    }
    let generation = running.displayed.get()?;
    (generation == running.generation.get()).then_some(StopIntent {
        generation,
        foreground: pty.foreground_group()?,
    })
}

/// GTK coalesces repeated native activations while its release animation is
/// pending. Preserve the first target, including an invalid target tombstone.
struct NativeActivation<T: Copy> {
    pending: Cell<Option<Option<T>>>,
    epoch: Cell<u64>,
}

impl<T: Copy> Default for NativeActivation<T> {
    fn default() -> Self {
        Self {
            pending: Cell::new(None),
            epoch: Cell::new(0),
        }
    }
}

impl<T: Copy> NativeActivation<T> {
    fn begin(&self, intent: Option<T>) {
        if self.pending.get().is_none() {
            self.epoch.set(self.epoch.get().wrapping_add(1));
            self.pending.set(Some(intent));
        }
    }

    fn invalidate(&self) {
        if self.pending.get().is_some() {
            self.pending.set(Some(None));
        }
    }

    fn retire(&self, epoch: u64) {
        if self.epoch.get() == epoch {
            self.pending.set(None);
        }
    }

    fn take(&self, pointer: &Cell<Option<Option<T>>>) -> Option<T> {
        // A native timeout's clicked must never consume a newer mouse press.
        let pointer = pointer.take();
        self.pending.take().or(pointer).flatten()
    }
}

struct CapturedActivation<T: Copy> {
    native: NativeActivation<T>,
    pointer: Cell<Option<Option<T>>>,
    press_generation: Cell<u64>,
}

/// Chat-only activation ledger. GTK's native keyboard/AT animation and mouse
/// press are distinct origins; clicked can consume one, never invent a target.
fn capture_activation<T: Copy + 'static>(
    button: &gtk::Button,
    capture: impl Fn() -> Option<T> + 'static,
) -> Rc<CapturedActivation<T>> {
    button.add_css_class("chat-command-control");
    let activation = Rc::new(CapturedActivation {
        native: NativeActivation::default(),
        pointer: Cell::new(None),
        press_generation: Cell::new(0),
    });
    let capture = Rc::new(capture);
    {
        let activation = activation.clone();
        let capture = capture.clone();
        // GTK's RUN_FIRST handler schedules the animation before this callback,
        // but clicked cannot run until this synchronous signal returns.
        button.connect_activate(move |button| {
            if button.has_css_class("keyboard-activating") {
                activation.native.begin(
                    (button.is_mapped() && button.is_sensitive())
                        .then(|| capture())
                        .flatten(),
                );
            }
        });
    }
    {
        let activation = activation.clone();
        // GTK removes this documented class before synchronous clicked, or
        // without clicked when activation is cancelled/unrealized. Retire on
        // idle, never early enough to make a delayed click look like new input.
        button.connect_css_classes_notify(move |button| {
            if !button.has_css_class("keyboard-activating") {
                let epoch = activation.native.epoch.get();
                let activation = activation.clone();
                let button = button.downgrade();
                glib::idle_add_local_once(move || {
                    if button
                        .upgrade()
                        .is_none_or(|button| !button.has_css_class("keyboard-activating"))
                    {
                        activation.native.retire(epoch);
                    }
                });
            }
        });
    }
    {
        let activation = activation.clone();
        button.connect_unmap(move |_| {
            activation.native.invalidate();
            activation.pointer.set(None);
        });
    }
    {
        let activation = activation.clone();
        button.connect_state_flags_changed(move |button, _| {
            if !button.is_sensitive() {
                activation.native.invalidate();
                activation.pointer.set(None);
            }
        });
    }
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gtk::gdk::BUTTON_PRIMARY);
    gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
    let press = activation.clone();
    gesture.connect_pressed(move |_, _, _, _| {
        press
            .press_generation
            .set(press.press_generation.get().wrapping_add(1));
        press.pointer.set(Some(capture()));
    });
    let cancelled = activation.clone();
    gesture.connect_cancel(move |_, _| {
        cancelled.pointer.set(None);
        cancelled.native.invalidate();
    });
    let released = activation.clone();
    gesture.connect_released(move |_, _, _, _| {
        let released = released.clone();
        let generation = released.press_generation.get();
        glib::idle_add_local_once(move || {
            // An earlier release's idle cleanup cannot erase a newer press.
            if released.press_generation.get() == generation {
                released.pointer.set(None);
            }
        });
    });
    button.add_controller(gesture);
    activation
}

/// Shared by Chat's footer and the existing sticky/pill interrupts. A mouse
/// press binds to the command that was actually displayed, not a later release.
/// Keyboard/accessibility activation binds at native `activate`, before GTK's
/// delayed `clicked`, rather than rebinding to the command displayed later.
pub(super) fn connect_stop(
    button: &gtk::Button,
    state: &Rc<Cell<BlockState>>,
    running: &Rc<RunningCommand>,
    pty: &Rc<crate::pty::OwnedPty>,
    hold: &Rc<super::SelectionFeedHold>,
    human_input: &super::HumanInputCallbacks,
    protocol_ready: Rc<dyn Fn() -> bool>,
) {
    let activation = {
        let (state, running, pty) = (state.clone(), running.clone(), Rc::downgrade(pty));
        capture_activation(button, move || {
            pty.upgrade()
                .and_then(|pty| stop_intent(state.get(), &running, &pty))
        })
    };
    let (state, running, pty, hold, human_input) = (
        state.clone(),
        running.clone(),
        Rc::downgrade(pty),
        hold.clone(),
        human_input.clone(),
    );
    button.connect_clicked(move |button| {
        let before = activation.native.take(&activation.pointer);
        if before.is_none() || !button.is_mapped() || !button.is_sensitive() {
            return;
        }
        let Some(pty) = pty.upgrade() else {
            return;
        };
        hold.flush_now();
        if before.is_none() || before != stop_intent(state.get(), &running, &pty) {
            return;
        }
        let expected = before.expect("validated Stop identity");
        if pty
            .try_dispatch_chat_byte(3, expected.foreground, false, || {
                protocol_ready()
                    && surface(state.get()) == ChatSurface::Running
                    && running.displayed.get() == Some(expected.generation)
                    && running.generation.get() == expected.generation
            })
            .is_ok()
        {
            super::emit_human_input(&human_input, super::HumanInputKind::StickyStop);
        }
        // A successful syscall sends one interrupt; it does not claim exit.
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SendIntent {
    pub generation: u64,
    pub foreground: i32,
}

pub(super) fn connect_submit(
    button: &gtk::Button,
    state: &Rc<Cell<BlockState>>,
    terminal: &Terminal,
    pty: &Rc<crate::pty::OwnedPty>,
    hold: &Rc<super::SelectionFeedHold>,
    generation: &Rc<Cell<u64>>,
    pending: &Rc<Cell<Option<SendIntent>>>,
) {
    let activation = {
        let (state, generation, pty) = (state.clone(), generation.clone(), Rc::downgrade(pty));
        capture_activation(button, move || {
            let pty = pty.upgrade()?;
            if super::ime_gate::blocked() || !submit_allowed(state.get(), pty.shell_is_foreground())
            {
                return None;
            }
            Some(SendIntent {
                generation: generation.get(),
                foreground: pty.foreground_group()?,
            })
        })
    };
    let notify = super::paste_feedback::notifier(terminal);
    button.set_tooltip_text(Some(
        "Send Enter to the live shell. Finish or cancel text composition before using Send.",
    ));
    let (state, terminal, pty, hold, generation, pending) = (
        state.clone(),
        terminal.downgrade(),
        Rc::downgrade(pty),
        hold.clone(),
        generation.clone(),
        pending.clone(),
    );
    button.connect_clicked(move |button| {
        let intent = activation.native.take(&activation.pointer);
        if super::ime_gate::blocked() {
            notify(super::ime_gate::notice());
            return;
        }
        let Some(intent) = intent else {
            return;
        };
        if !button.is_mapped() || !button.is_sensitive() {
            return;
        }
        let Some(pty) = pty.upgrade() else {
            return;
        };
        hold.flush_now();
        if super::ime_gate::blocked()
            || !submit_allowed(state.get(), pty.shell_is_foreground())
            || generation.get() != intent.generation
            || pty.foreground_group() != Some(intent.foreground)
        {
            return;
        }
        let Some(terminal) = terminal.upgrade() else {
            return;
        };
        // The synchronous native commit retains this target through its actual
        // transport boundary; it cannot fall back to the unguarded FIFO.
        pending.set(Some(intent));
        terminal.emit_by_name::<()>("commit", &[&"\r", &1_u32]);
        pending.set(None);
    });
}

fn submit_allowed(state: BlockState, shell_foreground: Option<bool>) -> bool {
    surface(state).allows_submit_affordance() && shell_foreground == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_input_rows_keep_three_lines_and_chat_keeps_one() {
        // An empty prompt, a one-line prompt and a two-line prompt all leave
        // three full lines of room to type into in Block.
        assert_eq!(block_composer_rows(1, 1, 30), BLOCK_INPUT_MIN_ROWS);
        assert_eq!(block_composer_rows(1, 2, 30), BLOCK_INPUT_MIN_ROWS);
        assert_eq!(block_composer_rows(2, 1, 30), BLOCK_INPUT_MIN_ROWS);
        // Beyond the floor the editor follows typed and drawn content.
        assert_eq!(block_composer_rows(4, 1, 30), 4);
        assert_eq!(block_composer_rows(1, 12, 30), 12);
        assert_eq!(block_composer_rows(1, 100, 30), 30);
        // A pane shorter than the floor never asks for more than it has.
        assert_eq!(block_composer_rows(1, 1, 2), 2);
        assert_eq!(block_composer_rows(1, 1, 0), 1);
        // Chat's companion composer is unchanged.
        assert_eq!(composer_rows(1, 1, 30), 1);
        assert_eq!(composer_rows(1, 2, 30), 2);
    }

    #[test]
    fn completion_presentation_survives_normal_and_coalesced_commands() {
        use BlockState::*;
        use ChatSurface::*;
        for states in [
            vec![
                AwaitingCommand,
                CollectingOutput,
                PostCommand,
                CollectingPrompt,
                AwaitingCommand,
            ],
            // A fast command can start and end between two layout callbacks.
            vec![
                AwaitingCommand,
                PostCommand,
                CollectingPrompt,
                AwaitingCommand,
            ],
            vec![AwaitingCommand, CollectingPrompt, AwaitingCommand],
        ] {
            let mut previous = Passthrough;
            for state in states {
                previous = presentation_surface(previous, state);
                assert_eq!(
                    previous,
                    match state {
                        CollectingOutput | PostCommand => Running,
                        _ => Prompt,
                    },
                    "{state:?} must not hand the viewport to native VTE"
                );
                if matches!(state, PostCommand | CollectingPrompt) {
                    assert!(!surface(state).allows_submit_affordance());
                }
            }
        }
    }

    #[test]
    fn completion_presentation_starts_native_and_retires_on_takeover() {
        use BlockState::*;
        use ChatSurface::*;
        let mut previous = Passthrough;
        for state in [Idle, CollectingPrompt, CollectingPrompt, PostCommand] {
            previous = presentation_surface(previous, state);
            assert_eq!(
                previous, Passthrough,
                "no confirmed Chat interaction: {state:?}"
            );
        }
        for held in [Prompt, Running] {
            for state in [Idle, RawFallback, AltScreen] {
                previous = held;
                previous = presentation_surface(previous, state);
                assert_eq!(
                    previous,
                    if state == AltScreen {
                        Fullscreen
                    } else {
                        Passthrough
                    }
                );
                let state = CollectingPrompt;
                assert_eq!(
                    presentation_surface(previous, state),
                    Passthrough,
                    "native takeover retires the presentation hold"
                );
            }
        }
    }
    #[test]
    fn completion_presentation_reset_clears_hold_without_granting_controls() {
        let running = RunningCommand::default();
        let pty = crate::pty::OwnedPty::from_openpty(Some(true)).unwrap();
        pty.set_test_foreground_group(Some(101));
        running.started();
        running.displayed.set(Some(running.generation.get()));
        assert_eq!(
            running.presentation_surface(BlockState::AwaitingCommand),
            ChatSurface::Prompt
        );
        assert_eq!(
            running.presentation_surface(BlockState::PostCommand),
            ChatSurface::Running
        );
        assert!(stop_intent(BlockState::PostCommand, &running, &pty).is_none());
        assert_eq!(
            running.presentation_surface(BlockState::CollectingPrompt),
            ChatSurface::Prompt
        );
        assert!(!submit_allowed(BlockState::CollectingPrompt, Some(true)));
        running.reset_presentation();
        assert!(running.displayed.get().is_none());
        assert_eq!(
            running.presentation_surface(BlockState::CollectingPrompt),
            ChatSurface::Passthrough
        );
    }

    #[test]
    fn native_stop_activation_keeps_first_target_and_rejects_unarmed_clicks() {
        let native = NativeActivation::default();
        let pointer = Cell::new(None);
        let old = StopIntent {
            generation: 1,
            foreground: 101,
        };
        let new = StopIntent {
            generation: 2,
            foreground: 202,
        };
        assert_eq!(native.take(&pointer), None);
        native.begin(Some(old));
        native.begin(Some(new));
        assert_eq!(native.take(&pointer), Some(old));
        assert_eq!(
            native.take(&pointer),
            None,
            "duplicate clicked is not a new activation"
        );
        native.begin(None);
        native.begin(Some(new));
        assert_eq!(
            native.take(&pointer),
            None,
            "invalid first intent cannot be rebound"
        );
        native.begin(Some(new));
        assert_eq!(native.take(&pointer), Some(new));
    }

    #[test]
    fn native_stop_tombstones_and_idle_retirement_do_not_rebind_new_input() {
        let native = NativeActivation::default();
        let old = StopIntent {
            generation: 1,
            foreground: 101,
        };
        let new = StopIntent {
            generation: 2,
            foreground: 202,
        };
        let pointer = Cell::new(Some(Some(new)));
        native.begin(Some(old));
        assert_eq!(
            native.take(&pointer),
            Some(old),
            "native timeout cannot borrow a new pointer target"
        );
        assert!(pointer.get().is_none());
        native.begin(Some(old));
        let old_epoch = native.epoch.get();
        native.invalidate();
        native.begin(Some(new));
        pointer.set(Some(Some(new)));
        assert_eq!(
            native.take(&pointer),
            None,
            "cancelled native intent dominates pointer fallback"
        );
        native.begin(Some(new));
        native.retire(old_epoch);
        assert_eq!(
            native.take(&pointer),
            Some(new),
            "old cancellation idle cannot clear fresh activation"
        );
        native.begin(Some(old));
        native.invalidate();
        native.retire(native.epoch.get());
        native.begin(Some(new));
        assert_eq!(
            native.take(&pointer),
            Some(new),
            "cancel without clicked recovers after retirement"
        );
        pointer.set(Some(Some(new)));
        native.retire(native.epoch.get());
        assert_eq!(
            native.take(&pointer),
            Some(new),
            "native cleanup never clears a new mouse press"
        );
    }

    #[test]
    fn native_send_activation_keeps_its_original_prompt_through_repeat_and_cancel() {
        let native = NativeActivation::default();
        let pointer = Cell::new(None);
        let old = SendIntent {
            generation: 10,
            foreground: 101,
        };
        let new = SendIntent {
            generation: 11,
            foreground: 101,
        };
        native.begin(Some(old));
        native.begin(Some(new));
        assert_eq!(native.take(&pointer), Some(old));
        native.begin(Some(old));
        native.invalidate();
        native.begin(Some(new));
        assert_eq!(native.take(&pointer), None);
        native.begin(Some(new));
        assert_eq!(native.take(&pointer), Some(new));
        assert_eq!(native.take(&pointer), None);
    }

    #[test]
    fn docked_anchor_at_bottom_does_not_create_a_false_scroll_lock() {
        assert!(anchor_at_bottom(400.0, 400.0, true));
        assert!(anchor_at_bottom(400.5, 400.0, true));
        assert!(!anchor_at_bottom(410.0, 400.0, true));
        assert!(!anchor_at_bottom(400.0, 400.0, false));
        assert!(anchor_at_bottom(390.0, 400.0, false));
    }

    #[test]
    fn current_lifecycle_owns_chat_surface() {
        for state in [
            BlockState::Idle,
            BlockState::CollectingPrompt,
            BlockState::RawFallback,
            BlockState::PostCommand,
        ] {
            assert_eq!(surface(state), ChatSurface::Passthrough);
            assert!(!surface(state).allows_submit_affordance());
        }
        assert_eq!(surface(BlockState::AwaitingCommand), ChatSurface::Prompt);
        assert_eq!(surface(BlockState::CollectingOutput), ChatSurface::Running);
        assert_eq!(surface(BlockState::AltScreen), ChatSurface::Fullscreen);
    }
    #[test]
    fn command_and_alt_screen_roundtrip_restores_only_a_valid_prompt() {
        let states = [
            BlockState::Idle,
            BlockState::CollectingPrompt,
            BlockState::AwaitingCommand,
            BlockState::CollectingOutput,
            BlockState::AltScreen,
            BlockState::CollectingOutput,
            BlockState::PostCommand,
            BlockState::CollectingPrompt,
            BlockState::AwaitingCommand,
            BlockState::RawFallback,
        ];
        for (index, state) in states.into_iter().enumerate() {
            assert_eq!(surface(state).composer_docked(), index == 2 || index == 8);
        }
    }
    #[test]
    fn submit_rechecks_held_boundaries_and_foreground_ownership() {
        let state = Rc::new(Cell::new(BlockState::AwaitingCommand));
        let hold = super::super::SelectionFeedHold::new();
        let state_for_flush = state.clone();
        hold.set_flush(move |_| state_for_flush.set(BlockState::CollectingOutput));
        hold.begin_drag();
        assert!(hold.try_buffer(b"\x1b]133;C\x07"));
        assert!(submit_allowed(state.get(), Some(true)));
        hold.flush_now();
        assert!(!submit_allowed(state.get(), Some(true)));
        state.set(BlockState::AwaitingCommand);
        assert!(!submit_allowed(state.get(), Some(false)));
        assert!(!submit_allowed(state.get(), None));
        for state in [
            BlockState::AltScreen,
            BlockState::RawFallback,
            BlockState::PostCommand,
        ] {
            assert!(!submit_allowed(state, Some(true)));
        }
    }

    #[test]
    fn stop_intent_requires_current_display_identity_and_known_foreground() {
        let pty = crate::pty::OwnedPty::from_openpty(Some(true)).unwrap();
        let running = RunningCommand::default();
        running.started();
        running.displayed.set(Some(running.generation.get()));
        assert!(stop_intent(BlockState::CollectingOutput, &running, &pty).is_none());
        pty.set_test_foreground_group(Some(101));
        let original = stop_intent(BlockState::CollectingOutput, &running, &pty).unwrap();
        running.started();
        assert!(stop_intent(BlockState::CollectingOutput, &running, &pty).is_none());
        running.displayed.set(Some(running.generation.get()));
        let next = stop_intent(BlockState::CollectingOutput, &running, &pty).unwrap();
        assert_ne!(original, next);
        pty.set_test_foreground_group(Some(202));
        assert_ne!(
            Some(next),
            stop_intent(BlockState::CollectingOutput, &running, &pty)
        );
        for state in [
            BlockState::AwaitingCommand,
            BlockState::AltScreen,
            BlockState::RawFallback,
            BlockState::PostCommand,
        ] {
            assert!(stop_intent(state, &running, &pty).is_none());
        }
    }

    use super::super::TermView;
    use std::time::{Duration, Instant};

    const PROMPT: &[u8] = b"\x1b]133;A\x07$ \x1b]133;B\x07";
    const START: &[u8] = b"echo hello\r\n\x1b]133;C;cmdline_url=echo%20hello\x07hello\r\n";
    const NEXT: &[u8] = b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07";

    fn pump_for(duration: Duration) {
        let deadline = Instant::now() + duration;
        let context = glib::MainContext::default();
        while Instant::now() < deadline {
            for _ in 0..100 {
                if !context.iteration(false) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn wait_for(message: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < deadline, "{message}");
            pump_for(Duration::from_millis(10));
        }
    }

    fn fixture() -> (Rc<TermView>, gtk::Window, gtk::Entry) {
        fixture_for_mode(crate::config::TerminalMode::Chat)
    }

    fn fixture_for_mode(
        mode: crate::config::TerminalMode,
    ) -> (Rc<TermView>, gtk::Window, gtk::Entry) {
        // Freeze the process environment before GTK, not once per window.
        // The refusal test creates separate full-queue and Closed PTYs in the
        // same process; neither fixture may recapture toolkit-mutated state.
        static CHILD_ENV: std::sync::Once = std::sync::Once::new();
        CHILD_ENV.call_once(|| {
            crate::child_env::capture_inherited_environment().expect("test child environment");
        });
        gtk::init().expect("GTK display");
        crate::workspace_shell::install_css();
        let view: Rc<TermView> = TermView::new_with_spawner(
            &crate::config::Config::safe_defaults(),
            &mode,
            &["sh".to_string()],
            None,
            false,
            None,
            "chat-display-test",
            &[],
            &[],
            |_, _, _, _| crate::pty::OwnedPty::from_openpty(Some(true)),
        )
        .expect("nonexecuting PTY recorder")
        .into();
        view.pty.set_test_foreground_group(Some(101));
        view.block_onboarding.history_resolved(false);
        let entry = gtk::Entry::new();
        entry.set_text("other pane retains focus");
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 0);
        box_.add_css_class("workspace-center");
        if mode == crate::config::TerminalMode::Block {
            box_.add_css_class("block-workspace");
        }
        box_.append(&entry);
        box_.append(&view.root);
        let window = gtk::Window::builder()
            .default_width(640)
            .default_height(480)
            .build();
        window.set_child(Some(&box_));
        window.present();
        wait_for("real window is allocated", || {
            view.root.height() > 100 && view.active_vte.is_mapped()
        });
        pump_for(Duration::from_millis(180));
        (view, window, entry)
    }

    fn owns_focus(widget: &impl IsA<gtk::Widget>) -> bool {
        let widget = widget.upcast_ref::<gtk::Widget>();
        widget
            .root()
            .and_then(|root| root.focus())
            .is_some_and(|focused| focused == *widget || focused.is_ancestor(widget))
    }

    fn widget_with_class(root: &impl IsA<gtk::Widget>, class: &str) -> gtk::Widget {
        fn find(widget: &gtk::Widget, class: &str) -> Option<gtk::Widget> {
            if widget.has_css_class(class) {
                return Some(widget.clone());
            }
            let mut child = widget.first_child();
            while let Some(current) = child {
                if let Some(found) = find(&current, class) {
                    return Some(found);
                }
                child = current.next_sibling();
            }
            None
        }
        find(root.upcast_ref(), class)
            .unwrap_or_else(|| panic!("missing production widget {class}"))
    }

    fn button(view: &TermView, class: &str) -> gtk::Button {
        widget_with_class(&view.root, class)
            .downcast()
            .expect("production button")
    }

    fn drive(view: &TermView, bytes: &[u8], state: BlockState) {
        view.pty.emit_test_output(bytes);
        wait_for("production parser reached lifecycle state", || {
            view.bstate.get() == state
        });
        pump_for(Duration::from_millis(180));
        wait_for("resize tick settled", || {
            view.resize_tick_id.borrow().is_none()
        });
    }

    /// No explicit layout/sync and no PTY output between set_default_size and
    /// ioctl: this proves the real allocation observer re-arms an idle tick.
    fn height_roundtrip(view: &TermView, window: &gtk::Window, native: bool) {
        wait_for("idle before height-only resize", || {
            view.resize_tick_id.borrow().is_none()
        });
        let identity = view.active_vte.clone();
        let fd = view.pty.master_fd_raw();
        let base = view.pty.test_slave_winsize();
        assert!(base.0 > 10 && base.1 > 3);
        let height = window.height();
        window.set_default_size(640, height + 160);
        wait_for("height-only grow reached real PTY", || {
            view.pty.test_slave_winsize().1 > base.1
        });
        pump_for(Duration::from_millis(180));
        wait_for("grow settled", || view.resize_tick_id.borrow().is_none());
        assert_eq!(view.pty.test_slave_winsize().0, base.0);
        if native {
            assert_eq!(
                view.active_vte.row_count(),
                i64::from(view.pty.test_slave_winsize().1)
            );
        }
        window.set_default_size(640, height);
        wait_for(
            "height-only shrink reached original allocation and PTY",
            || window.height() == height && view.pty.test_slave_winsize() == base,
        );
        pump_for(Duration::from_millis(180));
        assert_eq!(view.active_vte, identity);
        assert_eq!(view.pty.master_fd_raw(), fd);
        if native {
            assert_eq!(view.active_vte.row_count(), i64::from(base.1));
        }
    }

    fn assert_footer_inside_pane(view: &TermView, control: &gtk::Button) {
        let bounds = control
            .compute_bounds(&view.root)
            .expect("mapped footer allocation");
        assert!(bounds.height() > 0.0 && bounds.y() >= 0.0);
        assert!(
            bounds.y() + bounds.height() <= view.root.height() as f32 + 1.0,
            "footer must stay visible even when the editor fills the pane"
        );
    }

    fn emit_primary_capture(widget: &impl IsA<gtk::Widget>, signal: &str, x: f64, y: f64) {
        let controllers = widget.observe_controllers();
        let mut count = 0;
        for index in 0..controllers.n_items() {
            if let Some(click) = controllers.item(index).and_downcast::<gtk::GestureClick>() {
                if click.propagation_phase() == gtk::PropagationPhase::Capture
                    && click.button() == gtk::gdk::BUTTON_PRIMARY
                {
                    click.emit_by_name::<()>(signal, &[&1_i32, &x, &y]);
                    count += 1;
                }
            }
        }
        assert!(count > 0, "production capture owner must be installed");
    }

    fn press_capture(widget: &impl IsA<gtk::Widget>, x: f64, y: f64) {
        emit_primary_capture(widget, "pressed", x, y);
    }

    fn release_capture(widget: &impl IsA<gtk::Widget>, x: f64, y: f64) {
        // GtkButton's own PRIMARY/Capture gesture participates too: an inside
        // release synchronously emits clicked, just as a real release would.
        emit_primary_capture(widget, "released", x, y);
    }

    fn assert_finished_long_output(view: &TermView, expected_count: usize) {
        {
            let records = view.block_data.borrow();
            assert_eq!(
                records.len(),
                expected_count,
                "one retained record per command"
            );
            let record = records.back().expect("completed command record");
            assert_eq!(
                record.cmd, "echo hello",
                "shell command survives resize and native takeover"
            );
            assert!(
                record.command_exact,
                "the fixture's shell-reported command keeps its provenance"
            );
            assert!(
                !record.command_truncated,
                "resize must not invent command truncation"
            );
        }
        let terminal = {
            let blocks = view.finished_blocks.borrow();
            assert_eq!(blocks.len(), expected_count, "command became a real card");
            let block = blocks.last().expect("completed output card");
            assert_eq!(
                block.with_searchable_output(|text| text.matches("history output").count()),
                80,
                "the complete output is retained in the record"
            );
            block.output_vte.clone()
        };
        wait_for("long output is scrollable inside its mapped card", || {
            terminal.is_mapped()
                && terminal.vadjustment().is_some_and(|adjustment| {
                    adjustment.upper() - adjustment.lower() > adjustment.page_size()
                })
        });
        wait_for(
            "all long-output rows survive in native VTE scrollback",
            || {
                let stream = gtk::gio::MemoryOutputStream::new_resizable();
                terminal
                    .write_contents_sync(
                        &stream,
                        vte4::WriteFlags::Default,
                        gtk::gio::Cancellable::NONE,
                    )
                    .expect("VTE writes its complete buffer");
                stream.close(gtk::gio::Cancellable::NONE).expect("close");
                String::from_utf8_lossy(&stream.steal_as_bytes())
                    .matches("history output")
                    .count()
                    == 80
            },
        );
    }

    #[test]
    #[ignore = "requires DISPLAY; samples real GTK/VTE allocations after paint"]
    fn chat_completion_never_expands_between_shell_markers() {
        let (view, window, _) = fixture();
        let clip = view.active.borrow().live_clip();
        let dock = widget_with_class(&view.root, "chat-controls")
            .parent()
            .expect("input dock");
        let dock_limit = Cell::new(i32::MAX);
        let clock = view.root.frame_clock().expect("mapped frame clock");
        // Ticks run before layout. Sampling after-paint sees the allocation
        // the user actually saw, including the first frame after each marker.
        let samples = Rc::new(RefCell::new(Vec::new()));
        let captured = samples.clone();
        let state = view.bstate.clone();
        let sampled_clip = clip.clone();
        let sampled_dock = dock.clone();
        let handler = clock.connect_after_paint(move |_| {
            captured.borrow_mut().push((
                state.get(),
                sampled_clip.height(),
                if sampled_dock.is_mapped() {
                    sampled_dock.height()
                } else {
                    0
                },
            ));
        });
        let redraw = view.root.downgrade();
        let updater = clock.connect_update(move |_| {
            if let Some(root) = redraw.upgrade() {
                root.queue_draw();
            }
        });
        clock.begin_updating();
        let phase = |bytes: &[u8], expected: BlockState, cap: Option<i32>| {
            samples.borrow_mut().clear();
            view.pty.emit_test_output(bytes);
            wait_for(
                "eight allocated frames in the intermediate lifecycle state",
                || {
                    samples
                        .borrow()
                        .iter()
                        .filter(|(state, _, _)| *state == expected)
                        .count()
                        >= 8
                },
            );
            let samples = samples.borrow();
            let heights: Vec<_> = samples
                .iter()
                .filter(|(state, _, _)| *state == expected)
                .map(|(_, height, _)| *height)
                .collect();
            let max_dock = samples
                .iter()
                .filter(|(state, _, _)| *state == expected)
                .map(|(_, _, height)| *height)
                .max()
                .unwrap();
            let min = *heights.iter().min().unwrap();
            let max = *heights.iter().max().unwrap();
            eprintln!(
                "{expected:?}: {} after-paint frames, clip={min}..{max}, cap={cap:?}",
                heights.len()
            );
            eprintln!(
                "input dock={max_dock}px, compact dock limit={}px",
                dock_limit.get()
            );
            assert!(min > 0, "live terminal remains allocated");
            if let Some(cap) = cap {
                assert!(
                    max_dock <= dock_limit.get(),
                    "{expected:?} input dock expanded to {max_dock}px"
                );
                assert!(
                    max <= cap,
                    "{expected:?} expanded to {max}px (compact limit {cap}px)"
                );
            }
            max
        };
        let prompt_start = b"\x1b]133;A\x07$ ";
        let prompt_end = b"\x1b]133;B\x07";
        let start = b"echo hello\r\n\x1b]133;C;cmdline_url=echo%20hello\x07hello\r\n";
        let end = b"\x1b]133;D;0\x07";
        let initial = phase(prompt_start, BlockState::CollectingPrompt, None);
        phase(prompt_end, BlockState::AwaitingCommand, None);
        pump_for(Duration::from_millis(250));
        let compact = clip.height();
        assert!(
            initial > compact * 2,
            "the first incomplete prompt retains native VTE"
        );
        for (width, scale) in [(640, 1.0), (320, 1.0), (320, 1.6)] {
            window.set_default_size(width, 560);
            view.set_font_scale(scale);
            pump_for(Duration::from_millis(250));
            if window.width() > width {
                fn explain(widget: &gtk::Widget, depth: usize) {
                    let measured = widget.measure(gtk::Orientation::Horizontal, -1);
                    if measured.0 > 150 || widget.has_css_class("chat-controls") {
                        eprintln!(
                            "MINIMUM depth={depth} type={} css={:?} min={} natural={} allocated={}",
                            widget.type_().name(),
                            widget.css_classes(),
                            measured.0,
                            measured.1,
                            widget.width()
                        );
                    }
                    if depth < 16 {
                        let mut child = widget.first_child();
                        while let Some(current) = child {
                            explain(&current, depth + 1);
                            child = current.next_sibling();
                        }
                    }
                }
                explain(window.upcast_ref(), 0);
            }
            assert!(
                window.width() <= width,
                "narrow window was not actually allocated: requested {width}, got {}",
                window.width()
            );
            assert_eq!(view.active_vte.font_scale(), scale);
            eprintln!(
                "allocated window={}x{}, font scale={scale}",
                window.width(),
                window.height()
            );
            dock_limit.set(dock.height() + 2 * view.active_vte.char_height() as i32);
            let cap = clip.height() + 2 * view.active_vte.char_height() as i32;
            assert!(
                cap < view.root.height() * 3 / 4,
                "fixture must distinguish compact and native"
            );
            phase(start, BlockState::CollectingOutput, Some(cap));
            // Never combine D/A/B: each intermediate state must survive at
            // least eight painted frames, including in a narrow, zoomed pane.
            phase(end, BlockState::PostCommand, Some(cap));
            phase(prompt_start, BlockState::CollectingPrompt, Some(cap));
            phase(prompt_end, BlockState::AwaitingCommand, Some(cap));
            let mut fast = start.to_vec();
            fast.extend_from_slice(end);
            phase(&fast, BlockState::PostCommand, Some(cap));
            assert!(
                !samples
                    .borrow()
                    .iter()
                    .any(|(state, _, _)| *state == BlockState::CollectingOutput),
                "fast command completes before a Running frame is painted"
            );
            phase(prompt_start, BlockState::CollectingPrompt, Some(cap));
            phase(prompt_end, BlockState::AwaitingCommand, Some(cap));
            // RIS and A share a batch: no layout is guaranteed to observe Idle.
            let reset = b"\x1bc\x1b]133;A\x07$ ";
            assert!(
                phase(reset, BlockState::CollectingPrompt, None) > cap,
                "reset must retire continuity before the next unconfirmed prompt"
            );
            phase(prompt_end, BlockState::AwaitingCommand, Some(cap));
            assert!(phase(b"\x1b[?1049h", BlockState::AltScreen, None) > cap);
            phase(b"\x1b[?1049l", BlockState::AwaitingCommand, Some(cap));
            assert!(phase(b"\x1bcraw\r\n", BlockState::RawFallback, None) > cap);
            assert!(
                phase(prompt_start, BlockState::CollectingPrompt, None) > cap,
                "raw takeover cannot retain a prior compact prompt"
            );
            phase(prompt_end, BlockState::AwaitingCommand, Some(cap));
        }
        clock.end_updating();
        clock.disconnect(updater);
        clock.disconnect(handler);
        window.close();
        view.pty.kill();
    }

    #[test]
    #[ignore = "requires DISPLAY"]
    fn live_font_updates_align_chat_block_and_window_text() {
        let (chat, window, entry) = fixture();
        window.add_css_class("anvil-font-sync");
        window.set_default_size(1000, 900);
        let block = TermView::new_with_spawner(
            &crate::config::Config::safe_defaults(),
            &crate::config::TerminalMode::Block,
            &["sh".to_string()],
            None,
            false,
            None,
            "font-test",
            &[],
            &[],
            |_, _, _, _| crate::pty::OwnedPty::from_openpty(Some(true)),
        )
        .expect("nonexecuting block PTY");
        window
            .child()
            .unwrap()
            .downcast::<gtk::Box>()
            .unwrap()
            .append(&block.root);
        drive(&chat, PROMPT, BlockState::AwaitingCommand);
        let provider = crate::startup_ui::install_dynamic_css_provider();
        let mut config = crate::config::Config::safe_defaults();
        crate::font::freeze_ui_font(&mut config);
        let hint = widget_with_class(&chat.root, "chat-input-hint")
            .downcast::<gtk::Label>()
            .unwrap();
        let tools = widget_with_class(&chat.root, "shell-tool-label")
            .downcast::<gtk::Label>()
            .unwrap();
        let controls = widget_with_class(&chat.root, "chat-controls");
        let mut tools_glyph = None;
        for (desc, scale) in [("Monospace 16px", 1.25), ("Monospace 24px", 1.5)] {
            config.font_desc = desc.into();
            config.default_font_scale = scale;
            provider.load_from_string(&crate::config_ops::dynamic_css(&config));
            crate::workspace_shell::apply_ui_font_scale(crate::font::reference_ui_role_multiplier(
                &config,
                &window.pango_context(),
            ));
            for view in [&*chat, &block] {
                view.set_font(desc);
                view.set_font_scale(scale);
            }
            pump_for(Duration::from_millis(250));
            let expected =
                (gtk::pango::FontDescription::from_string(desc).size() as f64 * scale) as i32;
            assert_eq!(
                entry.pango_context().font_description().unwrap().size(),
                expected
            );
            for view in [&*chat, &block] {
                assert_eq!(view.active_vte.font_scale(), scale);
                assert_eq!(
                    view.active_vte.font().unwrap().size(),
                    gtk::pango::FontDescription::from_string(desc).size()
                );
            }
            // The old hint is now a hidden state bridge, not painted text.
            // Keep its production state/tooltip contract, and measure the
            // actual visible Chrome glyph instead of inventing hidden fonts.
            assert!(!hint.is_visible());
            assert_eq!(
                controls.tooltip_text().as_deref(),
                Some(hint.text().as_str())
            );
            assert!(tools.is_mapped());
            assert_eq!(
                tools.pango_context().font_description().unwrap().size(),
                10 * gtk::pango::SCALE
            );
            let glyph = tools.layout().pixel_size();
            if let Some(previous) = tools_glyph {
                assert_eq!(glyph, previous, "Main font must not change Chrome glyphs");
            }
            tools_glyph = Some(glyph);
            assert_footer_inside_pane(&chat, &button(&chat, "chat-send"));
        }
        drive(&chat, START, BlockState::CollectingOutput);
        let status = widget_with_class(&chat.root, "chat-running-status")
            .downcast::<gtk::Label>()
            .unwrap();
        assert!(status.is_mapped());
        assert!(!hint.is_visible());
        assert_eq!(status.text(), hint.text());
        let base_glyph = tools_glyph.unwrap();
        for (size, scale) in [(14.0, 1.0), (14.0, 1.3), (28.0, 1.0), (14.0, 1.0)] {
            config.ui_font_size = Some(size);
            config.ui_font_size_absolute = false;
            config.ui_font_scale = Some(scale);
            provider.load_from_string(&crate::config_ops::dynamic_css(&config));
            let factor =
                crate::font::reference_ui_role_multiplier(&config, &window.pango_context());
            crate::workspace_shell::apply_ui_font_scale(factor);
            pump_for(Duration::from_millis(250));
            assert!(tools.is_mapped() && status.is_mapped());
            for (label, base) in [(&tools, 10.0), (&status, 11.0)] {
                let raw = label.pango_context().font_description().unwrap().size();
                let expected = (base * factor * f64::from(gtk::pango::SCALE)).round() as i32;
                assert!(
                    (raw - expected).abs() <= 1,
                    "visible Chrome font {raw} vs {expected}"
                );
            }
            let glyph = tools.layout().pixel_size();
            if factor > 1.1 {
                assert!(
                    glyph.0 > base_glyph.0 && glyph.1 > base_glyph.1,
                    "actual Tools glyph must grow"
                );
            } else {
                assert_eq!(glyph, base_glyph);
            }
            assert_eq!(
                entry.pango_context().font_description().unwrap().size(),
                36 * gtk::pango::SCALE,
                "UI settings must not change Content entry"
            );
            assert_eq!(chat.active_vte.font_scale(), 1.5);
            assert_eq!(
                chat.active_vte.font().unwrap().size(),
                24 * gtk::pango::SCALE
            );
            assert_eq!(status.text(), hint.text());
            assert!(!hint.is_visible());
            eprintln!("visible footer: UI size={size} scale={scale} factor={factor} Tools glyph={glyph:?} status_raw={}", status.pango_context().font_description().unwrap().size());
        }
        crate::workspace_shell::apply_ui_font_scale(1.0);
        window.destroy();
        gtk::style_context_remove_provider_for_display(
            &gtk::gdk::Display::default().unwrap(),
            &provider,
        );
    }

    #[test]
    #[ignore = "requires DISPLAY; production window, parser and real PTY winsize"]
    fn chat_reparents_only_live_clip_and_hides_submit_for_native_takeover() {
        docked_reparent_scroll_and_resize_lifecycle(crate::config::TerminalMode::Chat);
    }

    #[test]
    #[ignore = "requires DISPLAY; production window, parser and real PTY winsize"]
    fn block_reparents_only_live_clip_and_keeps_bottom_input_fixed() {
        docked_reparent_scroll_and_resize_lifecycle(crate::config::TerminalMode::Block);
    }

    fn docked_reparent_scroll_and_resize_lifecycle(mode: crate::config::TerminalMode) {
        let (view, window, other) = fixture_for_mode(mode);
        let terminal = view.active_vte.clone();
        let holder = view.active.borrow().widget().clone();
        let send = button(&view, "chat-send");
        let stop = button(&view, "chat-stop");
        other.grab_focus();
        drive(&view, PROMPT, BlockState::AwaitingCommand);
        assert!(owns_focus(&other), "background prompt cannot steal focus");
        assert!(send.is_visible() && !stop.is_visible());
        let hint = widget_with_class(&view.root, "chat-input-hint")
            .downcast::<gtk::Label>()
            .unwrap();
        assert_eq!(hint.tooltip_text().as_deref(), Some(hint.text().as_str()));
        assert_eq!(hint.width_chars(), 1);
        assert_ne!(
            view.active.borrow().live_clip().parent().as_ref(),
            Some(holder.upcast_ref())
        );
        height_roundtrip(&view, &window, false);
        // A completion/multiline editor can occupy the entire grid. The old
        // measured Box made this old row count a permanent window minimum.
        let rows = view.pty.test_slave_winsize().1.max(4) as usize;
        let multiline = "continuation line\r\n".repeat(rows + 3);
        view.pty.emit_test_output(multiline.as_bytes());
        pump_for(Duration::from_millis(200));
        height_roundtrip(&view, &window, false);
        assert_footer_inside_pane(&view, &send);
        // Real dock coordinates reach the same cross-selection capture owner.
        view.selected_block_id.set(Some(42));
        view.selected_block_ids.borrow_mut().insert(42);
        view.selection_anchor_id.set(Some(42));
        let bounds = view
            .active
            .borrow()
            .live_clip()
            .compute_bounds(&view.root)
            .unwrap();
        press_capture(
            &view.root,
            f64::from(bounds.x()) + 20.0,
            f64::from(bounds.y()) + 10.0,
        );
        assert_eq!(view.selected_block_id.get(), None);
        assert!(view.selected_block_ids.borrow().is_empty());
        view.active_vte.select_all();
        pump_for(Duration::from_millis(30));
        assert!(view.active_vte.has_selection());
        assert!(view
            .cross_selection
            .copy_text()
            .unwrap()
            .unwrap()
            .contains("continuation"));
        view.active_vte.unselect_all();
        other.grab_focus();
        drive(&view, START, BlockState::CollectingOutput);
        assert!(owns_focus(&other), "background command cannot steal focus");
        assert!(!send.is_visible() && stop.is_visible());
        assert_eq!(hint.tooltip_text().as_deref(), Some(hint.text().as_str()));
        assert!(hint.text().starts_with("Running ·"));
        assert_eq!(
            view.active.borrow().live_clip().parent().as_ref(),
            Some(holder.upcast_ref())
        );
        height_roundtrip(&view, &window, false);
        assert_footer_inside_pane(&view, &stop);
        terminal.grab_focus();
        drive(&view, b"\x1b[?1049h", BlockState::AltScreen);
        assert!(
            terminal.has_focus(),
            "native reparent restores existing focus"
        );
        assert!(!send.is_visible() && !stop.is_visible());
        assert!(!view.block_scroll.is_mapped());
        height_roundtrip(&view, &window, true);
        other.grab_focus();
        drive(&view, b"\x1b[?1049l", BlockState::CollectingOutput);
        assert!(owns_focus(&other), "background alt exit cannot steal focus");
        let completed_before = view.finished_blocks.borrow().len();
        drive(
            &view,
            "history output\r\n".repeat(80).as_bytes(),
            BlockState::CollectingOutput,
        );
        drive(&view, NEXT, BlockState::AwaitingCommand);
        assert_finished_long_output(&view, completed_before + 1);
        // A single long card is deliberately viewport-capped, with overflow
        // retained in its own VTE. Complete several real commands to exercise
        // outer history scrolling independently of that inner scrollback.
        for completed in 2..=3 {
            drive(&view, START, BlockState::CollectingOutput);
            drive(
                &view,
                "history output\r\n".repeat(80).as_bytes(),
                BlockState::CollectingOutput,
            );
            drive(&view, NEXT, BlockState::AwaitingCommand);
            assert_finished_long_output(&view, completed_before + completed);
        }
        assert!(owns_focus(&other));
        assert!(send.is_visible() && !stop.is_visible());
        assert_eq!(holder.parent().as_ref(), Some(view.block_list.upcast_ref()));
        assert_eq!(terminal, view.active_vte);
        let adjustment = view.block_scroll.vadjustment();
        wait_for("real finished cards overflow the outer history", || {
            adjustment.upper() - adjustment.lower() > adjustment.page_size()
        });
        let clock = view.block_scroll.frame_clock().expect("mapped frame clock");
        let painted = Rc::new(Cell::new(0_u64));
        let painted_cb = painted.clone();
        let handler = clock.connect_after_paint(move |_| painted_cb.set(painted_cb.get() + 1));
        let before = painted.get();
        adjustment.set_value(adjustment.upper() - adjustment.page_size());
        view.block_scroll.queue_draw();
        wait_for(
            "outer history bottom has been allocated and painted",
            || {
                painted.get() > before
                    && (adjustment.value() - (adjustment.upper() - adjustment.page_size())).abs()
                        < 0.5
            },
        );
        let bottom = adjustment.value();
        assert!(bottom > adjustment.lower(), "history reached its bottom");
        let history_y = view.block_list.compute_bounds(&view.root).unwrap().y();
        let dock_y = view
            .active
            .borrow()
            .live_clip()
            .compute_bounds(&view.root)
            .unwrap()
            .y();
        let before = painted.get();
        adjustment.set_value(adjustment.lower());
        view.block_scroll.queue_draw();
        wait_for("outer history really moves from bottom to top", || {
            painted.get() > before
                && adjustment.value() == adjustment.lower()
                && view.block_list.compute_bounds(&view.root).unwrap().y() > history_y
        });
        clock.disconnect(handler);
        assert_eq!(adjustment.value(), adjustment.lower());
        assert!(adjustment.value() < bottom, "outer history really scrolled");
        assert!(
            view.block_list.compute_bounds(&view.root).unwrap().y() > history_y,
            "finished cards moved with the outer history viewport"
        );
        assert_eq!(
            view.active
                .borrow()
                .live_clip()
                .compute_bounds(&view.root)
                .unwrap()
                .y(),
            dock_y,
            "history scrolling cannot move the composer"
        );
        drive(&view, b"\x1bcplain output\r\n", BlockState::RawFallback);
        assert!(!send.is_visible() && !stop.is_visible());
        height_roundtrip(&view, &window, true);
        drive(&view, PROMPT, BlockState::AwaitingCommand);
        assert!(send.is_visible());
        window.close();
        view.pty.kill();
    }

    #[test]
    #[ignore = "requires DISPLAY; real native reparent allocation and PTY winsize"]
    fn chat_raw_takeover_updates_columns_without_resizing_the_window() {
        docked_raw_takeover_updates_columns_without_resizing_the_window(
            crate::config::TerminalMode::Chat,
        );
    }

    #[test]
    #[ignore = "requires DISPLAY; real native reparent allocation and PTY winsize"]
    fn block_raw_takeover_updates_columns_without_resizing_the_window() {
        docked_raw_takeover_updates_columns_without_resizing_the_window(
            crate::config::TerminalMode::Block,
        );
    }

    fn docked_raw_takeover_updates_columns_without_resizing_the_window(
        mode: crate::config::TerminalMode,
    ) {
        let (view, window, other) = fixture_for_mode(mode);
        other.grab_focus();
        drive(&view, PROMPT, BlockState::AwaitingCommand);
        let terminal = view.active_vte.clone();
        let clip = view.active.borrow().live_clip();
        let fd = view.pty.master_fd_raw();
        let window_size = (window.width(), window.height());
        let prompt_width = clip.width();
        let prompt_grid = view.pty.test_slave_winsize();
        // Border bounds include VTE's native CSS padding; Widget::width()
        // reports only its content and is not the allocation given by the clip.
        let terminal_width = || terminal.compute_bounds(&clip).expect("mapped VTE").width();
        assert_eq!(terminal_width(), prompt_width as f32);
        assert_eq!(terminal.column_count(), i64::from(prompt_grid.0));

        // RIS invalidates the prompt. The only stimulus is this one reader
        // batch: no sync/layout call, extra output or window resize may rescue
        // the old inset-composer width after native takeover.
        drive(&view, b"\x1bcplain output\r\n", BlockState::RawFallback);
        wait_for(
            "native remap reaches the real VTE and PTY without resize",
            || {
                let native_grid = view.pty.test_slave_winsize();
                clip.width() > prompt_width
                    && terminal_width() == clip.width() as f32
                    && native_grid.0 > prompt_grid.0
                    && terminal.column_count() == i64::from(native_grid.0)
                    && terminal.row_count() == i64::from(native_grid.1)
                    && view.resize_tick_id.borrow().is_none()
            },
        );
        assert_eq!((window.width(), window.height()), window_size);
        assert!(owns_focus(&other), "background fallback cannot steal focus");
        assert!(!button(&view, "chat-send").is_visible());
        assert!(!button(&view, "chat-stop").is_visible());
        height_roundtrip(&view, &window, true);

        drive(&view, PROMPT, BlockState::AwaitingCommand);
        wait_for(
            "prompt restores its original allocated width and PTY grid",
            || {
                clip.width() == prompt_width
                    && terminal_width() == prompt_width as f32
                    && view.pty.test_slave_winsize() == prompt_grid
                    && view.resize_tick_id.borrow().is_none()
            },
        );
        assert_eq!((window.width(), window.height()), window_size);
        assert_eq!(view.active_vte, terminal);
        assert_eq!(view.pty.master_fd_raw(), fd);
        assert!(owns_focus(&other));
        window.close();
        view.pty.kill();
    }

    #[test]
    #[ignore = "requires DISPLAY; production commit/admission and real PTY recorder"]
    fn chat_submit_uses_one_native_commit_and_refuses_a_stale_button() {
        docked_submit_uses_one_native_commit_and_refuses_a_stale_button(
            crate::config::TerminalMode::Chat,
        );
    }

    #[test]
    #[ignore = "requires DISPLAY; production commit/admission and real PTY recorder"]
    fn block_submit_uses_one_native_commit_and_refuses_a_stale_button() {
        docked_submit_uses_one_native_commit_and_refuses_a_stale_button(
            crate::config::TerminalMode::Block,
        );
    }

    fn docked_submit_uses_one_native_commit_and_refuses_a_stale_button(
        mode: crate::config::TerminalMode,
    ) {
        let (view, window, _) = fixture_for_mode(mode);
        drive(&view, PROMPT, BlockState::AwaitingCommand);
        let button = button(&view, "chat-send");
        let received = Rc::new(Cell::new(0));
        let received_cb = received.clone();
        view.connect_human_input(move |_| received_cb.set(received_cb.get() + 1));
        view.pty.drain_test_slave(Duration::from_millis(20));
        for expected in 1..=2 {
            press_capture(&button, 2.0, 2.0);
            button.emit_clicked();
            assert_eq!(view.pty.drain_test_slave(Duration::from_millis(30)), b"\r");
            assert_eq!(received.get(), expected);
        }
        // A PS2 continuation may have no new OSC B. Each activation still sends
        // just one Enter, never the shadow text and never a second exec path.
        view.selection_feed_hold.begin_drag();
        assert!(view.selection_feed_hold.try_buffer(b"\x1b]133;C\x07"));
        press_capture(&button, 2.0, 2.0);
        button.emit_clicked();
        assert_eq!(view.bstate.get(), BlockState::CollectingOutput);
        assert!(view
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        assert_eq!(received.get(), 2);
        drive(&view, b"\x1b[?1049h", BlockState::AltScreen);
        press_capture(&button, 2.0, 2.0);
        button.emit_clicked();
        assert!(view
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        assert_eq!(received.get(), 2);
        window.close();
        view.pty.kill();
    }

    fn saturate(pty: &crate::pty::OwnedPty) {
        let chunk = vec![b'x'; crate::pty::FD_WRITER_MAX_MESSAGE_BYTES];
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut admitted = 0;
        while admitted < 2 {
            assert!(Instant::now() < deadline, "fixture writer made no progress");
            if pty.try_write_bytes(&chunk).is_ok() {
                admitted += 1;
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        // The first chunk blocks in the undrained kernel, the second fills the
        // bounded queue. Even a single-byte Enter is now deterministically refused.
        assert!(pty
            .try_write_bytes(b"probe")
            .unwrap_err()
            .to_string()
            .contains("queue safety limit"));
    }

    #[test]
    #[ignore = "requires DISPLAY; real Closed and full-queue writer admission"]
    fn chat_buttons_preserve_input_and_feedback_when_writer_refuses() {
        for closed in [false, true] {
            let (view, window, _) = fixture();
            drive(&view, PROMPT, BlockState::AwaitingCommand);
            let send = button(&view, "chat-send");
            let received = Rc::new(Cell::new(0));
            let received_cb = received.clone();
            view.connect_human_input(move |_| received_cb.set(received_cb.get() + 1));
            *view.typed_cmd.borrow_mut() = "keep editor".to_string();
            view.pty_synced.set(false);
            view.idle_input_dirty.set(false);
            view.selected_block_id.set(Some(42));
            view.selected_block_ids.borrow_mut().insert(42);
            view.selection_anchor_id.set(Some(42));
            view.user_scrolled_up.set(true);
            view.unread_count.set(7);
            let text = view.active_vte.text_format(vte4::Format::Text);
            if closed {
                view.pty.kill();
                assert!(view
                    .pty
                    .try_write_bytes(b"probe")
                    .unwrap_err()
                    .to_string()
                    .contains("PTY is closed"));
            } else {
                saturate(&view.pty);
            }
            press_capture(&send, 2.0, 2.0);
            send.emit_clicked();
            assert_eq!(&*view.typed_cmd.borrow(), "keep editor");
            assert!(!view.pty_synced.get() && !view.idle_input_dirty.get());
            assert_eq!(view.selected_block_id.get(), Some(42));
            assert!(view.selected_block_ids.borrow().contains(&42));
            assert_eq!(view.selection_anchor_id.get(), Some(42));
            assert!(view.user_scrolled_up.get());
            assert_eq!(view.unread_count.get(), 7);
            assert_eq!(view.active_vte.text_format(vte4::Format::Text), text);
            assert_eq!(received.get(), 0);
            // Use actual parser replay to enter Running even when a Closed
            // fixture cannot emit new output, then exercise the real Stop too.
            view.selection_feed_hold.begin_drag();
            assert!(view.selection_feed_hold.try_buffer(b"\x1b]133;C\x07"));
            view.selection_feed_hold.flush_now();
            assert_eq!(view.bstate.get(), BlockState::CollectingOutput);
            let stop = button(&view, "chat-stop");
            press_capture(&stop, 2.0, 2.0);
            stop.emit_clicked();
            assert_eq!(received.get(), 0);
            assert_eq!(view.bstate.get(), BlockState::CollectingOutput);
            view.pty.kill();
            window.close();
            pump_for(Duration::from_millis(20));
        }
    }

    #[test]
    #[ignore = "requires DISPLAY; production Stop gesture and two-generation held parser batch"]
    fn chat_stop_is_bound_to_displayed_command_and_foreground() {
        let (view, window, _) = fixture();
        drive(&view, PROMPT, BlockState::AwaitingCommand);
        drive(&view, START, BlockState::CollectingOutput);
        let stop = button(&view, "chat-stop");
        // The reference control has no CSS padding and rounded corners.
        // (2,2) lies outside its hit shape; use a real, validated inside point
        // when asking GtkButton's release gesture to emit clicked.
        let hit = (
            f64::from(stop.width()) / 2.0,
            f64::from(stop.height()) / 2.0,
        );
        assert!(
            stop.contains(hit.0, hit.1),
            "mapped Stop center must be a hit"
        );
        eprintln!(
            "Stop hit: size={}x{} old_corner={} center=({},{})",
            stop.width(),
            stop.height(),
            stop.contains(2.0, 2.0),
            hit.0,
            hit.1
        );
        let feedback = Rc::new(Cell::new(0));
        let feedback_cb = feedback.clone();
        view.connect_human_input(move |_| feedback_cb.set(feedback_cb.get() + 1));
        view.pty.drain_test_slave(Duration::from_millis(20));
        press_capture(&stop, hit.0, hit.1);
        stop.emit_clicked();
        assert_eq!(
            view.pty.drain_test_slave(Duration::from_millis(30)),
            b"\x03"
        );
        assert_eq!(feedback.get(), 1);
        assert_eq!(
            view.bstate.get(),
            BlockState::CollectingOutput,
            "interrupt is not completion"
        );
        // The stale control sees Running both before and after replay. Identity,
        // rather than just surface/state equality, must reject the new command.
        view.active_vte.select_all();
        assert!(view.active_vte.has_selection());
        view.selection_feed_hold.begin_drag();
        let next_start = [NEXT, START].concat();
        assert!(view.selection_feed_hold.try_buffer(&next_start));
        view.selection_feed_hold.end_drag(true);
        let stop_bounds = stop.compute_bounds(&view.root).unwrap();
        // GTK visits the ancestor's selection observer before the button's
        // capture gesture. It must not replay the held new command too early.
        press_capture(
            &view.root,
            f64::from(stop_bounds.x()) + hit.0,
            f64::from(stop_bounds.y()) + hit.1,
        );
        assert!(
            view.active_vte.has_selection(),
            "the control owns selection-feed release"
        );
        press_capture(&stop, hit.0, hit.1);
        stop.emit_clicked();
        assert_eq!(view.bstate.get(), BlockState::CollectingOutput);
        assert!(view
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        assert_eq!(feedback.get(), 1);
        // A long press begun on the old displayed command must also fail when
        // a normal (not held) parser update replaces the command before release.
        press_capture(&stop, hit.0, hit.1);
        drive(&view, &next_start, BlockState::CollectingOutput);
        stop.emit_clicked();
        assert!(view
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        assert_eq!(feedback.get(), 1);
        // Old release cleanup must not erase the newer held press's identity.
        press_capture(&stop, hit.0, hit.1);
        release_capture(&stop, hit.0, hit.1);
        // The old command is still current at release, so GtkButton sends a
        // legitimate interrupt now. Account for that exact byte before the
        // next press; drain_test_slave does not pump the pending GTK idle.
        assert_eq!(
            view.pty.drain_test_slave(Duration::from_millis(30)),
            b"\x03"
        );
        assert_eq!(feedback.get(), 2);
        press_capture(&stop, hit.0, hit.1);
        drive(&view, &next_start, BlockState::CollectingOutput);
        stop.emit_clicked();
        assert_eq!(
            view.pty.drain_test_slave(Duration::from_millis(30)),
            b"",
            "the newer press must not interrupt a replacement command"
        );
        assert_eq!(feedback.get(), 2);
        // Also prove the newer token survives old idle cleanup, rather than
        // passing the refusal above merely because every token was erased.
        press_capture(&stop, hit.0, hit.1);
        release_capture(&stop, hit.0, hit.1);
        assert_eq!(
            view.pty.drain_test_slave(Duration::from_millis(30)),
            b"\x03"
        );
        assert_eq!(feedback.get(), 3);
        press_capture(&stop, hit.0, hit.1);
        pump_for(Duration::from_millis(20));
        stop.emit_clicked();
        assert_eq!(
            view.pty.drain_test_slave(Duration::from_millis(30)),
            b"\x03",
            "old release cleanup must preserve the newer same-command press"
        );
        assert_eq!(feedback.get(), 4);
        press_capture(&stop, hit.0, hit.1);
        view.pty.set_test_foreground_group(Some(202));
        stop.emit_clicked();
        assert!(view
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        assert_eq!(feedback.get(), 4);
        press_capture(&stop, hit.0, hit.1);
        stop.emit_clicked();
        assert_eq!(
            view.pty.drain_test_slave(Duration::from_millis(30)),
            b"\x03"
        );
        assert_eq!(feedback.get(), 5);
        drive(&view, b"\x1b[?1049h", BlockState::AltScreen);
        stop.emit_clicked();
        assert!(view
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        assert_eq!(feedback.get(), 5);
        window.close();
        view.pty.kill();
    }
    #[test]
    #[ignore = "requires DISPLAY; real GtkWidget activate and delayed native Stop callback"]
    fn chat_native_activation_never_rebinds_a_delayed_stop() {
        let (view, window, _) = fixture();
        drive(&view, PROMPT, BlockState::AwaitingCommand);
        drive(&view, START, BlockState::CollectingOutput);
        let stop = button(&view, "chat-stop");
        let feedback = Rc::new(Cell::new(0));
        let feedback_cb = feedback.clone();
        view.connect_human_input(move |_| feedback_cb.set(feedback_cb.get() + 1));
        view.pty.drain_test_slave(Duration::from_millis(20));
        assert!(
            !stop.is_focusable(),
            "AT-SPI activation does not require focusability"
        );
        assert!(gtk::prelude::WidgetExt::activate(&stop));
        assert!(stop.has_css_class("keyboard-activating"));
        // No main-context pumping between activate and the replacement: GTK
        // has not had an opportunity to deliver its delayed clicked signal.
        view.selection_feed_hold.begin_drag();
        assert!(view.selection_feed_hold.try_buffer(&[NEXT, START].concat()));
        view.selection_feed_hold.flush_now();
        assert_eq!(view.bstate.get(), BlockState::CollectingOutput);
        assert!(
            gtk::prelude::WidgetExt::activate(&stop),
            "repeated native activation is coalesced"
        );
        assert_eq!(feedback.get(), 0);
        pump_for(Duration::from_millis(350));
        assert!(view
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        assert_eq!(feedback.get(), 0);
        // Stable target: two activations in GTK's animation interval produce
        // one actual interrupt, and an unarmed duplicate clicked is rejected.
        assert!(gtk::prelude::WidgetExt::activate(&stop));
        assert!(gtk::prelude::WidgetExt::activate(&stop));
        pump_for(Duration::from_millis(350));
        assert_eq!(
            view.pty.drain_test_slave(Duration::from_millis(30)),
            b"\x03"
        );
        assert_eq!(feedback.get(), 1);
        stop.emit_clicked();
        assert!(view
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        assert_eq!(feedback.get(), 1);
        // Hiding and disabling invalidate the captured token, even if the
        // widget returns before the old native callback. New requests recover.
        for hide in [false, true] {
            assert!(gtk::prelude::WidgetExt::activate(&stop));
            if hide {
                stop.set_visible(false);
                stop.set_visible(true);
            } else {
                stop.set_sensitive(false);
                stop.set_sensitive(true);
            }
            pump_for(Duration::from_millis(350));
            assert!(view
                .pty
                .drain_test_slave(Duration::from_millis(30))
                .is_empty());
            assert_eq!(feedback.get(), 1);
        }
        assert!(gtk::prelude::WidgetExt::activate(&stop));
        pump_for(Duration::from_millis(350));
        assert_eq!(
            view.pty.drain_test_slave(Duration::from_millis(30)),
            b"\x03"
        );
        assert_eq!(feedback.get(), 2);
        assert!(gtk::prelude::WidgetExt::activate(&stop));
        window.close();
        pump_for(Duration::from_millis(350));
        assert!(view
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        assert_eq!(feedback.get(), 2);
        view.pty.kill();
    }

    #[test]
    #[ignore = "requires DISPLAY; real GtkWidget activate and delayed native Send callback"]
    fn chat_native_activation_never_rebinds_send_to_a_new_prompt() {
        let (view, window, _) = fixture();
        drive(&view, PROMPT, BlockState::AwaitingCommand);
        let send = button(&view, "chat-send");
        let feedback = Rc::new(Cell::new(0));
        let feedback_cb = feedback.clone();
        view.connect_human_input(move |_| feedback_cb.set(feedback_cb.get() + 1));
        view.pty.drain_test_slave(Duration::from_millis(20));
        assert!(gtk::prelude::WidgetExt::activate(&send));
        assert!(send.has_css_class("keyboard-activating"));
        // Old prompt -> command -> new prompt, all before GTK may dispatch the
        // animation timeout. Final state equality must not refresh the target.
        view.selection_feed_hold.begin_drag();
        assert!(view.selection_feed_hold.try_buffer(&[START, NEXT].concat()));
        view.selection_feed_hold.flush_now();
        assert_eq!(view.bstate.get(), BlockState::AwaitingCommand);
        *view.typed_cmd.borrow_mut() = "keep new prompt".to_string();
        view.idle_input_dirty.set(false);
        assert!(gtk::prelude::WidgetExt::activate(&send));
        pump_for(Duration::from_millis(350));
        assert!(view
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        assert_eq!(feedback.get(), 0);
        assert_eq!(view.typed_cmd.borrow().as_str(), "keep new prompt");
        assert!(!view.idle_input_dirty.get());
        // A genuine new native activation is accepted exactly once.
        assert!(gtk::prelude::WidgetExt::activate(&send));
        assert!(gtk::prelude::WidgetExt::activate(&send));
        pump_for(Duration::from_millis(350));
        assert_eq!(view.pty.drain_test_slave(Duration::from_millis(30)), b"\r");
        assert_eq!(feedback.get(), 1);
        send.emit_clicked();
        assert!(view
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        assert_eq!(feedback.get(), 1);
        for hide in [false, true] {
            assert!(gtk::prelude::WidgetExt::activate(&send));
            if hide {
                send.set_visible(false);
                send.set_visible(true);
            } else {
                send.set_sensitive(false);
                send.set_sensitive(true);
            }
            pump_for(Duration::from_millis(350));
            assert!(view
                .pty
                .drain_test_slave(Duration::from_millis(30))
                .is_empty());
            assert_eq!(feedback.get(), 1);
        }
        // The same ledger also covers an ordinary mouse press held while a
        // command executes and a different prompt becomes ready.
        press_capture(&send, 2.0, 2.0);
        view.selection_feed_hold.begin_drag();
        assert!(view.selection_feed_hold.try_buffer(&[START, NEXT].concat()));
        view.selection_feed_hold.flush_now();
        send.emit_clicked();
        assert!(view
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        assert_eq!(feedback.get(), 1);
        assert!(gtk::prelude::WidgetExt::activate(&send));
        pump_for(Duration::from_millis(350));
        assert_eq!(view.pty.drain_test_slave(Duration::from_millis(30)), b"\r");
        assert_eq!(feedback.get(), 2);
        assert!(gtk::prelude::WidgetExt::activate(&send));
        window.close();
        pump_for(Duration::from_millis(350));
        assert!(view
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        assert_eq!(feedback.get(), 2);
        view.pty.kill();
    }

    #[test]
    #[ignore = "requires DISPLAY; real in-flight writer and production optional actions"]
    fn chat_buttons_never_queue_behind_inflight_keyboard_input() {
        let (view, window, _) = fixture();
        drive(&view, PROMPT, BlockState::AwaitingCommand);
        let feedback = Rc::new(Cell::new(0));
        let feedback_cb = feedback.clone();
        view.connect_human_input(move |_| feedback_cb.set(feedback_cb.get() + 1));
        view.pty.drain_test_slave(Duration::from_millis(20));
        let earlier = view.pty.test_start_inflight_input();
        let shadow = view.typed_cmd.borrow().clone();
        let dirty = view.idle_input_dirty.get();
        let send = button(&view, "chat-send");
        press_capture(&send, 2.0, 2.0);
        send.emit_clicked();
        assert_eq!(view.typed_cmd.borrow().as_str(), shadow);
        assert_eq!(view.idle_input_dirty.get(), dirty);
        view.selection_feed_hold.begin_drag();
        assert!(view.selection_feed_hold.try_buffer(START));
        view.selection_feed_hold.flush_now();
        assert_eq!(view.bstate.get(), BlockState::CollectingOutput);
        let stop = button(&view, "chat-stop");
        press_capture(&stop, 2.0, 2.0);
        stop.emit_clicked();
        assert_eq!(
            feedback.get(),
            0,
            "queued capacity is not a successful action"
        );
        view.selection_feed_hold.begin_drag();
        assert!(view.selection_feed_hold.try_buffer(&[NEXT, START].concat()));
        view.selection_feed_hold.flush_now();
        view.pty.set_test_foreground_group(Some(202));
        view.pty.try_write_bytes(b"ordinary tail").unwrap();
        let received = view
            .pty
            .drain_test_slave_exact(earlier + b"ordinary tail".len());
        assert!(received[..earlier].iter().all(|byte| *byte == b'x'));
        assert_eq!(&received[earlier..], b"ordinary tail");
        assert_eq!(feedback.get(), 0);
        window.close();
        view.pty.kill();
    }

    include!("ime_tests.rs");
}
