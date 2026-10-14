//! A deferred focus return belongs to the owner that requested it, not the
//! next owner chosen before GTK gets around to running the idle.
use gtk::prelude::*;
use gtk::{glib, Widget, Window};
use relm4::gtk;
use std::cell::Cell;
use std::rc::Rc;

pub(super) struct FocusRestore {
    target: glib::WeakRef<Widget>,
    window: glib::WeakRef<Window>,
    superseded: Rc<Cell<bool>>,
    observer: Option<glib::SignalHandlerId>,
    root_observer: Option<glib::SignalHandlerId>,
}

impl FocusRestore {
    /// Capture while the terminal still owns focus. Losing focus to `None`
    /// during a synchronous remap is permitted; a different owner is not.
    pub(super) fn live(target: &impl IsA<Widget>) -> Option<Self> {
        let target = target.as_ref();
        target
            .has_focus()
            .then(|| Self::capture(target, None))
            .flatten()
    }

    /// Arm before popup, not from `closed`: by then a click in another pane
    /// may already have superseded the menu. Return to the actual original
    /// owner, including history/find, rather than inventing live-terminal
    /// ownership just because the menu was opened over that pane.
    pub(super) fn popover(target: &impl IsA<Widget>, popover: &gtk::Popover) -> Option<Self> {
        let window = target.as_ref().root()?.downcast::<Window>().ok()?;
        let owner = gtk::prelude::GtkWindowExt::focus(&window)?;
        Self::capture(&owner, Some(popover.upcast_ref()))
    }

    fn capture(target: &Widget, transient: Option<&Widget>) -> Option<Self> {
        let window = target.root()?.downcast::<Window>().ok()?;
        if !target.is_mapped() || !window.is_mapped() {
            return None;
        }
        let transient = transient.map(|widget| widget.downgrade());
        let target_weak = target.downgrade();
        let superseded = Rc::new(Cell::new(false));
        let superseded_notify = superseded.clone();
        let observer = window.connect_focus_widget_notify(move |window| {
            let Some(focus) = gtk::prelude::GtkWindowExt::focus(window) else {
                return;
            };
            let target = target_weak.upgrade();
            let transient = transient.as_ref().and_then(glib::WeakRef::upgrade);
            if target.as_ref() != Some(&focus)
                && !transient
                    .as_ref()
                    .is_some_and(|owner| &focus == owner || focus.is_ancestor(owner))
            {
                superseded_notify.set(true);
            }
        });
        let original_window = window.downgrade();
        let superseded_root = superseded.clone();
        let root_observer = target.connect_root_notify(move |target| {
            if let Some(root) = target.root() {
                if original_window
                    .upgrade()
                    .as_ref()
                    .map(|window| window.upcast_ref())
                    != Some(&root)
                {
                    superseded_root.set(true);
                }
            }
        });
        Some(Self {
            target: target.downgrade(),
            window: window.downgrade(),
            superseded,
            observer: Some(observer),
            root_observer: Some(root_observer),
        })
    }

    pub(super) fn defer(self) {
        glib::idle_add_local_once(move || {
            let (Some(target), Some(window)) = (self.target.upgrade(), self.window.upgrade())
            else {
                return;
            };
            // A temporary remap may finish before this idle. A hidden, removed,
            // disposed or reparented-to-another-window destination never wins.
            if !self.superseded.get()
                && target.is_mapped()
                && window.is_mapped()
                && target.root().as_ref() == Some(window.upcast_ref())
            {
                target.grab_focus();
            }
        });
    }
}

impl Drop for FocusRestore {
    fn drop(&mut self) {
        if let (Some(target), Some(observer)) = (self.target.upgrade(), self.root_observer.take()) {
            target.disconnect(observer);
        }
        if let (Some(window), Some(observer)) = (self.window.upgrade(), self.observer.take()) {
            window.disconnect(observer);
        }
    }
}

pub(super) fn restore_live_focus_deferred(target: &impl IsA<Widget>) {
    if let Some(restore) = FocusRestore::live(target) {
        restore.defer();
    }
}

#[cfg(test)]
mod tests {
    use super::super::{BlockState, TermView};
    use super::*;
    use crate::config::TerminalMode;
    use std::time::{Duration, Instant};

    const PROMPT: &[u8] = b"\x1b]133;A\x07$ \x1b]133;B\x07";

    fn pump() {
        let end = Instant::now() + Duration::from_millis(100);
        while Instant::now() < end {
            for _ in 0..100 {
                if !glib::MainContext::default().iteration(false) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn wait_for(condition: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < end, "GTK condition did not settle");
            pump();
        }
    }

    fn owns_focus(widget: &impl IsA<Widget>) -> bool {
        let widget = widget.as_ref();
        widget
            .root()
            .and_then(|root| root.focus())
            .is_some_and(|focus| focus == *widget || focus.is_ancestor(widget))
    }

    fn fixture(mode: TerminalMode) -> (Rc<TermView>, Window, gtk::Box, Vec<Widget>) {
        static CHILD_ENV: std::sync::Once = std::sync::Once::new();
        CHILD_ENV.call_once(|| {
            crate::child_env::capture_inherited_environment().expect("child environment");
        });
        gtk::init().expect("GTK display");
        let view: Rc<TermView> = TermView::new_with_spawner(
            &crate::config::Config::safe_defaults(),
            &mode,
            &["sh".to_string()],
            None,
            false,
            None,
            "focus-restore-test",
            &[],
            &[],
            |_, _, _, _| crate::pty::OwnedPty::from_openpty(Some(true)),
        )
        .expect("nonexecuting PTY")
        .into();
        view.block_onboarding.history_resolved(false);
        let pane = vte4::Terminal::new();
        let find = gtk::Entry::new();
        let history = vte4::Terminal::new();
        let peers: Vec<Widget> = vec![pane.upcast(), find.upcast(), history.upcast()];
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.append(&view.root);
        for peer in &peers {
            column.append(peer);
        }
        let window = Window::builder()
            .default_width(640)
            .default_height(600)
            .build();
        window.set_child(Some(&column));
        window.present();
        wait_for(|| view.active_vte.is_mapped() && view.root.height() > 30);
        feed(&view, PROMPT);
        pump();
        (view, window, column, peers)
    }

    // This is the production parser/ReaderCtx callback, synchronously replayed
    // through its installed selection hold. No idle runs before the next focus
    // choice, so the formerly racy interleaving is deterministic.
    fn feed(view: &TermView, bytes: &[u8]) {
        view.selection_feed_hold.begin_drag();
        assert!(view.selection_feed_hold.try_buffer(bytes));
        view.selection_feed_hold.flush_now();
    }

    #[test]
    #[ignore = "requires DISPLAY; real parser and mapped VTE focus"]
    fn deferred_pty_focus_respects_newer_owners() {
        for mode in [
            TerminalMode::Block,
            TerminalMode::Chat,
            TerminalMode::Unified,
        ] {
            let (view, window, _, peers) = fixture(mode);
            for alt_exit in [false, true] {
                for peer in &peers {
                    if alt_exit {
                        feed(&view, b"\x1b[?1049h");
                        pump();
                        assert_eq!(view.bstate.get(), BlockState::AltScreen);
                    }
                    view.active_vte.grab_focus();
                    assert!(view.active_vte.has_focus());
                    feed(&view, if alt_exit { b"\x1b[?1049l" } else { PROMPT });
                    assert_ne!(view.bstate.get(), BlockState::AltScreen);
                    peer.grab_focus();
                    assert!(owns_focus(peer));
                    pump();
                    assert!(
                        owns_focus(peer),
                        "PTY idle stole a newer pane/find/history owner"
                    );
                }
            }
            // Positive controls for both production paths: disabling either
            // restore must not make the negative ownership cases pass alone.
            for alt_exit in [false, true] {
                for remap in [false, true] {
                    if alt_exit {
                        feed(&view, b"\x1b[?1049h");
                        pump();
                        assert_eq!(view.bstate.get(), BlockState::AltScreen);
                    }
                    view.active_vte.grab_focus();
                    assert!(view.active_vte.has_focus());
                    feed(&view, if alt_exit { b"\x1b[?1049l" } else { PROMPT });
                    gtk::prelude::GtkWindowExt::set_focus(&window, Widget::NONE);
                    if remap {
                        view.active_vte.set_visible(false);
                        view.active_vte.set_visible(true);
                    }
                    assert!(view.active_vte.is_mapped());
                    assert!(gtk::prelude::GtkWindowExt::focus(&window).is_none());
                    pump();
                    assert!(
                        view.active_vte.has_focus(),
                        "legitimate restore was lost (alt_exit={alt_exit}, remap={remap})"
                    );
                }
            }
            window.close();
            pump();
        }
    }

    #[test]
    #[ignore = "requires DISPLAY; mapped focus and widget lifecycle"]
    fn deferred_focus_preserves_remaps_and_retires_hidden_targets() {
        let (view, window, column, peers) = fixture(TerminalMode::Block);
        let terminal = &view.active_vte;
        terminal.grab_focus();
        view.render_backend.focus_live_deferred();
        gtk::prelude::GtkWindowExt::set_focus(&window, Widget::NONE);
        terminal.set_visible(false);
        terminal.set_visible(true);
        pump();
        assert!(
            terminal.has_focus(),
            "temporary same-owner remap must restore"
        );

        terminal.grab_focus();
        view.render_backend.focus_live_deferred();
        peers[0].grab_focus();
        gtk::prelude::GtkWindowExt::set_focus(&window, Widget::NONE);
        pump();
        assert!(
            !terminal.has_focus(),
            "new owner then None must retire the old idle"
        );

        terminal.grab_focus();
        view.render_backend.focus_live_deferred();
        terminal.set_visible(false);
        pump();
        terminal.set_visible(true);
        peers[1].grab_focus();
        pump();
        assert!(
            owns_focus(&peers[1]),
            "hidden target must not leave a later focus retry"
        );

        terminal.grab_focus();
        view.render_backend.focus_live_deferred();
        column.remove(&view.root);
        pump();
        assert!(
            !terminal.has_focus(),
            "detached target must not regain focus"
        );
        column.prepend(&view.root);
        peers[2].grab_focus();
        pump();
        assert!(owns_focus(&peers[2]));

        terminal.grab_focus();
        view.render_backend.focus_live_deferred();
        gtk::prelude::GtkWindowExt::set_focus(&window, Widget::NONE);
        let other_window = Window::new();
        column.remove(&view.root);
        other_window.set_child(Some(&view.root));
        other_window.set_child(Widget::NONE);
        column.prepend(&view.root);
        gtk::prelude::GtkWindowExt::set_focus(&window, Widget::NONE);
        pump();
        assert!(
            !terminal.has_focus(),
            "root round-trip must not revive an old focus request"
        );

        // Each observer captures this state. Weak expiration proves both the
        // window and target handlers were disconnected, not merely made inert.
        for retire in [false, true] {
            terminal.grab_focus();
            let restore = FocusRestore::live(terminal).unwrap();
            let state = Rc::downgrade(&restore.superseded);
            restore.defer();
            if retire {
                peers[0].grab_focus();
            }
            pump();
            assert!(state.upgrade().is_none(), "idle leaked a tracking handler");
        }
        terminal.grab_focus();
        let restore = FocusRestore::live(terminal).unwrap();
        let state = Rc::downgrade(&restore.superseded);
        drop(restore);
        assert!(
            state.upgrade().is_none(),
            "abandoned token leaked a handler"
        );

        terminal.grab_focus();
        view.render_backend.focus_live_deferred();
        window.close();
        drop(view);
        pump();

        // No pending source or observer may keep native objects alive.
        let target = vte4::Terminal::new();
        let window = Window::builder().child(&target).build();
        window.present();
        wait_for(|| target.is_mapped());
        assert!(target.grab_focus());
        restore_live_focus_deferred(&target);
        let weak_target = target.downgrade();
        let weak_window = window.downgrade();
        window.set_child(Widget::NONE);
        window.destroy();
        drop(target);
        drop(window);
        assert!(weak_target.upgrade().is_none());
        assert!(weak_window.upgrade().is_none());
        pump();
    }

    #[test]
    #[ignore = "requires DISPLAY; popover focus ownership"]
    fn popover_focus_return_retires_on_newer_owner() {
        let (view, window, _, peers) = fixture(TerminalMode::Block);
        let owners = [
            view.active_vte.clone().upcast::<Widget>(),
            peers[1].clone(),
            peers[2].clone(),
        ];
        for owner in &owners {
            for supersede in [false, true] {
                owner.grab_focus();
                let popover = gtk::Popover::new();
                popover.set_autohide(false);
                popover.set_parent(&view.block_scroll);
                let action = gtk::Button::with_label("Copy");
                popover.set_child(Some(&action));
                let restore = FocusRestore::popover(&view.active_vte, &popover).unwrap();
                popover.popup();
                pump();
                action.grab_focus();
                assert!(owns_focus(&action));
                if supersede {
                    peers[0].grab_focus();
                }
                popover.popdown();
                popover.unparent();
                restore.defer();
                pump();
                if supersede {
                    assert!(owns_focus(&peers[0]), "closed menu stole newer pane focus");
                } else {
                    assert!(
                        owns_focus(owner),
                        "ordinary menu return must preserve the original owner"
                    );
                }
            }
        }
        window.close();
        pump();
    }
}
