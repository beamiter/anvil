//! Relm4 component for the live settings dialog.
//!
//! Keeping the dialog's transient UI state and signal handling here means the
//! application model only consumes typed outputs.  GTK remains the rendering
//! backend, but it no longer owns application state through closure captures.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

use super::remote_picker::CapturedRemoteProfile;
use crate::config::{remote_text_is_safe, RemoteHost};

/// GTK notify callbacks run synchronously, before Relm processes queued input.
/// Suppress reflection at that boundary, without disconnecting preview listeners.
fn without_ascii_settings_output<T>(gate: &Cell<bool>, update: impl FnOnce() -> T) -> T {
    struct Reset<'a>(&'a Cell<bool>, bool);
    impl Drop for Reset<'_> {
        fn drop(&mut self) {
            self.0.set(self.1);
        }
    }
    let _reset = Reset(gate, gate.replace(true));
    update()
}

fn reflect_ascii_organism_controls(
    gate: &Cell<bool>,
    enabled_row: &adw::SwitchRow,
    motion_row: &adw::ComboRow,
    enabled: bool,
    motion: u32,
    safe_mode: bool,
) {
    without_ascii_settings_output(gate, || {
        enabled_row.set_active(enabled);
        motion_row.set_selected(motion);
        enabled_row.set_sensitive(!safe_mode);
        motion_row.set_sensitive(!safe_mode && enabled);
    });
}

/// A settings-only gallery: no terminal, reducer, repository memory, or config
/// writes. All sources use weak references and are removed on hide or close.
mod organism_preview {
    use super::adw;
    use adw::prelude::*;
    use gtk::glib;
    use jterm_core::organism::sprite_frame_with_context;
    use jterm_core::organism_daily::{GentleInteraction, PreviewPose};
    use relm4::gtk;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use crate::config::OrganismMotion;

    pub(super) const INTERACTION_HINT: &str = concat!(
        "Changes apply to open panes.\n",
        "In Full or Calm, keep the pointer near an idle companion for 600 ms ",
        "for a brief hello (about 2 seconds). Greetings are at least 8 seconds apart; ",
        "move away and return to try again.\n",
        "Typing, running commands and alternate-screen apps take priority. ",
        "Static uses inline cards only, without live hover greetings. ",
        "Clicks and selection stay with your terminal."
    );

    const FRAME_INTERVAL: Duration = Duration::from_millis(100);

    fn motion_for_selection(selected: u32, animations: bool) -> OrganismMotion {
        match selected {
            1 => OrganismMotion::Full,
            2 => OrganismMotion::Calm,
            3 => OrganismMotion::Static,
            _ if animations => OrganismMotion::Full,
            _ => OrganismMotion::Calm,
        }
    }

    fn motion_policy(selected: u32, animations: Option<bool>) -> &'static str {
        match (selected, animations) {
            (1, _) => "Full: animated movement and quiet hover greetings.",
            (2, _) => "Calm: still poses and quiet hover greetings.",
            (3, _) => "Static: inline cards only; no live hover greetings.",
            (_, Some(true)) => "Automatic (Full): follows desktop animations.",
            (_, Some(false)) => "Automatic (Calm): desktop animations are disabled.",
            (_, None) => "Automatic (Full): desktop preference unavailable.",
        }
    }

    fn selected_pose(index: u32) -> PreviewPose {
        PreviewPose::ALL
            .get(index as usize)
            .copied()
            .unwrap_or(PreviewPose::Calm)
    }

    fn can_greet(pose: PreviewPose) -> bool {
        matches!(
            pose,
            PreviewPose::Calm
                | PreviewPose::Curious
                | PreviewPose::Sleeping
                | PreviewPose::Greeting
        )
    }

    fn frame_index(motion: OrganismMotion, elapsed: Duration) -> u64 {
        if motion == OrganismMotion::Full {
            (elapsed.as_millis() / FRAME_INTERVAL.as_millis()) as u64
        } else {
            0
        }
    }

    fn preview_active(visible: bool, mapped: bool, host_active: Option<bool>) -> bool {
        visible && mapped && host_active == Some(true)
    }

    /// Still modes only wake for a requested greeting's expiry and cooldown.
    /// Visibility is a hard gate, including during the dialog close animation.
    fn next_wake(
        visible: bool,
        motion: OrganismMotion,
        now: Duration,
        greeting: Option<Duration>,
        last_hello: Option<Duration>,
    ) -> Option<Duration> {
        if !visible {
            return None;
        }
        let animation = (motion == OrganismMotion::Full).then_some(FRAME_INTERVAL);
        let expiry = greeting.and_then(|start| {
            start
                .saturating_add(GentleInteraction::HOLD)
                .checked_sub(now)
        });
        let cooldown = last_hello.and_then(|start| {
            start
                .saturating_add(GentleInteraction::COOLDOWN)
                .checked_sub(now)
        });
        [animation, expiry, cooldown]
            .into_iter()
            .flatten()
            .filter(|delay| !delay.is_zero())
            .min()
            // GLib rounds down to milliseconds; never spin on a sub-ms expiry.
            .map(|delay| delay.max(Duration::from_millis(1)))
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct DemoFrame {
        pose: PreviewPose,
        index: usize,
        next_at: Duration,
    }

    #[derive(Debug, Default)]
    struct PreviewSequence {
        started: Option<Duration>,
    }

    impl PreviewSequence {
        const POSES: [PreviewPose; 5] = [
            PreviewPose::Calm,
            PreviewPose::Working,
            PreviewPose::Concerned,
            PreviewPose::Success,
            PreviewPose::Sleeping,
        ];

        fn start(&mut self, now: Duration) {
            self.started = Some(now);
        }

        fn cancel(&mut self) {
            self.started = None;
        }

        fn sample(&mut self, now: Duration) -> Option<DemoFrame> {
            let started = self.started?;
            let elapsed = now.saturating_sub(started);
            if elapsed >= Duration::from_secs(10) {
                self.cancel();
                return None;
            }
            let index = (elapsed.as_secs() / 2) as usize;
            let Some(next_at) = started.checked_add(Duration::from_secs((index as u64 + 1) * 2))
            else {
                self.cancel();
                return None;
            };
            Some(DemoFrame {
                pose: Self::POSES[index],
                index,
                next_at,
            })
        }
    }

    fn preview_wake(
        active: bool,
        motion: OrganismMotion,
        now: Duration,
        greeting: Option<Duration>,
        last_hello: Option<Duration>,
        demo: Option<DemoFrame>,
    ) -> Option<Duration> {
        if !active {
            return None;
        }
        let (greeting, last_hello) = if demo.is_some() {
            (None, None)
        } else {
            (greeting, last_hello)
        };
        [
            next_wake(active, motion, now, greeting, last_hello),
            demo.and_then(|frame| frame.next_at.checked_sub(now)),
        ]
        .into_iter()
        .flatten()
        .min()
        .map(|delay| delay.max(Duration::from_millis(1)))
    }

    struct Preview {
        group: glib::WeakRef<adw::PreferencesGroup>,
        pose: glib::WeakRef<adw::ComboRow>,
        motion: glib::WeakRef<adw::ComboRow>,
        sample: glib::WeakRef<adw::ActionRow>,
        sprite: glib::WeakRef<gtk::Label>,
        hello: glib::WeakRef<gtk::Button>,
        demo_button: glib::WeakRef<gtk::Button>,
        sequence: RefCell<PreviewSequence>,
        desktop: Option<gtk::Settings>,
        desktop_handler: RefCell<Option<glib::SignalHandlerId>>,
        source: RefCell<Option<glib::SourceId>>,
        host_observer: RefCell<Option<(glib::WeakRef<gtk::Window>, glib::SignalHandlerId)>>,
        visible: Cell<bool>,
        epoch: Instant,
        greeting: Cell<Option<Duration>>,
        last_hello: Cell<Option<Duration>>,
        interaction: RefCell<GentleInteraction>,
    }

    impl Preview {
        fn stop_source(&self) {
            if let Some(source) = self.source.borrow_mut().take() {
                source.remove();
            }
        }

        fn cancel_greeting(&self) {
            self.interaction.borrow_mut().cancel();
            self.greeting.set(None);
        }

        fn cancel_demo(&self) {
            self.sequence.borrow_mut().cancel();
        }

        fn host_window(&self) -> Option<gtk::Window> {
            self.group.upgrade()?.root().and_downcast::<gtk::Window>()
        }

        fn active(&self) -> bool {
            preview_active(
                self.visible.get(),
                self.group.upgrade().is_some_and(|group| group.is_mapped()),
                self.host_window().map(|window| window.is_active()),
            )
        }

        fn disconnect_host(&self) {
            let observer = self.host_observer.borrow_mut().take();
            if let Some((window, handler)) = observer {
                if let Some(window) = window.upgrade() {
                    window.disconnect(handler);
                }
            }
        }

        fn observe_host(self: &Rc<Self>) {
            let host = self.visible.get().then(|| self.host_window()).flatten();
            let old_host = self
                .host_observer
                .borrow()
                .as_ref()
                .and_then(|(window, _)| window.upgrade());
            if host.is_some() && host == old_host {
                return;
            }
            self.disconnect_host();
            self.cancel_greeting();
            self.cancel_demo();
            if let Some(window) = host {
                let weak = Rc::downgrade(self);
                let handler = window.connect_is_active_notify(move |_| {
                    if let Some(preview) = weak.upgrade() {
                        preview.refresh();
                    }
                });
                *self.host_observer.borrow_mut() = Some((window.downgrade(), handler));
            }
        }

        fn hide(&self) {
            self.visible.set(false);
            self.disconnect_host();
            self.stop_source();
            self.cancel_greeting();
            self.cancel_demo();
        }

        fn pose(&self) -> PreviewPose {
            selected_pose(self.pose.upgrade().map_or(0, |row| row.selected()))
        }

        fn refresh(self: &Rc<Self>) {
            self.stop_source();
            let (
                Some(_group),
                Some(pose_row),
                Some(motion_row),
                Some(sample),
                Some(sprite),
                Some(hello),
                Some(demo_button),
            ) = (
                self.group.upgrade(),
                self.pose.upgrade(),
                self.motion.upgrade(),
                self.sample.upgrade(),
                self.sprite.upgrade(),
                self.hello.upgrade(),
                self.demo_button.upgrade(),
            )
            else {
                return;
            };
            let active = self.active();
            if !active {
                self.cancel_greeting();
                self.cancel_demo();
            }
            let now = self.epoch.elapsed();
            let manual_pose = self.pose();
            let demo = self.sequence.borrow_mut().sample(now);
            let pose = demo.map_or(manual_pose, |frame| frame.pose);
            let animations = self
                .desktop
                .as_ref()
                .map(|settings| settings.is_gtk_enable_animations());
            let motion = motion_for_selection(motion_row.selected(), animations.unwrap_or(true));
            let policy = motion_policy(motion_row.selected(), animations);
            if motion_row.subtitle().as_deref() != Some(policy) {
                motion_row.set_subtitle(policy);
            }
            let context = if demo.is_some() {
                pose.context()
            } else {
                self.interaction.borrow_mut().apply(now, pose.context())
            };
            let frame = sprite_frame_with_context(context, frame_index(motion, now));
            if sprite.text().as_str() != frame.as_ref() {
                sprite.set_text(frame.as_ref());
            }
            if pose_row.subtitle().as_deref() != Some(manual_pose.explanation()) {
                pose_row.set_subtitle(manual_pose.explanation());
            }
            let greeting = self
                .greeting
                .get()
                .filter(|start| now.saturating_sub(*start) < GentleInteraction::HOLD);
            self.greeting.set(greeting);
            let cooling_down = self
                .last_hello
                .get()
                .is_some_and(|start| now.saturating_sub(start) < GentleInteraction::COOLDOWN);
            hello.set_sensitive(active && demo.is_none() && can_greet(pose) && !cooling_down);
            demo_button.set_sensitive(active);
            let demo_label = if demo.is_some() {
                "Stop demo"
            } else {
                "Play demo"
            };
            if demo_button.label().as_deref() != Some(demo_label) {
                demo_button.set_label(demo_label);
            }
            let motion_note = match motion {
                OrganismMotion::Full => "Full motion: animated example.",
                OrganismMotion::Calm => "Calm: still poses, with no frame animation.",
                OrganismMotion::Static => {
                    "Static: still example; the live companion uses inline cards only."
                }
            };
            let interaction_note = if demo.is_some() {
                "Demo examples only, not terminal results. Stop the demo before saying hello."
            } else if greeting.is_some() {
                "Hello! Returning to your chosen pose in a moment."
            } else if !can_greet(pose) {
                "Choose a quiet pose to try a greeting."
            } else if cooling_down {
                "A little rest before another hello."
            } else {
                "Say hello for a brief, local response."
            };
            let motion_title = match motion {
                OrganismMotion::Full => "Full motion",
                OrganismMotion::Calm => "Calm motion",
                OrganismMotion::Static => "Static motion",
            };
            let short_status = if let Some(frame) = demo {
                format!(
                    "Demo {}/5: {} · example",
                    frame.index + 1,
                    frame.pose.label()
                )
            } else if greeting.is_some() {
                "Hello!".to_owned()
            } else if !can_greet(pose) {
                "Let it settle".to_owned()
            } else if cooling_down {
                "Resting…".to_owned()
            } else {
                "Local preview".to_owned()
            };
            if sample.title().as_str() != motion_title
                || sample.subtitle().as_deref() != Some(short_status.as_str())
            {
                sample.set_title(motion_title);
                sample.set_subtitle(&short_status);
                sample.set_tooltip_text(Some(&format!("{motion_note}\n{interaction_note}")));
            }
            if let Some(delay) = preview_wake(
                active,
                motion,
                now,
                greeting,
                can_greet(pose).then(|| self.last_hello.get()).flatten(),
                demo,
            ) {
                let weak = Rc::downgrade(self);
                let source = glib::timeout_add_local_once(delay, move || {
                    if let Some(preview) = weak.upgrade() {
                        // The one-shot has fired; never remove a stale source id.
                        preview.source.borrow_mut().take();
                        preview.refresh();
                    }
                });
                *self.source.borrow_mut() = Some(source);
            }
        }
    }

    impl Drop for Preview {
        fn drop(&mut self) {
            self.disconnect_host();
            self.stop_source();
            if let (Some(desktop), Some(handler)) =
                (&self.desktop, self.desktop_handler.borrow_mut().take())
            {
                desktop.disconnect(handler);
            }
        }
    }

    pub(super) fn install(
        group: &adw::PreferencesGroup,
        motion: &adw::ComboRow,
        dialog: &adw::PreferencesDialog,
    ) {
        build(group, motion, dialog);
    }

    fn build(
        group: &adw::PreferencesGroup,
        motion: &adw::ComboRow,
        dialog: &adw::PreferencesDialog,
    ) -> Rc<Preview> {
        group.set_title("Organism Preview");
        group.set_description(Some("Preview eight offline moods, even when your companion is off. This never runs commands or changes its memory."));
        let labels: Vec<_> = PreviewPose::ALL.iter().map(|pose| pose.label()).collect();
        let pose = adw::ComboRow::builder()
            .title("Pose")
            .model(&gtk::StringList::new(&labels))
            .selected(0)
            .build();
        let sprite = gtk::Label::builder()
            .width_chars(10)
            .height_request(76)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .can_target(false)
            .focusable(false)
            .build();
        sprite.add_css_class("monospace");
        sprite.update_property(&[gtk::accessible::Property::Label(
            "ASCII organism pose preview",
        )]);
        let sample = adw::ActionRow::builder().title("Try a greeting").build();
        sample.add_prefix(&sprite);
        let hello = gtk::Button::builder()
            .label("Say hello")
            .valign(gtk::Align::Center)
            .build();
        hello.set_tooltip_text(Some(
            "A brief preview-only greeting. Does not send terminal input.",
        ));
        sample.add_suffix(&hello);
        let demo_button = gtk::Button::builder()
            .label("Play demo")
            .valign(gtk::Align::Center)
            .build();
        demo_button.set_tooltip_text(Some(
            "A ten-second offline example, not real terminal results. Stop at any time.",
        ));
        sample.add_suffix(&demo_button);
        group.add(&pose);
        group.add(&sample);
        let preview = Rc::new(Preview {
            group: group.downgrade(),
            pose: pose.downgrade(),
            motion: motion.downgrade(),
            sample: sample.downgrade(),
            sprite: sprite.downgrade(),
            hello: hello.downgrade(),
            demo_button: demo_button.downgrade(),
            sequence: RefCell::new(PreviewSequence::default()),
            desktop: gtk::Settings::default(),
            desktop_handler: RefCell::new(None),
            source: RefCell::new(None),
            host_observer: RefCell::new(None),
            visible: Cell::new(false),
            epoch: Instant::now(),
            greeting: Cell::new(None),
            last_hello: Cell::new(None),
            interaction: RefCell::new(GentleInteraction::default()),
        });
        pose.connect_selected_notify({
            let preview = preview.clone();
            move |_| {
                preview.cancel_greeting();
                preview.cancel_demo();
                preview.refresh();
            }
        });
        hello.connect_clicked({
            let preview = preview.clone();
            move |_| {
                if !preview.active() {
                    return;
                }
                let now = preview.epoch.elapsed();
                if preview.sequence.borrow_mut().sample(now).is_some() {
                    return;
                }
                if preview
                    .interaction
                    .borrow_mut()
                    .request(now, preview.pose().context())
                {
                    preview.greeting.set(Some(now));
                    preview.last_hello.set(Some(now));
                }
                preview.refresh();
            }
        });
        demo_button.connect_clicked({
            let weak = Rc::downgrade(&preview);
            move |_| {
                let Some(preview) = weak.upgrade() else {
                    return;
                };
                if !preview.active() {
                    return;
                }
                let now = preview.epoch.elapsed();
                let playing = preview.sequence.borrow_mut().sample(now).is_some();
                if playing {
                    preview.cancel_demo();
                } else {
                    preview.cancel_greeting();
                    preview.sequence.borrow_mut().start(now);
                }
                preview.refresh();
            }
        });
        group.connect_map({
            let preview = preview.clone();
            move |_| {
                preview.visible.set(true);
                preview.observe_host();
                preview.refresh();
            }
        });
        group.connect_root_notify({
            let weak = Rc::downgrade(&preview);
            move |_| {
                if let Some(preview) = weak.upgrade() {
                    preview.observe_host();
                    preview.refresh();
                }
            }
        });
        group.connect_unmap({
            let preview = preview.clone();
            move |_| preview.hide()
        });
        dialog.connect_closed({
            let preview = preview.clone();
            move |_| preview.hide()
        });
        motion.connect_selected_notify({
            let weak = Rc::downgrade(&preview);
            move |_| {
                if let Some(preview) = weak.upgrade() {
                    preview.refresh();
                }
            }
        });
        if let Some(desktop) = &preview.desktop {
            let weak = Rc::downgrade(&preview);
            let handler = desktop.connect_gtk_enable_animations_notify(move |_| {
                if let Some(preview) = weak.upgrade() {
                    preview.refresh();
                }
            });
            *preview.desktop_handler.borrow_mut() = Some(handler);
        }
        preview.refresh();
        preview
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        #[ignore = "requires DISPLAY"]
        fn preview_lifecycle_keeps_motion_local_and_stops_hidden_sources() {
            gtk::init().expect("GTK display");
            adw::init().expect("Adwaita initialization");
            let desktop = gtk::Settings::default().expect("desktop settings");
            let original_animations = desktop.is_gtk_enable_animations();
            desktop.set_gtk_enable_animations(false);
            let entry = gtk::Entry::new();
            entry.set_text("untouched terminal draft");
            let view = crate::block_view::organism_settings_test_view();
            let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
            content.append(&entry);
            content.append(&view.widget());
            // Use the same dialog-capable Adwaita host as the application.
            // A plain GtkWindow exercises AdwDialog's separate-window fallback.
            let window = adw::ApplicationWindow::builder()
                .default_width(800)
                .default_height(600)
                .content(&content)
                .build();
            window.present();
            let dialog = adw::PreferencesDialog::new();
            dialog.set_title("Settings");
            let page = adw::PreferencesPage::new();
            let group = adw::PreferencesGroup::new();
            // Disabled live controls simulate companion-off or safe mode. The
            // gallery has no enable/config callback and remains usable.
            let motion = adw::ComboRow::builder()
                .model(&gtk::StringList::new(&[
                    "Automatic",
                    "Full",
                    "Calm",
                    "Static",
                ]))
                .selected(1)
                .sensitive(false)
                .build();
            let preview = build(&group, &motion, &dialog);
            page.add(&group);
            dialog.add(&page);
            dialog.present(Some(&window));
            let main = glib::MainContext::default();
            let spin_until = |condition: &dyn Fn() -> bool| {
                let deadline = Instant::now() + Duration::from_secs(4);
                while !condition() && Instant::now() < deadline {
                    while main.pending() {
                        main.iteration(false);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert!(condition(), "GTK condition did not settle");
            };
            spin_until(&|| preview.active());
            let reflected_enabled = adw::SwitchRow::new();
            let syncing = Rc::new(Cell::new(false));
            let reflected_outputs = Rc::new(Cell::new(0));
            reflected_enabled.connect_active_notify({
                let syncing = syncing.clone();
                let outputs = reflected_outputs.clone();
                move |_| {
                    if !syncing.get() {
                        outputs.set(outputs.get() + 1);
                    }
                }
            });
            motion.connect_selected_notify({
                let syncing = syncing.clone();
                let outputs = reflected_outputs.clone();
                move |_| {
                    if !syncing.get() {
                        outputs.set(outputs.get() + 1);
                    }
                }
            });
            super::super::reflect_ascii_organism_controls(
                &syncing,
                &reflected_enabled,
                &motion,
                false,
                3,
                false,
            );
            assert!(!reflected_enabled.is_active());
            assert_eq!(motion.selected(), 3);
            assert!(!motion.is_sensitive());
            assert_eq!(
                motion.subtitle().as_deref(),
                Some(motion_policy(3, Some(false)))
            );
            assert!(preview.source.borrow().is_none());
            super::super::reflect_ascii_organism_controls(
                &syncing,
                &reflected_enabled,
                &motion,
                true,
                0,
                false,
            );
            assert!(reflected_enabled.is_active());
            assert!(motion.is_sensitive());
            assert_eq!(
                motion.subtitle().as_deref(),
                Some(motion_policy(0, Some(false)))
            );
            assert_eq!(
                reflected_outputs.get(),
                0,
                "reflection must not queue settings output"
            );
            assert!(!syncing.get());
            // Restore the fixture's original disabled-live, explicit Full setup.
            super::super::reflect_ascii_organism_controls(
                &syncing,
                &reflected_enabled,
                &motion,
                false,
                1,
                false,
            );
            assert_eq!(reflected_outputs.get(), 0);
            reflected_enabled.set_active(true);
            assert_eq!(
                reflected_outputs.get(),
                1,
                "later user changes remain observable"
            );
            reflected_enabled.set_active(false);
            // Discard fixture initialization bytes; the following interactions
            // must never write into this nonexecuting terminal's PTY.
            crate::block_view::organism_settings_test_pty_bytes(&view);
            assert!(group.is_sensitive());
            assert!(preview.source.borrow().is_some());
            // Optional native screenshot capture window; ordinary CI never waits.
            if let Some(pause) = std::env::var("ORGANISM_PREVIEW_SCREENSHOT_PAUSE_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
            {
                eprintln!("ORGANISM_PREVIEW_READY");
                let until = Instant::now() + Duration::from_millis(pause.min(30_000));
                while Instant::now() < until {
                    while main.pending() {
                        main.iteration(false);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            let pose = preview.pose.upgrade().unwrap();
            let sprite = preview.sprite.upgrade().unwrap();
            let hello = preview.hello.upgrade().unwrap();
            let demo_button = preview.demo_button.upgrade().unwrap();
            let sample = preview.sample.upgrade().unwrap();
            assert!(!sprite.can_target());
            assert!(!sprite.is_focusable());

            motion.set_selected(3);
            assert!(preview.source.borrow().is_none());
            for (index, example) in PreviewPose::ALL.into_iter().enumerate() {
                pose.set_selected(index as u32);
                assert_eq!(pose.subtitle().as_deref(), Some(example.explanation()));
                assert_eq!(
                    sprite.text().as_str(),
                    sprite_frame_with_context(example.context(), 0).as_ref()
                );
            }
            pose.set_selected(0);
            let previous_hello = preview.last_hello.get();
            demo_button.emit_clicked();
            assert!(preview.sequence.borrow().started.is_some());
            assert!(!hello.is_sensitive());
            hello.emit_clicked();
            assert!(preview.greeting.get().is_none());
            assert_eq!(preview.last_hello.get(), previous_hello);
            spin_until(&|| sample.subtitle().as_deref() == Some("Demo 2/5: Working · example"));
            assert_eq!(pose.selected(), 0, "demo never changes the manual pose");
            assert_eq!(
                sprite.text().as_str(),
                sprite_frame_with_context(PreviewPose::Working.context(), 0).as_ref()
            );
            demo_button.emit_clicked();
            assert!(preview.sequence.borrow().started.is_none());
            assert_eq!(demo_button.label().as_deref(), Some("Play demo"));
            assert_eq!(
                sprite.text().as_str(),
                sprite_frame_with_context(PreviewPose::Calm.context(), 0).as_ref()
            );
            assert!(preview.source.borrow().is_none());
            demo_button.emit_clicked();
            pose.set_selected(1);
            assert!(preview.sequence.borrow().started.is_none());
            assert_eq!(demo_button.label().as_deref(), Some("Play demo"));
            assert_eq!(
                sprite.text().as_str(),
                sprite_frame_with_context(PreviewPose::Curious.context(), 0).as_ref()
            );
            pose.set_selected(0);
            pose.grab_focus();
            let focus_before = gtk::prelude::GtkWindowExt::focus(&window);
            assert!(
                focus_before.is_some(),
                "the settings control owns keyboard focus"
            );
            hello.emit_clicked();
            let accepted = preview.greeting.get();
            assert!(accepted.is_some());
            assert!(!hello.is_sensitive());
            hello.emit_clicked(); // Even a repeated/programmatic click is bounded.
            assert_eq!(preview.greeting.get(), accepted);
            assert_eq!(gtk::prelude::GtkWindowExt::focus(&window), focus_before);
            assert_eq!(
                sprite.text().as_str(),
                sprite_frame_with_context(PreviewPose::Greeting.context(), 0).as_ref()
            );
            spin_until(&|| preview.greeting.get().is_none());
            assert_eq!(
                sprite.text().as_str(),
                sprite_frame_with_context(PreviewPose::Calm.context(), 0).as_ref()
            );
            assert!(
                preview.source.borrow().is_some(),
                "one cooldown wake remains"
            );
            pose.set_selected(2);
            assert!(preview.greeting.get().is_none());
            assert!(!hello.is_sensitive());
            assert!(
                preview.source.borrow().is_none(),
                "busy still poses do not poll a cooldown"
            );
            hello.emit_clicked();
            assert!(preview.greeting.get().is_none());

            motion.set_selected(0);
            assert!(
                preview.source.borrow().is_none(),
                "automatic follows reduced motion"
            );
            desktop.set_gtk_enable_animations(true);
            assert!(
                preview.source.borrow().is_some(),
                "desktop preference updates live"
            );
            desktop.set_gtk_enable_animations(false);
            assert!(preview.source.borrow().is_none());
            motion.set_selected(1);
            assert!(preview.source.borrow().is_some());
            demo_button.emit_clicked();
            assert!(preview.sequence.borrow().started.is_some());
            group.set_visible(false);
            assert!(preview.source.borrow().is_none());
            assert!(preview.sequence.borrow().started.is_none());
            group.set_visible(true);
            spin_until(&|| preview.active());
            assert!(preview.source.borrow().is_some());
            assert!(preview.sequence.borrow().started.is_none());
            assert_eq!(sample.subtitle().as_deref(), Some("Let it settle"));
            demo_button.emit_clicked();
            assert!(preview.sequence.borrow().started.is_some());
            dialog.force_close();
            spin_until(&|| !group.is_mapped());
            assert!(preview.source.borrow().is_none());
            // Reopening the same dialog starts exactly one mapped source,
            // but never resumes the demo that closing canceled.
            assert!(preview.sequence.borrow().started.is_none());
            dialog.present(Some(&window));
            spin_until(&|| preview.active());
            assert!(preview.source.borrow().is_some());
            assert!(preview.sequence.borrow().started.is_none());
            assert_eq!(sample.subtitle().as_deref(), Some("Let it settle"));
            dialog.force_close();
            spin_until(&|| !group.is_mapped());
            assert!(preview.source.borrow().is_none());
            assert_eq!(entry.text().as_str(), "untouched terminal draft");
            assert!(
                crate::block_view::organism_settings_test_pty_bytes(&view).is_empty(),
                "pose preview and greeting must not send terminal input"
            );
            assert_eq!(
                motion.selected(),
                1,
                "preview never edits the motion control"
            );
            desktop.set_gtk_enable_animations(original_animations);
            assert!(
                preview.source.borrow().is_none(),
                "hidden settings callbacks cannot restart a timer"
            );
            window.close();
        }

        #[test]
        fn demo_sequence_has_five_bounded_stages_and_never_catches_up() {
            let mut sequence = PreviewSequence::default();
            sequence.start(Duration::ZERO);
            for (millis, index, pose, boundary) in [
                (0, 0, PreviewPose::Calm, 2),
                (1_999, 0, PreviewPose::Calm, 2),
                (2_000, 1, PreviewPose::Working, 4),
                (4_000, 2, PreviewPose::Concerned, 6),
                (6_000, 3, PreviewPose::Success, 8),
                (9_999, 4, PreviewPose::Sleeping, 10),
            ] {
                assert_eq!(
                    sequence.sample(Duration::from_millis(millis)),
                    Some(DemoFrame {
                        pose,
                        index,
                        next_at: Duration::from_secs(boundary),
                    })
                );
            }
            assert_eq!(sequence.sample(Duration::from_secs(10)), None);
            assert_eq!(sequence.sample(Duration::from_secs(60)), None);
            sequence.start(Duration::from_secs(60));
            assert_eq!(sequence.sample(Duration::from_secs(90)), None);
            assert!(sequence.started.is_none());
        }

        #[test]
        fn demo_cancel_restores_manual_pose_without_resuming() {
            let manual = PreviewPose::Curious;
            let mut sequence = PreviewSequence::default();
            sequence.start(Duration::from_secs(5));
            assert_eq!(
                sequence.sample(Duration::from_secs(7)).unwrap().pose,
                PreviewPose::Working
            );
            sequence.cancel();
            for now in [7, 8, 30] {
                assert_eq!(
                    sequence
                        .sample(Duration::from_secs(now))
                        .map_or(manual, |frame| frame.pose),
                    manual
                );
            }
            sequence.start(Duration::from_secs(40));
            assert_eq!(
                sequence.sample(Duration::from_secs(40)).unwrap().pose,
                PreviewPose::Calm
            );
            sequence.start(Duration::MAX);
            assert_eq!(sequence.sample(Duration::MAX), None);
        }

        #[test]
        fn demo_still_modes_wake_at_the_final_boundary_and_then_stop() {
            let mut sequence = PreviewSequence::default();
            sequence.start(Duration::ZERO);
            for motion in [OrganismMotion::Calm, OrganismMotion::Static] {
                let now = Duration::from_secs(8);
                let frame = sequence.sample(now);
                assert_eq!(
                    preview_wake(true, motion, now, None, None, frame),
                    Some(Duration::from_secs(2))
                );
                assert_eq!(preview_wake(false, motion, now, None, None, frame), None);
                assert_eq!(
                    preview_wake(true, motion, now, None, Some(Duration::from_secs(1)), frame),
                    Some(Duration::from_secs(2))
                );
                assert_eq!(frame_index(motion, now), 0);
            }
            let now = Duration::from_secs(10);
            assert_eq!(sequence.sample(now), None);
            assert_eq!(
                preview_wake(true, OrganismMotion::Static, now, None, None, None),
                None
            );
        }

        #[test]
        fn demo_wiring_is_explicit_cancellable_and_isolated_from_hello() {
            let source = include_str!("settings.rs");
            let production = source.split("#[cfg(test)]").next().unwrap();
            let start = production
                .split("demo_button.connect_clicked")
                .nth(1)
                .unwrap()
                .split("group.connect_map")
                .next()
                .unwrap();
            assert!(start.contains("if !preview.active()"));
            assert!(start.contains("preview.cancel_greeting()"));
            assert!(start.contains("preview.sequence.borrow_mut().start(now)"));
            assert!(start.contains("preview.cancel_demo()"));
            assert!(!start.contains(".request("));
            assert!(!start.contains("last_hello"));
            assert!(!start.contains("set_selected"));
            for marker in [
                "fn observe_host",
                "fn hide(&self)",
                "pose.connect_selected_notify",
            ] {
                let body = production.split(marker).nth(1).unwrap();
                assert!(body
                    .split("preview.refresh()")
                    .next()
                    .unwrap()
                    .contains("cancel_demo()"));
            }
            assert!(production.contains("active && demo.is_none() && can_greet(pose)"));
            assert!(production.contains("Demo {}/5: {} · example"));
            assert!(production.contains("pose_row.set_subtitle(manual_pose.explanation())"));
            let hello = production
                .split("hello.connect_clicked")
                .nth(1)
                .unwrap()
                .split("demo_button.connect_clicked")
                .next()
                .unwrap();
            assert!(hello.contains("preview.sequence.borrow_mut().sample(now).is_some()"));
        }

        #[test]
        fn interaction_hint_matches_live_timing_and_input_ownership() {
            assert_eq!(GentleInteraction::HOLD, Duration::from_secs(2));
            assert_eq!(GentleInteraction::COOLDOWN, Duration::from_secs(8));
            let runtime = include_str!("../organism_ui.rs");
            let production = runtime.split("#[cfg(test)]\nmod tests {").next().unwrap();
            assert!(production
                .contains("const POINTER_GREETING_DWELL: Duration = Duration::from_millis(600);"));
            for detail in [
                "Full or Calm",
                "600 ms",
                "about 2 seconds",
                "at least 8 seconds",
                "move away and return",
                "Static uses inline cards only",
                "Clicks and selection stay with your terminal.",
            ] {
                assert!(INTERACTION_HINT.contains(detail));
            }
            let settings = include_str!("settings.rs");
            let component = settings
                .split("\n#[relm4::component(pub(crate))]")
                .nth(1)
                .unwrap();
            assert!(
                component.contains("set_tooltip_text: Some(organism_preview::INTERACTION_HINT)")
            );
        }

        #[test]
        fn inactive_preview_has_no_wake_even_with_a_pending_greeting() {
            for visible in [false, true] {
                for mapped in [false, true] {
                    for host in [None, Some(false), Some(true)] {
                        let active = preview_active(visible, mapped, host);
                        assert_eq!(active, visible && mapped && host == Some(true));
                        if !active {
                            for motion in [
                                OrganismMotion::Full,
                                OrganismMotion::Calm,
                                OrganismMotion::Static,
                            ] {
                                assert_eq!(
                                    next_wake(
                                        active,
                                        motion,
                                        Duration::ZERO,
                                        Some(Duration::ZERO),
                                        Some(Duration::ZERO),
                                    ),
                                    None
                                );
                            }
                        }
                    }
                }
            }
        }

        #[test]
        fn canceled_preview_greeting_does_not_replay_or_reset_cooldown() {
            let mut interaction = GentleInteraction::default();
            let pose = PreviewPose::Calm.context();
            assert!(interaction.request(Duration::ZERO, pose));
            interaction.cancel();
            assert_eq!(interaction.apply(Duration::from_secs(1), pose), pose);
            assert!(!interaction.request(Duration::from_secs(1), pose));
            assert!(interaction.request(GentleInteraction::COOLDOWN, pose));
        }

        #[test]
        fn preview_focus_lifecycle_uses_actual_host_and_preserves_cooldown() {
            let source = include_str!("settings.rs");
            let production = source.split("#[cfg(test)]").next().unwrap();
            assert!(production.contains("root().and_downcast::<gtk::Window>()"));
            assert!(production.contains("window.connect_is_active_notify"));
            assert!(production.contains("group.connect_root_notify"));
            assert!(production.contains("host_observer.borrow_mut().take()"));
            assert!(production.contains("window.disconnect(handler)"));
            let cancel = production
                .split("fn cancel_greeting(&self)")
                .nth(1)
                .unwrap()
                .split("fn host_window")
                .next()
                .unwrap();
            assert!(cancel.contains("self.interaction.borrow_mut().cancel()"));
            assert!(cancel.contains("self.greeting.set(None)"));
            assert!(!cancel.contains("last_hello"));
            let refresh = production
                .split("fn refresh(self: &Rc<Self>)")
                .nth(1)
                .unwrap();
            assert!(refresh.contains("if !active {\n                self.cancel_greeting();"));
            assert!(refresh.contains("hello.set_sensitive(active &&"));
            let click = production.split("hello.connect_clicked").nth(1).unwrap();
            assert!(click.contains("if !preview.active() {"));
        }

        #[test]
        fn motion_policy_explains_effective_mode_and_explicit_overrides() {
            assert_eq!(
                motion_policy(0, Some(true)),
                "Automatic (Full): follows desktop animations."
            );
            assert_eq!(
                motion_policy(0, Some(false)),
                "Automatic (Calm): desktop animations are disabled."
            );
            for selected in 1..=3 {
                assert_eq!(
                    motion_policy(selected, Some(true)),
                    motion_policy(selected, Some(false))
                );
                assert_eq!(
                    motion_policy(selected, None),
                    motion_policy(selected, Some(true))
                );
            }
            assert_eq!(
                motion_policy(3, Some(true)),
                "Static: inline cards only; no live hover greetings."
            );
            assert_eq!(
                motion_policy(0, None),
                "Automatic (Full): desktop preference unavailable."
            );
            for animations in [None, Some(false), Some(true)] {
                assert_eq!(
                    motion_policy(u32::MAX, animations),
                    motion_policy(0, animations)
                );
            }
        }

        #[test]
        fn motion_policy_refresh_uses_existing_selection_and_desktop_notifications() {
            let source = include_str!("settings.rs");
            let production = source.split("#[cfg(test)]").next().unwrap();
            let refresh = production
                .split("fn refresh(self: &Rc<Self>)")
                .nth(1)
                .unwrap()
                .split("impl Drop for Preview")
                .next()
                .unwrap();
            assert!(refresh.contains("motion_policy(motion_row.selected(), animations)"));
            assert!(refresh.contains("motion_row.subtitle().as_deref() != Some(policy)"));
            assert!(refresh.contains("motion_row.set_subtitle(policy)"));
            let build = production.split("fn build(").nth(1).unwrap();
            for signal in [
                "motion.connect_selected_notify",
                "desktop.connect_gtk_enable_animations_notify",
            ] {
                let callback = build.split(signal).nth(1).unwrap();
                assert!(callback
                    .split("});")
                    .next()
                    .unwrap()
                    .contains("preview.refresh()"));
            }
        }

        #[test]
        fn automatic_respects_reduced_motion_and_explicit_choices_win() {
            assert_eq!(motion_for_selection(0, false), OrganismMotion::Calm);
            assert_eq!(motion_for_selection(0, true), OrganismMotion::Full);
            assert_eq!(motion_for_selection(1, false), OrganismMotion::Full);
            assert_eq!(motion_for_selection(2, true), OrganismMotion::Calm);
            assert_eq!(motion_for_selection(3, true), OrganismMotion::Static);
        }

        #[test]
        fn still_modes_never_advance_frames_or_keep_an_idle_timer() {
            for motion in [OrganismMotion::Calm, OrganismMotion::Static] {
                assert_eq!(frame_index(motion, Duration::from_secs(9)), 0);
                assert_eq!(next_wake(true, motion, Duration::ZERO, None, None), None);
            }
            assert_eq!(
                frame_index(OrganismMotion::Full, Duration::from_secs(1)),
                10
            );
        }

        #[test]
        fn greeting_expiry_and_cooldown_are_bounded_even_in_static_mode() {
            let start = Duration::ZERO;
            let motion = OrganismMotion::Static;
            assert_eq!(
                next_wake(true, motion, start, Some(start), Some(start)),
                Some(GentleInteraction::HOLD)
            );
            assert_eq!(
                next_wake(
                    true,
                    motion,
                    GentleInteraction::HOLD,
                    Some(start),
                    Some(start)
                ),
                Some(GentleInteraction::COOLDOWN - GentleInteraction::HOLD)
            );
            assert_eq!(
                next_wake(
                    true,
                    motion,
                    GentleInteraction::COOLDOWN,
                    Some(start),
                    Some(start)
                ),
                None
            );
        }

        #[test]
        fn hiding_cancels_every_kind_of_preview_wake() {
            for motion in [
                OrganismMotion::Full,
                OrganismMotion::Calm,
                OrganismMotion::Static,
            ] {
                assert_eq!(
                    next_wake(
                        false,
                        motion,
                        Duration::ZERO,
                        Some(Duration::ZERO),
                        Some(Duration::ZERO)
                    ),
                    None
                );
            }
        }

        #[test]
        fn work_previews_do_not_offer_a_greeting_and_invalid_selection_is_safe() {
            assert_eq!(selected_pose(u32::MAX), PreviewPose::Calm);
            for (index, pose) in PreviewPose::ALL.into_iter().enumerate() {
                assert_eq!(selected_pose(index as u32), pose);
                let mut interaction = GentleInteraction::default();
                assert_eq!(
                    can_greet(pose),
                    interaction.request(Duration::ZERO, pose.context())
                );
            }
        }
    }
}

const EDIT_REMOTE_HOST_LABEL: &str = "Edit remote host";
const REMOVE_REMOTE_HOST_LABEL: &str = "Remove remote host";

#[derive(Debug, Clone)]
pub(crate) struct SettingsValues {
    pub(crate) theme: u32,
    pub(crate) font: u32,
    pub(crate) font_size: f64,
    pub(crate) font_scale: f64,
    pub(crate) opacity: f64,
    pub(crate) scrollback: f64,
    pub(crate) terminal_mode: u32,
    pub(crate) block_compact: bool,
    pub(crate) command_history: bool,
    pub(crate) ascii_organism_enabled: bool,
    /// 0 automatic, 1 full, 2 calm, 3 static.
    pub(crate) ascii_organism_motion: u32,
    pub(crate) ai_enabled: bool,
    pub(crate) ai_panel_visible: bool,
    pub(crate) ai_panel_width: f64,
    pub(crate) agent_enabled: bool,
    pub(crate) command_correction_enabled: bool,
    pub(crate) ai_provider: u32,
    pub(crate) ai_model: String,
    pub(crate) ai_base_url: String,
    /// TOML-configured key path (never the environment override); the write
    /// target when the API Key row stores a pasted key.
    pub(crate) ai_api_key_file: Option<String>,
    pub(crate) ai_max_tokens: f64,
    pub(crate) ai_redact_secrets: bool,
    pub(crate) ai_stream: bool,
    pub(crate) agent_max_turns: f64,
    pub(crate) safe_mode: bool,
    pub(crate) notifications: bool,
    pub(crate) remote_clipboard: bool,
    pub(crate) remote_hosts: Vec<RemoteHost>,
}

/// Unsubmitted host-form state, for a new entry or an existing one under edit.
/// Lives beside `SettingsValues` rather than inside it so the two construction
/// sites only carry persisted state.
#[derive(Debug, Default)]
struct RemoteDraft {
    name: String,
    host: String,
    user: String,
    docker: bool,
    /// Index into the deploy combo: 0 off, 1 persist, 2 incognito.
    deploy: u32,
}

impl RemoteDraft {
    /// The form as it should read while `host` is being edited. Only the fields
    /// the form owns are copied; `ssh_args`, `session`, `remote_shell`,
    /// `login_shell`, `multiplex` and `deploy_artifact` have no widget here and
    /// are carried over untouched when the edit is saved.
    fn from_host(host: &RemoteHost) -> Self {
        Self {
            name: host.name.clone(),
            host: host.host.clone(),
            user: host.user.clone().unwrap_or_default(),
            docker: host.docker,
            deploy: match host.deploy {
                jterm_core::jsh_remote::Deploy::Persist => 1,
                jterm_core::jsh_remote::Deploy::Incognito => 2,
                _ => 0,
            },
        }
    }
}

/// Which form row carries the validation error, so only that entry turns red.
#[derive(Clone, Copy, Debug)]
enum RemoteField {
    Form,
    Name,
    Host,
    User,
}

/// Widgets owned by the independent Add/Edit dialog. Keeping them together
/// lets the Relm4 update loop validate and focus fields without putting
/// application state in GTK signal closures.
struct RemoteDialogUi {
    epoch: Rc<()>,
    dialog: adw::Dialog,
    name: adw::EntryRow,
    host: adw::EntryRow,
    user: adw::EntryRow,
    error: gtk::Label,
}

/// An opening owns its callbacks until its UI is taken. Holding the old token
/// prevents address reuse from making a delayed event belong to a new dialog.
fn remote_dialog_event_is_current(current: Option<&Rc<()>>, event: &Rc<()>) -> bool {
    current.is_some_and(|current| Rc::ptr_eq(current, event))
}

pub(crate) struct SettingsInit {
    pub(crate) theme_names: Vec<String>,
    pub(crate) font_names: Vec<String>,
    pub(crate) values: SettingsValues,
}

/// Build the font list around the description that is actually active.
///
/// Pango's generic `Monospace` family and configured fonts that are not
/// installed locally do not necessarily appear in `list_families()`. Keeping
/// the active family in the list prevents a size-only edit from silently
/// selecting whichever installed family happened to sort first.
pub(crate) fn font_choices(
    mut available_families: Vec<String>,
    current_family: &str,
) -> (Vec<String>, u32) {
    let current_family = match current_family.trim() {
        "" => "Monospace",
        family => family,
    };
    available_families.retain(|family| !family.trim().is_empty());
    if !available_families
        .iter()
        .any(|family| family.eq_ignore_ascii_case(current_family))
    {
        available_families.push(current_family.to_string());
    }
    available_families.sort_by_cached_key(|family| family.to_lowercase());
    available_families.dedup_by(|left, right| left.eq_ignore_ascii_case(right));

    let selected = available_families
        .iter()
        .position(|family| family.eq_ignore_ascii_case(current_family))
        .expect("the active font family was inserted above") as u32;
    (available_families, selected)
}

fn font_desc_for_choice(font_names: &[String], selected: u32, size: f64) -> String {
    let family = font_names
        .get(selected as usize)
        .map(String::as_str)
        .unwrap_or("Monospace");
    format!("{family} {}", size as i32)
}

#[derive(Debug)]
pub(crate) enum SettingsMsg {
    Toggle(SettingsValues, Vec<String>, adw::ApplicationWindow),
    SyncAsciiOrganism {
        enabled: bool,
        motion: u32,
    },
    Theme(u32),
    Font(u32),
    FontSize(f64),
    FontScale(f64),
    Opacity(f64),
    Scrollback(f64),
    TerminalMode(u32),
    BlockCompact(bool),
    CommandHistory(bool),
    AsciiOrganism(bool),
    AsciiOrganismMotion(u32),
    AiEnabled(bool),
    AiPanelVisible(bool),
    AiPanelWidth(f64),
    AgentEnabled(bool),
    CommandCorrection(bool),
    AiProvider(u32),
    AiModel(String),
    AiBaseUrl(String),
    AiApiKeyStore(String),
    AiMaxTokens(f64),
    AiRedactSecrets(bool),
    AiStream(bool),
    AgentMaxTurns(f64),
    Notifications(bool),
    RemoteClipboard(bool),
    RemoteHostName(Rc<()>, String),
    RemoteHostHost(Rc<()>, String),
    RemoteHostUser(Rc<()>, String),
    RemoteHostDocker(Rc<()>, bool),
    RemoteHostDeploy(Rc<()>, u32),
    RemoteHostOpenAdd,
    /// Commit the independent dialog: append or replace in place.
    RemoteHostSave(Rc<()>),
    /// Load an existing host into the form instead of starting a new one.
    RemoteHostEdit(CapturedRemoteProfile),
    /// Abandon an in-progress edit and leave the saved host as it was.
    RemoteHostCancel(Rc<()>),
    RemoteHostRemove(CapturedRemoteProfile),
    RemoteHostRemoveConfirmed(CapturedRemoteProfile),
}

#[derive(Debug)]
pub(crate) enum SettingsOutput {
    Theme(usize),
    FontDesc(String),
    FontScale(f64),
    Opacity(f64),
    Scrollback(u32),
    TerminalMode(usize),
    BlockCompact(bool),
    CommandHistory(bool),
    AsciiOrganism(bool),
    AsciiOrganismMotion(u32),
    AiEnabled(bool),
    AiPanelVisible(bool),
    AiPanelWidth(u32),
    AgentEnabled(bool),
    CommandCorrection(bool),
    AiProvider(usize),
    AiModel(String),
    AiBaseUrl(String),
    /// A key was stored into this path; the app records and persists it.
    AiApiKeyFile(String),
    AiMaxTokens(u32),
    AiRedactSecrets(bool),
    AiStream(bool),
    AgentMaxTurns(u32),
    Notifications(bool),
    RemoteClipboard(bool),
    /// The full list before and after an edit, so a reload cannot silently
    /// authorize an older dialog to replace newer remote profiles.
    RemoteHosts {
        expected: Vec<RemoteHost>,
        hosts: Vec<RemoteHost>,
    },
}

pub(crate) struct SettingsModel {
    theme_names: Vec<String>,
    font_names: Vec<String>,
    values: SettingsValues,
    organism_syncing: Rc<Cell<bool>>,
    remote_draft: RemoteDraft,
    /// Original immutable profile identity, separate from the editable draft.
    /// Only `None` represents an explicit Add operation.
    remote_editing: Option<CapturedRemoteProfile>,
    remote_dialog: Option<RemoteDialogUi>,
    /// Host rows currently added to the "Remote Hosts" group. The view! macro
    /// cannot express a dynamic list, so these are rebuilt imperatively.
    remote_rows: Vec<adw::ActionRow>,
}

#[relm4::component(pub(crate))]
impl Component for SettingsModel {
    type Init = SettingsInit;
    type Input = SettingsMsg;
    type Output = SettingsOutput;
    type CommandOutput = ();

    view! {
        root = adw::PreferencesDialog {
            set_title: "Settings",

            add = &adw::PreferencesPage {
                adw::PreferencesGroup {
                    set_title: "Appearance",

                    #[name(theme_row)]
                    adw::ComboRow {
                        set_title: "Theme",
                        set_model: Some(&gtk::StringList::new(
                            &model.theme_names.iter().map(String::as_str).collect::<Vec<_>>()
                        )),
                        set_selected: model.values.theme,
                        connect_selected_notify[sender] => move |row| {
                            sender.input(SettingsMsg::Theme(row.selected()));
                        },
                    },

                    #[name(font_row)]
                    adw::ComboRow {
                        set_title: "Font",
                        set_model: Some(&gtk::StringList::new(
                            &model.font_names.iter().map(String::as_str).collect::<Vec<_>>()
                        )),
                        set_selected: model.values.font,
                        connect_selected_notify[sender] => move |row| {
                            sender.input(SettingsMsg::Font(row.selected()));
                        },
                    },

                    #[name(font_size_row)]
                    adw::SpinRow::new(
                        Some(&gtk::Adjustment::new(
                            model.values.font_size, 6.0, 72.0, 1.0, 4.0, 0.0
                        )),
                        1.0,
                        0,
                    ) {
                        set_title: "Font Size",
                        connect_value_notify[sender] => move |row| {
                            sender.input(SettingsMsg::FontSize(row.value()));
                        },
                    },

                    #[name(font_scale_row)]
                    adw::SpinRow::new(
                        Some(&gtk::Adjustment::new(
                            model.values.font_scale, 0.1, 10.0, 0.025, 0.1, 0.0
                        )),
                        0.025,
                        3,
                    ) {
                        set_title: "Font Scale",
                        connect_value_notify[sender] => move |row| {
                            sender.input(SettingsMsg::FontScale(row.value()));
                        },
                    },

                    adw::ActionRow {
                        set_title: "Opacity",

                        #[name(opacity_scale)]
                        add_suffix = &gtk::Scale::with_range(
                            gtk::Orientation::Horizontal, 0.01, 1.0, 0.025
                        ) {
                            set_value: model.values.opacity,
                            set_hexpand: true,
                            set_size_request: (180, -1),
                            set_draw_value: true,
                            set_value_pos: gtk::PositionType::Left,
                            set_format_value_func: |_, value| format!("{:.0}%", value * 100.0),
                            connect_value_changed[sender] => move |scale| {
                                sender.input(SettingsMsg::Opacity(scale.value()));
                            },
                        },
                    },

                    #[name(scrollback_row)]
                    adw::SpinRow::new(
                        Some(&gtk::Adjustment::new(
                            model.values.scrollback, 0.0, 1_000_000.0, 100.0, 1000.0, 0.0
                        )),
                        100.0,
                        0,
                    ) {
                        set_title: "Scrollback Lines",
                        connect_value_notify[sender] => move |row| {
                            sender.input(SettingsMsg::Scrollback(row.value()));
                        },
                    },

                },

                adw::PreferencesGroup {
                    set_title: &gtk::glib::markup_escape_text("Terminal & Blocks"),

                    #[name(terminal_mode_row)]
                    adw::ComboRow {
                        set_title: "Terminal Backend",
                        set_subtitle: "Applies to new and restored local panes",
                        set_model: Some(&gtk::StringList::new(&[
                            "Block",
                            "VTE compatibility",
                            "Unified (experimental)",
                        ])),
                        set_selected: model.values.terminal_mode,
                        set_sensitive: !model.values.safe_mode,
                        connect_selected_notify[sender] => move |row| {
                            sender.input(SettingsMsg::TerminalMode(row.selected()));
                        },
                    },

                    #[name(block_compact_row)]
                    adw::SwitchRow {
                        set_title: "Compact Block Layout",
                        set_subtitle: "Denser spacing for blocks and the input cell",
                        set_active: model.values.block_compact,
                        set_sensitive: !model.values.safe_mode,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::BlockCompact(row.is_active()));
                        },
                    },

                    #[name(command_history_row)]
                    adw::SwitchRow {
                        set_title: "Command History Index",
                        set_subtitle: "Store commands, cwd and status; never terminal output",
                        set_active: model.values.command_history,
                        set_sensitive: !model.values.safe_mode,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::CommandHistory(row.is_active()));
                        },
                    },

                    #[name(ascii_organism_row)]
                    adw::SwitchRow {
                        set_title: "ASCII Organism",
                        set_subtitle: "Local, no-LLM companion for Block/Unified panes",
                        set_tooltip_text: Some(organism_preview::INTERACTION_HINT),
                        set_active: model.values.ascii_organism_enabled,
                        set_sensitive: !model.values.safe_mode,
                        connect_active_notify[sender, syncing = model.organism_syncing.clone()] => move |row| {
                            if !syncing.get() {
                                sender.input(SettingsMsg::AsciiOrganism(row.is_active()));
                            }
                        },
                    },

                    #[name(ascii_organism_motion_row)]
                    adw::ComboRow {
                        set_title: "Organism Motion",
                        set_subtitle: "Automatic follows the desktop animation preference",
                        set_model: Some(&gtk::StringList::new(
                            &["Automatic", "Full", "Calm", "Static"]
                        )),
                        set_selected: model.values.ascii_organism_motion,
                        set_sensitive: !model.values.safe_mode
                            && model.values.ascii_organism_enabled,
                        connect_selected_notify[sender, syncing = model.organism_syncing.clone()] => move |row| {
                            if !syncing.get() {
                                sender.input(SettingsMsg::AsciiOrganismMotion(row.selected()));
                            }
                        },
                    },
                },

                #[name(organism_preview_group)]
                adw::PreferencesGroup {},

                adw::PreferencesGroup {
                    set_title: &gtk::glib::markup_escape_text("Features & Privacy"),

                    #[name(notifications_row)]
                    adw::SwitchRow {
                        set_title: "Long-command Notifications",
                        set_active: model.values.notifications,
                        set_sensitive: !model.values.safe_mode,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::Notifications(row.is_active()));
                        },
                    },

                    #[name(remote_clipboard_row)]
                    adw::SwitchRow {
                        set_title: "Allow OSC 52 Clipboard Writes",
                        set_subtitle: "Enable only for trusted local and remote programs",
                        set_active: model.values.remote_clipboard,
                        set_sensitive: !model.values.safe_mode,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::RemoteClipboard(row.is_active()));
                        },
                    },
                },

                adw::PreferencesGroup {
                    set_title: &gtk::glib::markup_escape_text("AI & Agent"),
                    set_description: Some(
                        "Environment variables take priority. Keys entered here are stored in a private ai.key file, never in config.toml"
                    ),

                    #[name(ai_enabled_row)]
                    adw::SwitchRow {
                        set_title: "Enable AI Features",
                        set_active: model.values.ai_enabled,
                        set_sensitive: !model.values.safe_mode,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiEnabled(row.is_active()));
                        },
                    },

                    #[name(ai_panel_visible_row)]
                    adw::SwitchRow {
                        set_title: "Show AI Chats at Startup",
                        set_subtitle: "Keep the persistent right-side chat panel open",
                        set_active: model.values.ai_panel_visible,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiPanelVisible(row.is_active()));
                        },
                    },

                    #[name(ai_panel_width_row)]
                    adw::SpinRow::new(
                        Some(&gtk::Adjustment::new(
                            model.values.ai_panel_width, 240.0, 1_200.0, 10.0, 50.0, 0.0
                        )),
                        10.0,
                        0,
                    ) {
                        set_title: "AI Chats Width",
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_value_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiPanelWidth(row.value()));
                        },
                    },

                    #[name(agent_enabled_row)]
                    adw::SwitchRow {
                        set_title: "Enable Approval-gated Agent",
                        set_subtitle: "Every proposed command remains editable and requires approval",
                        set_active: model.values.agent_enabled,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AgentEnabled(row.is_active()));
                        },
                    },

                    adw::SwitchRow {
                        set_title: "Automatic Agent Execution Retired",
                        set_subtitle: "Every proposal requires explicit approval; command text cannot prove what aliases, helpers, or flags will execute",
                        set_active: false,
                        set_sensitive: false,
                    },

                    #[name(command_correction_row)]
                    adw::SwitchRow {
                        set_title: "Correct Mistyped Block Commands",
                        set_subtitle: "Offer an editable correction after typo-like failures; never run automatically",
                        set_active: model.values.command_correction_enabled,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::CommandCorrection(row.is_active()));
                        },
                    },

                    #[name(ai_provider_row)]
                    adw::ComboRow {
                        set_title: "Provider",
                        set_model: Some(&gtk::StringList::new(
                            &["Anthropic", "OpenAI-compatible", "Ollama"]
                        )),
                        set_selected: model.values.ai_provider,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_selected_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiProvider(row.selected()));
                        },
                    },

                    #[name(ai_model_row)]
                    adw::EntryRow {
                        set_title: "Model",
                        set_text: &model.values.ai_model,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_changed[sender] => move |row| {
                            sender.input(SettingsMsg::AiModel(row.text().to_string()));
                        },
                    },

                    #[name(ai_base_url_row)]
                    adw::EntryRow {
                        set_title: "Base URL",
                        set_text: &model.values.ai_base_url,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_changed[sender] => move |row| {
                            sender.input(SettingsMsg::AiBaseUrl(row.text().to_string()));
                        },
                    },

                    #[name(ai_api_key_row)]
                    adw::PasswordEntryRow {
                        set_title: "API Key — enter a new value and press Apply",
                        set_show_apply_button: true,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_apply[sender] => move |row| {
                            sender.input(SettingsMsg::AiApiKeyStore(row.text().to_string()));
                        },
                    },

                    #[name(ai_max_tokens_row)]
                    adw::SpinRow::new(
                        Some(&gtk::Adjustment::new(
                            model.values.ai_max_tokens, 64.0, 32_768.0, 64.0, 512.0, 0.0
                        )),
                        64.0,
                        0,
                    ) {
                        set_title: "Maximum Response Tokens",
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_value_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiMaxTokens(row.value()));
                        },
                    },

                    #[name(agent_max_turns_row)]
                    adw::SpinRow::new(
                        Some(&gtk::Adjustment::new(
                            model.values.agent_max_turns, 1.0, 100.0, 1.0, 5.0, 0.0
                        )),
                        1.0,
                        0,
                    ) {
                        set_title: "Agent Turn Limit",
                        set_sensitive: !model.values.safe_mode
                            && model.values.ai_enabled
                            && model.values.agent_enabled,
                        connect_value_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AgentMaxTurns(row.value()));
                        },
                    },

                    #[name(ai_stream_row)]
                    adw::SwitchRow {
                        set_title: "Stream Chat Responses",
                        set_subtitle: "Show AI chat replies incrementally while they are generated",
                        set_active: model.values.ai_stream,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiStream(row.is_active()));
                        },
                    },

                    #[name(ai_redact_secrets_row)]
                    adw::SwitchRow {
                        set_title: "Redact Common Secrets",
                        set_subtitle: "Apply before terminal context is sent to a provider",
                        set_active: model.values.ai_redact_secrets,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiRedactSecrets(row.is_active()));
                        },
                    },

                },

                // Host rows are managed imperatively (`rebuild_remote_rows`):
                // the view! macro cannot express a list that grows and shrinks.
                #[name(remote_hosts_group)]
                adw::PreferencesGroup {
                    set_title: "Remote Hosts",
                    set_description: Some(
                        "Targets for the Ctrl+Shift+S picker. Advanced fields (ssh_args, session, deploy_artifact) are edited in config.toml"
                    ),
                    set_sensitive: !model.values.safe_mode,

                    #[wrap(Some)]
                    set_header_suffix = &gtk::Button {
                        set_icon_name: "list-add-symbolic",
                        add_css_class: "flat",
                        set_valign: gtk::Align::Center,
                        set_tooltip_text: Some("Add Remote Host"),
                        update_property: &[gtk::accessible::Property::Label("Add Remote Host")],
                        connect_clicked => SettingsMsg::RemoteHostOpenAdd,
                    },
                },
            },
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let mut model = Self {
            theme_names: init.theme_names,
            font_names: init.font_names,
            values: init.values,
            organism_syncing: Rc::new(Cell::new(false)),
            remote_draft: RemoteDraft::default(),
            remote_editing: None,
            remote_dialog: None,
            remote_rows: Vec::new(),
        };
        let preview_dialog = root.clone();
        let widgets = view_output!();
        organism_preview::install(
            &widgets.organism_preview_group,
            &widgets.ascii_organism_motion_row,
            &preview_dialog,
        );
        model.rebuild_remote_rows(&widgets.remote_hosts_group, &sender);
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        // Reject stale editor events before touching the currently open UI or
        // its draft. In particular, closing a replaced dialog queues Cancel.
        let editor_epoch = match &msg {
            SettingsMsg::RemoteHostName(epoch, _)
            | SettingsMsg::RemoteHostHost(epoch, _)
            | SettingsMsg::RemoteHostUser(epoch, _)
            | SettingsMsg::RemoteHostDocker(epoch, _)
            | SettingsMsg::RemoteHostDeploy(epoch, _)
            | SettingsMsg::RemoteHostSave(epoch)
            | SettingsMsg::RemoteHostCancel(epoch) => Some(epoch),
            _ => None,
        };
        if editor_epoch.is_some_and(|epoch| {
            !remote_dialog_event_is_current(self.remote_dialog.as_ref().map(|ui| &ui.epoch), epoch)
        }) {
            return;
        }
        match msg {
            SettingsMsg::Toggle(values, font_names, parent) => {
                if root.parent().is_some() {
                    root.force_close();
                    return;
                }
                self.values = values;
                self.font_names = font_names;
                let font_notify_guard = widgets.font_row.freeze_notify();
                widgets.font_row.set_model(Some(&gtk::StringList::new(
                    &self
                        .font_names
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                )));
                widgets.theme_row.set_selected(self.values.theme);
                widgets.font_row.set_selected(self.values.font);
                drop(font_notify_guard);
                widgets.font_size_row.set_value(self.values.font_size);
                widgets.font_scale_row.set_value(self.values.font_scale);
                widgets.opacity_scale.set_value(self.values.opacity);
                widgets.scrollback_row.set_value(self.values.scrollback);
                widgets
                    .terminal_mode_row
                    .set_selected(self.values.terminal_mode);
                widgets
                    .terminal_mode_row
                    .set_sensitive(!self.values.safe_mode);
                widgets
                    .block_compact_row
                    .set_active(self.values.block_compact);
                widgets
                    .block_compact_row
                    .set_sensitive(!self.values.safe_mode);
                widgets
                    .command_history_row
                    .set_active(self.values.command_history);
                widgets
                    .command_history_row
                    .set_sensitive(!self.values.safe_mode);
                reflect_ascii_organism_controls(
                    &self.organism_syncing,
                    &widgets.ascii_organism_row,
                    &widgets.ascii_organism_motion_row,
                    self.values.ascii_organism_enabled,
                    self.values.ascii_organism_motion,
                    self.values.safe_mode,
                );
                widgets.ai_enabled_row.set_active(self.values.ai_enabled);
                widgets
                    .ai_panel_visible_row
                    .set_active(self.values.ai_panel_visible);
                widgets
                    .ai_panel_width_row
                    .set_value(self.values.ai_panel_width);
                widgets
                    .agent_enabled_row
                    .set_active(self.values.agent_enabled);
                widgets
                    .command_correction_row
                    .set_active(self.values.command_correction_enabled);
                widgets
                    .ai_provider_row
                    .set_selected(self.values.ai_provider);
                widgets.ai_model_row.set_text(&self.values.ai_model);
                widgets.ai_base_url_row.set_text(&self.values.ai_base_url);
                widgets.ai_api_key_row.set_text("");
                widgets
                    .ai_api_key_row
                    .set_title("API Key — enter a new value and press Apply");
                widgets
                    .ai_max_tokens_row
                    .set_value(self.values.ai_max_tokens);
                widgets
                    .ai_redact_secrets_row
                    .set_active(self.values.ai_redact_secrets);
                widgets.ai_stream_row.set_active(self.values.ai_stream);
                widgets
                    .agent_max_turns_row
                    .set_value(self.values.agent_max_turns);
                let ai_sensitive = !self.values.safe_mode && self.values.ai_enabled;
                widgets.ai_enabled_row.set_sensitive(!self.values.safe_mode);
                widgets.ai_panel_visible_row.set_sensitive(ai_sensitive);
                widgets.ai_panel_width_row.set_sensitive(ai_sensitive);
                widgets.agent_enabled_row.set_sensitive(ai_sensitive);
                widgets.command_correction_row.set_sensitive(ai_sensitive);
                widgets.ai_provider_row.set_sensitive(ai_sensitive);
                widgets.ai_model_row.set_sensitive(ai_sensitive);
                widgets.ai_base_url_row.set_sensitive(ai_sensitive);
                widgets.ai_api_key_row.set_sensitive(ai_sensitive);
                widgets.ai_max_tokens_row.set_sensitive(ai_sensitive);
                widgets.ai_redact_secrets_row.set_sensitive(ai_sensitive);
                widgets.ai_stream_row.set_sensitive(ai_sensitive);
                widgets
                    .agent_max_turns_row
                    .set_sensitive(ai_sensitive && self.values.agent_enabled);
                widgets
                    .notifications_row
                    .set_active(self.values.notifications);
                widgets
                    .notifications_row
                    .set_sensitive(!self.values.safe_mode);
                widgets
                    .remote_clipboard_row
                    .set_active(self.values.remote_clipboard);
                widgets
                    .remote_clipboard_row
                    .set_sensitive(!self.values.safe_mode);
                // A reopened dialog starts on a fresh host: the index an edit
                // was holding may not survive whatever changed the list while
                // the dialog was closed.
                if let Some(ui) = self.remote_dialog.take() {
                    ui.dialog.close();
                }
                self.remote_editing = None;
                self.remote_draft = RemoteDraft::default();
                self.rebuild_remote_rows(&widgets.remote_hosts_group, &sender);
                root.present(Some(&parent));
            }
            SettingsMsg::Theme(index) => {
                self.values.theme = index;
                let _ = sender.output(SettingsOutput::Theme(index as usize));
            }
            SettingsMsg::Font(index) => {
                self.values.font = index;
                self.output_font(&sender);
            }
            SettingsMsg::FontSize(size) => {
                self.values.font_size = size;
                self.output_font(&sender);
            }
            SettingsMsg::FontScale(scale) => {
                self.values.font_scale = scale;
                let _ = sender.output(SettingsOutput::FontScale(scale));
            }
            SettingsMsg::Opacity(opacity) => {
                self.values.opacity = opacity;
                let _ = sender.output(SettingsOutput::Opacity(opacity));
            }
            SettingsMsg::Scrollback(lines) => {
                self.values.scrollback = lines;
                let _ = sender.output(SettingsOutput::Scrollback(lines as u32));
            }
            SettingsMsg::TerminalMode(mode) => {
                self.values.terminal_mode = mode;
                let _ = sender.output(SettingsOutput::TerminalMode(mode as usize));
            }
            SettingsMsg::BlockCompact(enabled) => {
                self.values.block_compact = enabled;
                let _ = sender.output(SettingsOutput::BlockCompact(enabled));
            }
            SettingsMsg::CommandHistory(enabled) => {
                self.values.command_history = enabled;
                let _ = sender.output(SettingsOutput::CommandHistory(enabled));
            }
            SettingsMsg::SyncAsciiOrganism { enabled, motion } => {
                self.values.ascii_organism_enabled = enabled;
                self.values.ascii_organism_motion = motion;
                reflect_ascii_organism_controls(
                    &self.organism_syncing,
                    &widgets.ascii_organism_row,
                    &widgets.ascii_organism_motion_row,
                    enabled,
                    motion,
                    self.values.safe_mode,
                );
            }
            SettingsMsg::AsciiOrganism(enabled) => {
                self.values.ascii_organism_enabled = enabled;
                widgets
                    .ascii_organism_motion_row
                    .set_sensitive(!self.values.safe_mode && enabled);
                let _ = sender.output(SettingsOutput::AsciiOrganism(enabled));
            }
            SettingsMsg::AsciiOrganismMotion(motion) => {
                self.values.ascii_organism_motion = motion;
                let _ = sender.output(SettingsOutput::AsciiOrganismMotion(motion));
            }
            SettingsMsg::AiEnabled(enabled) => {
                self.values.ai_enabled = enabled;
                let sensitive = !self.values.safe_mode && enabled;
                widgets.agent_enabled_row.set_sensitive(sensitive);
                widgets.ai_panel_visible_row.set_sensitive(sensitive);
                widgets.ai_panel_width_row.set_sensitive(sensitive);
                widgets.command_correction_row.set_sensitive(sensitive);
                widgets.ai_provider_row.set_sensitive(sensitive);
                widgets.ai_model_row.set_sensitive(sensitive);
                widgets.ai_base_url_row.set_sensitive(sensitive);
                widgets.ai_api_key_row.set_sensitive(sensitive);
                widgets.ai_max_tokens_row.set_sensitive(sensitive);
                widgets.ai_redact_secrets_row.set_sensitive(sensitive);
                widgets.ai_stream_row.set_sensitive(sensitive);
                widgets
                    .agent_max_turns_row
                    .set_sensitive(sensitive && self.values.agent_enabled);
                let _ = sender.output(SettingsOutput::AiEnabled(enabled));
            }
            SettingsMsg::AiPanelVisible(visible) => {
                self.values.ai_panel_visible = visible;
                let _ = sender.output(SettingsOutput::AiPanelVisible(visible));
            }
            SettingsMsg::AiPanelWidth(width) => {
                self.values.ai_panel_width = width;
                let _ = sender.output(SettingsOutput::AiPanelWidth(width as u32));
            }
            SettingsMsg::AgentEnabled(enabled) => {
                self.values.agent_enabled = enabled;
                widgets
                    .agent_max_turns_row
                    .set_sensitive(!self.values.safe_mode && self.values.ai_enabled && enabled);
                let _ = sender.output(SettingsOutput::AgentEnabled(enabled));
            }
            SettingsMsg::CommandCorrection(enabled) => {
                self.values.command_correction_enabled = enabled;
                let _ = sender.output(SettingsOutput::CommandCorrection(enabled));
            }
            SettingsMsg::AiProvider(provider) => {
                self.values.ai_provider = provider;
                let _ = sender.output(SettingsOutput::AiProvider(provider as usize));
            }
            SettingsMsg::AiModel(model) => {
                self.values.ai_model = model.clone();
                let _ = sender.output(SettingsOutput::AiModel(model));
            }
            SettingsMsg::AiBaseUrl(base_url) => {
                self.values.ai_base_url = base_url.clone();
                let _ = sender.output(SettingsOutput::AiBaseUrl(base_url));
            }
            SettingsMsg::AiApiKeyStore(key) => {
                // Same write-target rule as the rest of the family: the
                // configured path, else the per-app default. The environment
                // override stays read-only and is never written to.
                let path = self
                    .values
                    .ai_api_key_file
                    .clone()
                    .unwrap_or_else(jterm_core::ai::default_api_key_path);
                match jterm_core::ai::write_api_key_file(&path, &key) {
                    Ok(()) => {
                        widgets.ai_api_key_row.set_text("");
                        widgets
                            .ai_api_key_row
                            .set_title("API Key stored — enter a new value to replace it");
                        self.values.ai_api_key_file = Some(path.clone());
                        let _ = sender.output(SettingsOutput::AiApiKeyFile(path));
                    }
                    Err(error) => {
                        widgets
                            .ai_api_key_row
                            .set_title(&format!("API Key not saved: {error}"));
                    }
                }
            }
            SettingsMsg::AiMaxTokens(max_tokens) => {
                self.values.ai_max_tokens = max_tokens;
                let _ = sender.output(SettingsOutput::AiMaxTokens(max_tokens as u32));
            }
            SettingsMsg::AiRedactSecrets(enabled) => {
                self.values.ai_redact_secrets = enabled;
                let _ = sender.output(SettingsOutput::AiRedactSecrets(enabled));
            }
            SettingsMsg::AiStream(enabled) => {
                self.values.ai_stream = enabled;
                let _ = sender.output(SettingsOutput::AiStream(enabled));
            }
            SettingsMsg::AgentMaxTurns(turns) => {
                self.values.agent_max_turns = turns;
                let _ = sender.output(SettingsOutput::AgentMaxTurns(turns as u32));
            }
            SettingsMsg::Notifications(enabled) => {
                self.values.notifications = enabled;
                let _ = sender.output(SettingsOutput::Notifications(enabled));
            }
            SettingsMsg::RemoteClipboard(enabled) => {
                self.values.remote_clipboard = enabled;
                let _ = sender.output(SettingsOutput::RemoteClipboard(enabled));
            }
            SettingsMsg::RemoteHostName(_, name) => self.remote_draft.name = name,
            SettingsMsg::RemoteHostHost(_, host) => self.remote_draft.host = host,
            SettingsMsg::RemoteHostUser(_, user) => self.remote_draft.user = user,
            SettingsMsg::RemoteHostDocker(_, docker) => self.remote_draft.docker = docker,
            SettingsMsg::RemoteHostDeploy(_, mode) => self.remote_draft.deploy = mode,
            SettingsMsg::RemoteHostOpenAdd => {
                self.present_remote_host_dialog(None, root, &sender);
            }
            SettingsMsg::RemoteHostSave(_) => {
                self.clear_remote_errors();
                let expected = self.values.remote_hosts.clone();
                match self.commit_remote_draft() {
                    Ok(()) => {
                        if let Some(ui) = self.remote_dialog.take() {
                            ui.dialog.close();
                        }
                        self.remote_editing = None;
                        self.remote_draft = RemoteDraft::default();
                        self.rebuild_remote_rows(&widgets.remote_hosts_group, &sender);
                        let _ = sender.output(SettingsOutput::RemoteHosts {
                            expected,
                            hosts: self.values.remote_hosts.clone(),
                        });
                    }
                    Err((field, message)) => {
                        self.show_remote_error(field, message);
                    }
                }
            }
            SettingsMsg::RemoteHostEdit(expected) => {
                self.present_remote_host_dialog(Some(expected), root, &sender);
            }
            SettingsMsg::RemoteHostCancel(_) => {
                if let Some(ui) = self.remote_dialog.take() {
                    ui.dialog.close();
                }
                self.remote_editing = None;
                self.remote_draft = RemoteDraft::default();
            }
            SettingsMsg::RemoteHostRemove(expected) => {
                if let Some(index) = expected.exact_index_in(&self.values.remote_hosts) {
                    let name = self.values.remote_hosts[index].name.clone();
                    let display = crate::review_input::safe_inline_display(&name, 1_024);
                    let dialog = adw::AlertDialog::new(
                        Some("Remove this host?"),
                        Some(&format!(
                            "“{display}” will be removed from config.toml. Nothing on the destination is touched."
                        )),
                    );
                    dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
                    dialog.set_default_response(Some("cancel"));
                    dialog.set_close_response("cancel");
                    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                    let sender = sender.clone();
                    dialog.connect_response(None, move |_, response| {
                        if response == "remove" {
                            sender.input(SettingsMsg::RemoteHostRemoveConfirmed(expected.clone()));
                        }
                    });
                    dialog.present(Some(root));
                }
            }
            SettingsMsg::RemoteHostRemoveConfirmed(target) => {
                let expected = self.values.remote_hosts.clone();
                let removed = remove_remote_host(&mut self.values.remote_hosts, &target);
                if removed {
                    self.rebuild_remote_rows(&widgets.remote_hosts_group, &sender);
                    let _ = sender.output(SettingsOutput::RemoteHosts {
                        expected,
                        hosts: self.values.remote_hosts.clone(),
                    });
                }
            }
        }
    }
}

impl SettingsModel {
    /// Drop the red outline left by the previous failed submit, so an error is
    /// only ever pointing at the field the user is being told about now.
    fn clear_remote_errors(&self) {
        let Some(ui) = self.remote_dialog.as_ref() else {
            return;
        };
        for row in [&ui.name, &ui.host, &ui.user] {
            row.remove_css_class("error");
        }
        ui.error.set_visible(false);
    }

    fn show_remote_error(&self, field: RemoteField, message: &str) {
        let Some(ui) = self.remote_dialog.as_ref() else {
            return;
        };
        let row = match field {
            RemoteField::Form => None,
            RemoteField::Name => Some(&ui.name),
            RemoteField::Host => Some(&ui.host),
            RemoteField::User => Some(&ui.user),
        };
        if let Some(row) = row {
            row.add_css_class("error");
            row.grab_focus();
        }
        ui.error.set_label(message);
        ui.error.set_visible(true);
    }

    fn present_remote_host_dialog(
        &mut self,
        editing: Option<CapturedRemoteProfile>,
        parent: &adw::PreferencesDialog,
        sender: &ComponentSender<Self>,
    ) {
        let existing = editing.as_ref().and_then(|expected| {
            expected
                .exact_index_in(&self.values.remote_hosts)
                .and_then(|index| self.values.remote_hosts.get(index))
                .cloned()
        });
        if editing.is_some() && existing.is_none() {
            return;
        }
        if let Some(ui) = self.remote_dialog.take() {
            ui.dialog.close();
        }
        self.remote_editing = editing;
        self.remote_draft = existing
            .as_ref()
            .map(RemoteDraft::from_host)
            .unwrap_or_default();

        let epoch = Rc::new(());
        let dialog = adw::Dialog::builder()
            .title(if existing.is_some() {
                "Edit Remote Host"
            } else {
                "Add Remote Host"
            })
            .content_width(420)
            .build();
        let name = adw::EntryRow::new();
        name.set_title("Name (optional)");
        name.set_text(&self.remote_draft.name);
        let host = adw::EntryRow::new();
        host.set_title("Host / container");
        host.set_text(&self.remote_draft.host);
        let user = adw::EntryRow::new();
        user.set_title("User (optional)");
        user.set_text(&self.remote_draft.user);
        let docker = adw::SwitchRow::builder()
            .title("Docker Container")
            .subtitle("Attach to a running container with docker exec instead of ssh")
            .active(self.remote_draft.docker)
            .build();
        let deploy_model = gtk::StringList::new(&["Off", "Persist", "Incognito"]);
        let deploy = adw::ComboRow::builder()
            .title("Deploy jsh")
            .subtitle("Put a jsh on the destination for the life of the session")
            .model(&deploy_model)
            .selected(self.remote_draft.deploy)
            .build();

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("boxed-list");
        list.append(&name);
        list.append(&host);
        list.append(&user);
        list.append(&docker);
        list.append(&deploy);

        let error = gtk::Label::new(None);
        error.add_css_class("error");
        error.set_wrap(true);
        error.set_xalign(0.0);
        error.set_visible(false);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_all(12);
        content.append(&list);
        if let Some(existing) = existing.as_ref() {
            if let Some(note) = advanced_fields_note(existing) {
                let note = crate::review_input::safe_inline_display(&note, 4 * 1024);
                let label = gtk::Label::new(Some(&note));
                label.add_css_class("dim-label");
                label.set_wrap(true);
                label.set_xalign(0.0);
                content.append(&label);
            }
        }
        content.append(&error);

        let header = adw::HeaderBar::new();
        header.set_show_start_title_buttons(false);
        header.set_show_end_title_buttons(false);
        let cancel = gtk::Button::with_label("Cancel");
        let save = gtk::Button::with_label(if existing.is_some() { "Save" } else { "Add" });
        save.add_css_class("suggested-action");
        header.pack_start(&cancel);
        header.pack_end(&save);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));
        dialog.set_child(Some(&toolbar));

        {
            let sender = sender.clone();
            let epoch = epoch.clone();
            name.connect_changed(move |row| {
                sender.input(SettingsMsg::RemoteHostName(
                    epoch.clone(),
                    row.text().to_string(),
                ));
            });
        }
        {
            let sender = sender.clone();
            let epoch = epoch.clone();
            host.connect_changed(move |row| {
                sender.input(SettingsMsg::RemoteHostHost(
                    epoch.clone(),
                    row.text().to_string(),
                ));
            });
        }
        {
            let sender = sender.clone();
            let epoch = epoch.clone();
            user.connect_changed(move |row| {
                sender.input(SettingsMsg::RemoteHostUser(
                    epoch.clone(),
                    row.text().to_string(),
                ));
            });
        }
        {
            let sender = sender.clone();
            let epoch = epoch.clone();
            docker.connect_active_notify(move |row| {
                sender.input(SettingsMsg::RemoteHostDocker(
                    epoch.clone(),
                    row.is_active(),
                ));
            });
        }
        {
            let sender = sender.clone();
            let epoch = epoch.clone();
            deploy.connect_selected_notify(move |row| {
                sender.input(SettingsMsg::RemoteHostDeploy(epoch.clone(), row.selected()));
            });
        }
        {
            let sender = sender.clone();
            let epoch = epoch.clone();
            cancel.connect_clicked(move |_| {
                sender.input(SettingsMsg::RemoteHostCancel(epoch.clone()))
            });
        }
        {
            let sender = sender.clone();
            let epoch = epoch.clone();
            save.connect_clicked(move |_| sender.input(SettingsMsg::RemoteHostSave(epoch.clone())));
        }
        {
            let sender = sender.clone();
            let epoch = epoch.clone();
            dialog.connect_closed(move |_| {
                sender.input(SettingsMsg::RemoteHostCancel(epoch.clone()))
            });
        }

        self.remote_dialog = Some(RemoteDialogUi {
            epoch,
            dialog: dialog.clone(),
            name,
            host: host.clone(),
            user,
            error,
        });
        dialog.present(Some(parent));
        host.grab_focus();
    }

    fn rebuild_remote_rows(
        &mut self,
        group: &adw::PreferencesGroup,
        sender: &ComponentSender<Self>,
    ) {
        for row in self.remote_rows.drain(..) {
            group.remove(&row);
        }
        if self.values.remote_hosts.is_empty() {
            let row = adw::ActionRow::new();
            row.set_title("No remote hosts configured");
            row.set_subtitle("Add an ssh destination or a running container");
            row.set_sensitive(false);
            group.add(&row);
            self.remote_rows.push(row);
            return;
        }
        for host in &self.values.remote_hosts {
            let expected = CapturedRemoteProfile::new(host.clone());
            let row = adw::ActionRow::new();
            row.set_use_markup(false);
            let title = if host.name.is_empty() {
                &host.host
            } else {
                &host.name
            };
            row.set_title(&crate::review_input::safe_inline_display(title, 1_024));
            let target = match &host.user {
                Some(user) => format!("{user}@{}", host.host),
                None => host.host.clone(),
            };
            let transport = if host.docker { "docker" } else { "ssh" };
            let mut subtitle = format!("{transport} · {target} · deploy {}", host.deploy.as_str());
            // The form has no widget for these, so say they are there rather
            // than let an edit look like it silently dropped them.
            if !host.ssh_args.is_empty() {
                subtitle.push_str(&format!(" · ssh_args {}", host.ssh_args.join(" ")));
            }
            row.set_subtitle(&crate::review_input::safe_inline_display(
                &subtitle,
                4 * 1024,
            ));
            let edit = gtk::Button::from_icon_name("document-edit-symbolic");
            edit.set_valign(gtk::Align::Center);
            edit.add_css_class("flat");
            edit.set_tooltip_text(Some("Edit Host"));
            edit.update_property(&[gtk::accessible::Property::Label(EDIT_REMOTE_HOST_LABEL)]);
            edit.connect_clicked({
                let sender = sender.clone();
                let expected = expected.clone();
                move |_| sender.input(SettingsMsg::RemoteHostEdit(expected.clone()))
            });
            row.add_suffix(&edit);
            let remove = gtk::Button::from_icon_name("user-trash-symbolic");
            remove.set_valign(gtk::Align::Center);
            remove.add_css_class("flat");
            remove.add_css_class("destructive-action");
            remove.set_tooltip_text(Some("Remove Host"));
            remove.update_property(&[gtk::accessible::Property::Label(REMOVE_REMOTE_HOST_LABEL)]);
            remove.connect_clicked({
                let sender = sender.clone();
                let expected = expected.clone();
                move |_| sender.input(SettingsMsg::RemoteHostRemove(expected.clone()))
            });
            row.add_suffix(&remove);
            group.add(&row);
            self.remote_rows.push(row);
        }
    }

    fn remote_editing_index(&self) -> Result<Option<usize>, (RemoteField, &'static str)> {
        self.remote_editing
            .as_ref()
            .map(|expected| {
                expected.exact_index_in(&self.values.remote_hosts).ok_or((
                RemoteField::Form,
                "The edited host changed or is no longer uniquely configured; reopen the editor.",
            ))
            })
            .transpose()
    }

    fn commit_remote_draft(&mut self) -> Result<(), (RemoteField, &'static str)> {
        let host = self.validate_remote_draft()?;
        match self.remote_editing_index()? {
            Some(index) => self.values.remote_hosts[index] = host,
            None => self.values.remote_hosts.push(host),
        }
        Ok(())
    }

    /// Mirror `parse_remote_hosts`' acceptance rules so a host added here
    /// always survives the next config load.
    fn validate_remote_draft(&self) -> Result<RemoteHost, (RemoteField, &'static str)> {
        let editing_index = self.remote_editing_index()?;
        if editing_index.is_none()
            && self.values.remote_hosts.len() >= crate::config::MAX_REMOTE_HOSTS
        {
            return Err((RemoteField::Form, "The remote host limit is reached."));
        }
        let host = self.remote_draft.host.trim().to_string();
        if host.is_empty() {
            return Err((RemoteField::Host, "Host is required."));
        }
        // ssh and docker would both read a leading dash as an option.
        if host.starts_with('-') {
            return Err((RemoteField::Host, "Host must not start with \"-\"."));
        }
        if !remote_text_is_safe(&host, false, 1_024) {
            return Err((
                RemoteField::Host,
                "Host must not contain whitespace or control characters.",
            ));
        }
        let name = match self.remote_draft.name.trim() {
            "" => host.clone(),
            value => value.to_string(),
        };
        if !remote_text_is_safe(&name, true, 256) {
            return Err((
                RemoteField::Name,
                "Name must be at most 256 characters without control characters.",
            ));
        }
        // Session restore uses the display name as the stable profile
        // identifier; the parser rejects duplicates for the same reason. The
        // host being edited is not its own duplicate — otherwise no edit that
        // keeps the name could ever be saved.
        if self
            .values
            .remote_hosts
            .iter()
            .enumerate()
            .any(|(index, existing)| existing.name == name && Some(index) != editing_index)
        {
            return Err((RemoteField::Name, "Another host already uses this name."));
        }
        let user = match self.remote_draft.user.trim() {
            "" => None,
            value => {
                if value.contains('@') || !remote_text_is_safe(value, false, 256) {
                    return Err((
                        RemoteField::User,
                        "User must not contain \"@\", whitespace, or control characters.",
                    ));
                }
                Some(value.to_string())
            }
        };
        let deploy = match self.remote_draft.deploy {
            1 => jterm_core::jsh_remote::Deploy::Persist,
            2 => jterm_core::jsh_remote::Deploy::Incognito,
            _ => jterm_core::jsh_remote::Deploy::Off,
        };
        // An edit keeps everything the form cannot show. Rebuilding the entry
        // from the visible rows alone would quietly delete a `-p 2222`, a
        // pinned session id or a deploy_artifact the moment someone fixed a
        // typo in the name — the config.toml-only fields are exactly the ones
        // nobody would think to check afterwards.
        let existing = editing_index.and_then(|index| self.values.remote_hosts.get(index));
        Ok(RemoteHost {
            name,
            host,
            user,
            docker: self.remote_draft.docker,
            deploy_artifact: existing.and_then(|h| h.deploy_artifact.clone()),
            remote_shell: existing
                .map(|h| h.remote_shell.clone())
                .unwrap_or_else(|| "jsh".to_string()),
            session: existing.and_then(|h| h.session.clone()),
            ssh_args: existing.map(|h| h.ssh_args.clone()).unwrap_or_default(),
            login_shell: existing.is_none_or(|h| h.login_shell),
            multiplex: existing.is_none_or(|h| h.multiplex),
            deploy,
        })
    }

    fn output_font(&self, sender: &ComponentSender<Self>) {
        let _ = sender.output(SettingsOutput::FontDesc(font_desc_for_choice(
            &self.font_names,
            self.values.font,
            self.values.font_size,
        )));
    }
}

fn advanced_fields_note(host: &RemoteHost) -> Option<String> {
    let mut kept = Vec::new();
    if !host.ssh_args.is_empty() {
        kept.push(format!("ssh_args = {:?}", host.ssh_args));
    }
    if let Some(session) = &host.session {
        kept.push(format!("session = {session:?}"));
    }
    if host.remote_shell != "jsh" {
        kept.push(format!("remote_shell = {:?}", host.remote_shell));
    }
    if !host.login_shell {
        kept.push("login_shell = false".into());
    }
    if !host.multiplex {
        kept.push("multiplex = false".into());
    }
    if let Some(artifact) = &host.deploy_artifact {
        kept.push(format!("deploy_artifact = {artifact:?}"));
    }
    (!kept.is_empty()).then(|| format!("Kept as configured: {}", kept.join(", ")))
}

fn remove_remote_host(hosts: &mut Vec<RemoteHost>, expected: &CapturedRemoteProfile) -> bool {
    let Some(index) = expected.exact_index_in(hosts) else {
        return false;
    };
    hosts.remove(index);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_host_icon_buttons_have_distinct_accessible_labels() {
        assert!(!EDIT_REMOTE_HOST_LABEL.is_empty());
        assert!(!REMOVE_REMOTE_HOST_LABEL.is_empty());
        assert_ne!(EDIT_REMOTE_HOST_LABEL, REMOVE_REMOTE_HOST_LABEL);
    }

    #[test]
    fn missing_generic_family_stays_selected_for_size_changes() {
        let (font_names, selected) = font_choices(vec!["DejaVu Sans Mono".into()], "Monospace");

        assert_eq!(font_names[selected as usize], "Monospace");
        assert_eq!(
            font_desc_for_choice(&font_names, selected, 18.0),
            "Monospace 18"
        );
    }

    #[test]
    fn configured_nerd_font_is_kept_when_pango_does_not_list_it() {
        let configured = "SauceCodePro Nerd Font Mono";
        let (font_names, selected) = font_choices(vec!["DejaVu Sans Mono".into()], configured);

        assert_eq!(font_names[selected as usize], configured);
        assert_eq!(
            font_desc_for_choice(&font_names, selected, 16.0),
            "SauceCodePro Nerd Font Mono 16"
        );
    }

    #[test]
    fn an_existing_current_family_is_not_duplicated() {
        let (font_names, selected) = font_choices(
            vec!["monospace".into(), "DejaVu Sans Mono".into()],
            "Monospace",
        );

        assert_eq!(font_names[selected as usize], "monospace");
        assert_eq!(
            font_names
                .iter()
                .filter(|family| family.eq_ignore_ascii_case("Monospace"))
                .count(),
            1
        );
    }

    fn host_with_hidden_fields() -> RemoteHost {
        RemoteHost {
            name: "dev-60".to_string(),
            host: "10.68.18.60".to_string(),
            user: Some("root".to_string()),
            docker: false,
            deploy_artifact: Some("/opt/jsh/jsh".to_string()),
            remote_shell: "jsh".to_string(),
            session: Some("dev-main".to_string()),
            ssh_args: vec!["-p".to_string(), "2222".to_string()],
            login_shell: false,
            multiplex: false,
            deploy: jterm_core::jsh_remote::Deploy::Persist,
        }
    }

    /// A model with no widgets: `validate_remote_draft` reads only model state,
    /// so the form logic is testable without a display.
    fn model(hosts: Vec<RemoteHost>, editing: Option<usize>) -> SettingsModel {
        let draft = editing
            .and_then(|index| hosts.get(index))
            .map(RemoteDraft::from_host)
            .unwrap_or_default();
        let editing_profile = editing
            .and_then(|index| hosts.get(index))
            .cloned()
            .map(CapturedRemoteProfile::new);
        SettingsModel {
            theme_names: Vec::new(),
            font_names: Vec::new(),
            values: SettingsValues {
                theme: 0,
                font: 0,
                font_size: 12.0,
                font_scale: 1.0,
                opacity: 1.0,
                scrollback: 5000.0,
                terminal_mode: 0,
                block_compact: false,
                command_history: true,
                ascii_organism_enabled: false,
                ascii_organism_motion: 0,
                ai_enabled: false,
                ai_panel_visible: false,
                ai_panel_width: 360.0,
                agent_enabled: false,
                command_correction_enabled: false,
                ai_provider: 0,
                ai_model: String::new(),
                ai_base_url: String::new(),
                ai_api_key_file: None,
                ai_max_tokens: 1024.0,
                ai_redact_secrets: true,
                ai_stream: true,
                agent_max_turns: 20.0,
                safe_mode: false,
                notifications: true,
                remote_clipboard: false,
                remote_hosts: hosts,
            },
            organism_syncing: Rc::new(Cell::new(false)),
            remote_draft: draft,
            remote_editing: editing_profile,
            remote_dialog: None,
            remote_rows: Vec::new(),
        }
    }

    /// The form shows five fields; the entry has ten. Renaming through the form
    /// must not be a way to lose the other five, because nothing in the dialog
    /// would show that it happened.
    #[test]
    fn editing_preserves_fields_the_form_cannot_show() {
        let mut model = model(vec![host_with_hidden_fields()], Some(0));
        model.remote_draft.name = "prod-60".to_string();

        let edited = model.validate_remote_draft().expect("valid draft");
        assert_eq!(edited.name, "prod-60");
        assert_eq!(edited.ssh_args, ["-p", "2222"]);
        assert_eq!(edited.session.as_deref(), Some("dev-main"));
        assert_eq!(edited.deploy_artifact.as_deref(), Some("/opt/jsh/jsh"));
        assert!(!edited.login_shell);
        assert!(!edited.multiplex);
    }

    /// A new host gets the plain defaults rather than anything left over from a
    /// previously edited entry.
    #[test]
    fn adding_a_host_starts_from_the_defaults() {
        let mut model = model(vec![host_with_hidden_fields()], None);
        model.remote_draft.name = "staging".to_string();
        model.remote_draft.host = "staging.example.com".to_string();

        let added = model.validate_remote_draft().expect("valid draft");
        assert!(added.ssh_args.is_empty());
        assert_eq!(added.session, None);
        assert_eq!(added.deploy_artifact, None);
        assert!(added.login_shell);
        assert!(added.multiplex);
    }

    #[test]
    fn adding_is_refused_at_the_shared_remote_host_limit() {
        let hosts = (0..crate::config::MAX_REMOTE_HOSTS)
            .map(|index| {
                let mut host = host_with_hidden_fields();
                host.name = format!("host-{index}");
                host.host = format!("host-{index}.example");
                host
            })
            .collect();
        let mut model = model(hosts, None);
        model.remote_draft.host = "one-too-many.example".to_string();

        let (field, message) = model
            .validate_remote_draft()
            .expect_err("host limit must be enforced");
        assert!(matches!(field, RemoteField::Form));
        assert_eq!(message, "The remote host limit is reached.");
    }

    #[test]
    fn an_edit_that_keeps_the_name_is_not_a_duplicate() {
        let mut model = model(vec![host_with_hidden_fields()], Some(0));
        model.remote_draft.host = "10.68.18.61".to_string();

        let edited = model.validate_remote_draft().expect("valid draft");
        assert_eq!(edited.name, "dev-60");
        assert_eq!(edited.host, "10.68.18.61");
    }

    #[test]
    fn an_edit_may_not_take_another_hosts_name() {
        let mut other = host_with_hidden_fields();
        other.name = "myubuntu".to_string();
        other.host = "myubuntu".to_string();
        let mut model = model(vec![host_with_hidden_fields(), other], Some(0));
        model.remote_draft.name = "myubuntu".to_string();

        let (field, _) = model.validate_remote_draft().expect_err("duplicate name");
        assert!(matches!(field, RemoteField::Name));
    }

    #[test]
    fn edit_dialog_draft_loads_every_visible_field() {
        let host = host_with_hidden_fields();
        let draft = RemoteDraft::from_host(&host);

        assert_eq!(draft.name, "dev-60");
        assert_eq!(draft.host, "10.68.18.60");
        assert_eq!(draft.user, "root");
        assert!(!draft.docker);
        assert_eq!(draft.deploy, 1);
    }

    #[test]
    fn advanced_note_discloses_every_preserved_field() {
        let mut host = host_with_hidden_fields();
        host.remote_shell = "/bin/bash".to_string();
        let note = advanced_fields_note(&host).expect("host has advanced fields");

        for field in [
            "ssh_args",
            "session",
            "remote_shell",
            "login_shell",
            "multiplex",
            "deploy_artifact",
        ] {
            assert!(note.contains(field), "missing {field} in {note:?}");
        }
    }

    #[test]
    fn default_host_has_no_advanced_note() {
        let mut host = host_with_hidden_fields();
        host.ssh_args.clear();
        host.session = None;
        host.remote_shell = "jsh".to_string();
        host.login_shell = true;
        host.multiplex = true;
        host.deploy_artifact = None;

        assert_eq!(advanced_fields_note(&host), None);
    }

    #[test]
    fn confirmed_delete_uses_the_exact_unique_snapshot() {
        let first = host_with_hidden_fields();
        let mut second = host_with_hidden_fields();
        second.name = "staging".to_string();
        let expected = CapturedRemoteProfile::new(second.clone());
        let mut hosts = vec![first, second];

        assert!(remove_remote_host(&mut hosts, &expected));
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "dev-60");
    }

    #[test]
    fn confirmed_delete_resolves_the_exact_snapshot_after_reordering() {
        let target = host_with_hidden_fields();
        let mut other = host_with_hidden_fields();
        other.name = "staging".to_string();
        let mut hosts = vec![other, target];

        // The captured profile moved from index zero. Exact unique identity,
        // rather than the old index or only its name, still identifies it.
        assert!(remove_remote_host(
            &mut hosts,
            &CapturedRemoteProfile::new(host_with_hidden_fields())
        ));
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "staging");
        assert!(!remove_remote_host(
            &mut hosts,
            &CapturedRemoteProfile::new(host_with_hidden_fields())
        ));
    }
    #[test]
    fn ascii_reflection_gate_suppresses_synchronous_callbacks_and_restores_state() {
        let gate = Cell::new(false);
        without_ascii_settings_output(&gate, || {
            assert!(gate.get());
            without_ascii_settings_output(&gate, || assert!(gate.get()));
            assert!(gate.get());
        });
        assert!(!gate.get());
    }

    #[test]
    fn ascii_reload_reflects_only_accepted_fields_without_outputs_or_reopening() {
        let source = include_str!("settings.rs");
        let sync = source
            .split("SettingsMsg::SyncAsciiOrganism { enabled, motion } => {")
            .nth(1)
            .unwrap()
            .split("SettingsMsg::AsciiOrganism(enabled)")
            .next()
            .unwrap();
        assert!(sync.contains("self.values.ascii_organism_enabled = enabled"));
        assert!(sync.contains("self.values.ascii_organism_motion = motion"));
        assert!(sync.contains("reflect_ascii_organism_controls("));
        for forbidden in [
            "sender.output",
            "root.present",
            "set_selected(0)",
            "remote_draft",
            "cancel_greeting",
        ] {
            assert!(!sync.contains(forbidden));
        }
        for callback in [
            "connect_active_notify[sender, syncing = model.organism_syncing.clone()]",
            "connect_selected_notify[sender, syncing = model.organism_syncing.clone()]",
        ] {
            let body = source
                .split(callback)
                .nth(1)
                .unwrap()
                .split("},")
                .next()
                .unwrap();
            assert!(body.contains("if !syncing.get()"));
        }
        let reload = include_str!("../config_ops.rs");
        assert!(
            reload
                .find("*self.config.borrow_mut() = new_config.clone()")
                .unwrap()
                < reload
                    .find("self.sync_ascii_organism_settings_dialog()")
                    .unwrap()
        );
    }
    #[test]
    fn ascii_confirmations_converge_in_actual_relm_queue_order_without_feedback() {
        let (inputs, settings_queue) = relm4::channel::<(bool, u32)>();
        let (outputs, app_queue) = relm4::channel::<(bool, u32)>();
        let gate = Cell::new(false);
        let writes = Cell::new(0);
        // A user output is pending while AppModel accepts an external reload.
        outputs.emit((true, 3));
        let mut accepted = (false, 1);
        inputs.emit(accepted);
        // AppModel subsequently accepts that user change and confirms it.
        accepted = app_queue.recv_sync().unwrap();
        writes.set(writes.get() + 1);
        inputs.emit(accepted);
        let mut displayed = (true, 0);
        for expected in [(false, 1), (true, 3)] {
            let reflected = settings_queue.recv_sync().unwrap();
            assert_eq!(reflected, expected);
            without_ascii_settings_output(&gate, || {
                displayed = reflected;
                if !gate.get() {
                    outputs.emit(reflected);
                    writes.set(writes.get() + 1);
                }
            });
        }
        assert_eq!(displayed, accepted);
        assert_eq!(writes.get(), 1, "reflection never persists a second change");
        // A safe-mode rejection confirms the unchanged accepted pair.
        outputs.emit((false, 0));
        let _rejected = app_queue.recv_sync().unwrap();
        inputs.emit(accepted);
        let reflected = settings_queue.recv_sync().unwrap();
        without_ascii_settings_output(&gate, || displayed = reflected);
        assert_eq!(displayed, accepted);
        assert_eq!(writes.get(), 1);
    }

    #[test]
    fn every_ascii_app_mutation_confirms_the_current_pair() {
        let source = include_str!("../settings_ops.rs");
        let helper = source
            .split("pub(crate) fn sync_ascii_organism_settings_dialog")
            .nth(1)
            .unwrap()
            .split("pub(crate) fn apply_settings_ascii_organism(")
            .next()
            .unwrap();
        assert!(helper.contains("self.config.borrow()"));
        assert!(helper.contains("config.ascii_organism_enabled"));
        assert!(helper.contains("config.ascii_organism_motion"));
        assert!(helper.contains("SettingsMsg::SyncAsciiOrganism { enabled, motion }"));
        let mutations = source
            .split("pub(crate) fn apply_settings_ascii_organism(")
            .nth(1)
            .unwrap()
            .split("pub(crate) fn apply_settings_ai_enabled")
            .next()
            .unwrap();
        assert_eq!(
            mutations
                .matches("self.sync_ascii_organism_settings_dialog()")
                .count(),
            4
        );
    }
    #[test]
    fn stale_editor_close_save_and_fields_cannot_touch_a_reopened_draft() {
        let old = Rc::new(());
        let current = Rc::new(());
        let mut draft = "current draft";
        let mut saves = 0;
        let mut closes = 0;
        for event in ["field", "save", "close"] {
            if remote_dialog_event_is_current(Some(&current), &old) {
                match event {
                    "field" => draft = "stale draft",
                    "save" => saves += 1,
                    _ => closes += 1,
                }
            }
        }
        assert_eq!(draft, "current draft");
        assert_eq!((saves, closes), (0, 0));
        if remote_dialog_event_is_current(Some(&current), &current) {
            draft = "current edit";
            saves += 1;
        }
        assert_eq!(draft, "current edit");
        assert_eq!(saves, 1);
    }

    #[test]
    fn taking_current_editor_retires_late_callbacks_without_token_reuse() {
        let event = Rc::new(());
        let mut current = Some(event.clone());
        assert!(remote_dialog_event_is_current(current.as_ref(), &event));
        let _retired = current.take();
        assert!(!remote_dialog_event_is_current(current.as_ref(), &event));
        current = Some(Rc::new(()));
        assert!(!remote_dialog_event_is_current(current.as_ref(), &event));
    }

    #[test]
    fn all_editor_callbacks_carry_the_opening_epoch_and_gate_precedes_mutation() {
        let source = include_str!("settings.rs");
        let update = source
            .split("fn update_with_view(")
            .nth(1)
            .unwrap()
            .split("impl SettingsModel {")
            .next()
            .unwrap();
        let gate = update.split("match msg {").next().unwrap();
        for variant in ["Name", "Host", "User", "Docker", "Deploy", "Save", "Cancel"] {
            assert!(gate.contains(&format!("SettingsMsg::RemoteHost{variant}(epoch")));
        }
        assert!(gate.contains("remote_dialog_event_is_current("));
        assert!(gate.contains("return;"));
        let open = source
            .split("fn present_remote_host_dialog(")
            .nth(1)
            .unwrap()
            .split("fn rebuild_remote_rows(")
            .next()
            .unwrap();
        assert!(
            open.find("self.remote_dialog.take()").unwrap()
                < open.find("ui.dialog.close()").unwrap()
        );
        assert!(open.contains("let epoch = Rc::new(())"));
        assert_eq!(open.matches("let epoch = epoch.clone()").count(), 8);
        assert!(open.contains("SettingsMsg::RemoteHostSave(epoch.clone())"));
        assert_eq!(
            open.matches("SettingsMsg::RemoteHostCancel(epoch.clone())")
                .count(),
            2
        );
    }
    #[test]
    fn editor_original_snapshot_survives_preceding_removal_without_retargeting() {
        let mut other = host_with_hidden_fields();
        other.name = "other".into();
        other.host = "other.example".into();
        let original = host_with_hidden_fields();
        let mut model = model(vec![other, original.clone()], Some(1));
        model.values.remote_hosts.remove(0);
        model.remote_draft.name = "renamed".into();
        model.commit_remote_draft().unwrap();
        assert_eq!(model.values.remote_hosts.len(), 1);
        assert_eq!(model.values.remote_hosts[0].name, "renamed");
        assert_eq!(model.values.remote_hosts[0].ssh_args, original.ssh_args);
        assert_eq!(model.values.remote_hosts[0].session, original.session);
    }

    #[test]
    fn stale_editor_target_is_not_replaced_or_recreated_as_an_add() {
        let original = host_with_hidden_fields();
        let mut replacement = original.clone();
        replacement.host = "replacement.example".into();
        for current in [
            vec![],
            vec![replacement],
            vec![original.clone(), original.clone()],
        ] {
            let mut model = model(vec![original.clone()], Some(0));
            model.values.remote_hosts = current.clone();
            model.remote_draft.name = "unsaved draft".into();
            assert!(model.commit_remote_draft().is_err());
            assert_eq!(model.values.remote_hosts, current);
            assert_eq!(model.remote_draft.name, "unsaved draft");
        }
    }

    #[test]
    fn removal_snapshot_rejects_same_name_edits_and_duplicate_profiles() {
        let original = host_with_hidden_fields();
        let expected = CapturedRemoteProfile::new(original.clone());
        let mut edited = original.clone();
        edited.host = "changed.example".into();
        for mut current in [
            vec![edited],
            vec![original.clone(), original.clone()],
            vec![],
        ] {
            let before = current.clone();
            assert!(!remove_remote_host(&mut current, &expected));
            assert_eq!(current, before);
        }
    }

    #[test]
    fn editor_and_remove_callbacks_never_fall_back_to_row_indices() {
        let source = include_str!("settings.rs");
        let rows = source
            .split("fn rebuild_remote_rows(")
            .nth(1)
            .unwrap()
            .split("fn remote_editing_index(")
            .next()
            .unwrap();
        assert!(rows.contains("SettingsMsg::RemoteHostEdit(expected.clone())"));
        assert!(rows.contains("SettingsMsg::RemoteHostRemove(expected.clone())"));
        assert!(!rows.contains("RemoteHostEdit(index)"));
        let response = source
            .split("SettingsMsg::RemoteHostRemove(expected) =>")
            .nth(1)
            .unwrap()
            .split("SettingsMsg::RemoteHostRemoveConfirmed(target)")
            .next()
            .unwrap();
        assert!(response.contains("SettingsMsg::RemoteHostRemoveConfirmed(expected.clone())"));
        let save = source
            .split("SettingsMsg::RemoteHostSave(_) =>")
            .nth(1)
            .unwrap()
            .split("SettingsMsg::RemoteHostEdit(expected)")
            .next()
            .unwrap();
        assert!(save.contains("self.commit_remote_draft()"));
        assert!(save.contains("SettingsOutput::RemoteHosts {"));
    }
    #[test]
    fn editor_can_repair_or_remove_a_uniquely_captured_unavailable_profile() {
        let mut unavailable = host_with_hidden_fields();
        unavailable.host = "-unavailable".into();
        let target = CapturedRemoteProfile::new(unavailable.clone());
        let mut model = model(vec![unavailable.clone()], Some(0));
        model.remote_draft.host = "repaired.example".into();
        model.commit_remote_draft().unwrap();
        assert_eq!(model.values.remote_hosts[0].host, "repaired.example");
        let mut hosts = vec![unavailable];
        assert!(remove_remote_host(&mut hosts, &target));
        assert!(hosts.is_empty());
    }

    #[test]
    fn exact_editor_identity_does_not_confuse_same_name_or_apply_connection_cap() {
        let original = host_with_hidden_fields();
        let expected = CapturedRemoteProfile::new(original.clone());
        let mut other = original.clone();
        other.host = "different.example".into();
        let mut hosts = vec![other.clone(); crate::config::MAX_REMOTE_HOSTS];
        hosts.push(original);
        assert_eq!(
            expected.exact_index_in(&hosts),
            Some(crate::config::MAX_REMOTE_HOSTS)
        );
        assert!(remove_remote_host(&mut hosts, &expected));
        assert_eq!(hosts, vec![other; crate::config::MAX_REMOTE_HOSTS]);
    }
}
