//! Physical Enter ownership for insert-only UI actions.
//!
//! GTK does not label repeated key-press signals. Observe presses before widgets
//! consume them, then own a confirming press until its matching real release.
//! Ownership survives focus loss; a missed release conservatively costs one
//! extra press/release cycle rather than submitting freshly inserted text.

use gtk::prelude::*;
use relm4::gtk;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum KeyId {
    Hardware(u32),
    Main,
    Keypad,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnterKind {
    Main,
    Keypad,
}

impl EnterKind {
    fn index(self) -> usize {
        match self {
            Self::Main => 0,
            Self::Keypad => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Release {
    key: KeyId,
    kind: EnterKind,
    serial: u64,
    focus_epoch: u64,
}

struct Presses {
    pressed: HashSet<KeyId>,
    kinds: HashMap<KeyId, EnterKind>,
    // A press may be an inherited repeat; only release proves this kind is up.
    uncertain: [bool; 2],
    // Armed only by review actions, never by ordinary focus/typing alone.
    unseen_owned: [bool; 2],
    owned: HashSet<KeyId>,
    pending: HashMap<KeyId, u64>,
    serial: u64,
    focus_epoch: u64,
}

impl Default for Presses {
    fn default() -> Self {
        Self {
            pressed: HashSet::new(),
            kinds: HashMap::new(),
            owned: HashSet::new(),
            pending: HashMap::new(),
            serial: 0,
            focus_epoch: 0,
            uncertain: [true; 2],
            unseen_owned: [false; 2],
        }
    }
}

impl Presses {
    fn press(&mut self, key: KeyId, kind: EnterKind) -> bool {
        self.pending.remove(&key);
        self.pressed.insert(key);
        let kind = *self.kinds.entry(key).or_insert(kind);
        if self.unseen_owned[kind.index()] {
            self.owned.insert(key);
        }
        self.owned.contains(&key)
    }
    fn claim(&mut self) {
        self.owned.extend(self.pressed.iter().copied());
        // GTK may omit keys pressed outside the window when focus returns.
        // Until a real release proves a kind is up, insertion must also own
        // an unseen held Enter. This is deliberately backend-independent.
        for (owned, uncertain) in self.unseen_owned.iter_mut().zip(self.uncertain) {
            *owned |= uncertain;
        }
    }
    fn release(&mut self, key: KeyId, kind: Option<EnterKind>) -> Option<Release> {
        let kind = self.kinds.get(&key).copied().or(kind)?;
        if !self.pressed.contains(&key)
            && !self.owned.contains(&key)
            && !self.uncertain[kind.index()]
            && !self.unseen_owned[kind.index()]
        {
            return None;
        }
        self.serial = self.serial.wrapping_add(1);
        self.pending.insert(key, self.serial);
        Some(Release {
            key,
            kind,
            serial: self.serial,
            focus_epoch: self.focus_epoch,
        })
    }
    fn settle_release(&mut self, release: Release) {
        if self.focus_epoch == release.focus_epoch
            && self.pending.get(&release.key) == Some(&release.serial)
        {
            self.pending.remove(&release.key);
            self.pressed.remove(&release.key);
            self.owned.remove(&release.key);
            self.kinds.remove(&release.key);
            self.uncertain[release.kind.index()] = false;
            self.unseen_owned[release.kind.index()] = false;
        }
    }
    fn blur(&mut self) {
        self.focus_epoch = self.focus_epoch.wrapping_add(1);
        self.pending.clear();
        self.uncertain = [true; 2];
    }
}

thread_local! {
    static PRESSES: RefCell<Presses> = RefCell::new(Presses::default());
}

fn enter_kind(key: gtk::gdk::Key) -> Option<EnterKind> {
    match key {
        gtk::gdk::Key::KP_Enter => Some(EnterKind::Keypad),
        gtk::gdk::Key::Return | gtk::gdk::Key::ISO_Enter => Some(EnterKind::Main),
        _ => None,
    }
}

fn is_enter(key: gtk::gdk::Key) -> bool {
    enter_kind(key).is_some()
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    PRESSES.with(|state| {
        let epoch = state.borrow().focus_epoch.wrapping_add(1);
        *state.borrow_mut() = Presses {
            focus_epoch: epoch,
            ..Presses::default()
        };
    });
}

fn key_id(key: gtk::gdk::Key, code: u32) -> Option<KeyId> {
    if code != 0 {
        Some(KeyId::Hardware(code))
    } else if key == gtk::gdk::Key::KP_Enter {
        Some(KeyId::Keypad)
    } else if is_enter(key) {
        Some(KeyId::Main)
    } else {
        None
    }
}

pub(crate) const RELEASE_HINT: &str = "Release Enter, then press it again.";

pub(crate) fn claim_held() {
    PRESSES.with(|state| state.borrow_mut().claim());
}

/// Install before action handlers on the same capture controller. Multiple
/// scopes observing one physical event are idempotent and share ownership.
pub(crate) fn install_controller(controller: &gtk::EventControllerKey) {
    install_controller_with_hint(controller, || {});
}

pub(crate) fn install_controller_with_hint(
    controller: &gtk::EventControllerKey,
    hint: impl Fn() + 'static,
) {
    controller.connect_key_pressed(move |_, key, code, _| {
        let Some(id) = key_id(key, code) else {
            return gtk::glib::Propagation::Proceed;
        };
        let (owned, unseen) = PRESSES.with(|state| {
            let mut state = state.borrow_mut();
            let kind = state.kinds.get(&id).copied().or_else(|| enter_kind(key));
            let unseen = kind
                .is_some_and(|kind| state.unseen_owned[kind.index()] && !state.owned.contains(&id));
            (kind.is_some_and(|kind| state.press(id, kind)), unseen)
        });
        if unseen {
            hint();
        }
        if owned {
            gtk::glib::Propagation::Stop
        } else {
            gtk::glib::Propagation::Proceed
        }
    });
    controller.connect_key_released(|controller, key, code, _| {
        // Controller reset/focus cleanup may emit key-released without a real
        // key event. Such a release is not evidence that the finger came up.
        if !controller
            .current_event()
            .is_some_and(|event| event.event_type() == gtk::gdk::EventType::KeyRelease)
        {
            return;
        }
        let Some(id) = key_id(key, code) else {
            return;
        };
        if let Some(release) = PRESSES.with(|state| state.borrow_mut().release(id, enter_kind(key)))
        {
            // Defer lifting ownership past same-turn focus loss and repeat
            // events. A new press or blur invalidates this release token.
            gtk::glib::idle_add_local_once(move || {
                PRESSES.with(|state| state.borrow_mut().settle_release(release))
            });
        }
    });
}

pub(crate) fn install_widget(widget: &impl IsA<gtk::Widget>) {
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    install_controller(&controller);
    widget.add_controller(controller);
}

pub(crate) fn watch_window(window: &impl IsA<gtk::Window>) {
    window.connect_is_active_notify(|window| {
        if !window.is_active() {
            PRESSES.with(|state| state.borrow_mut().blur());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn confirming_press_stays_owned_across_repeats_until_release_settles() {
        let mut state = Presses::default();
        let key = KeyId::Hardware(36);
        assert!(!state.press(key, EnterKind::Main));
        state.claim();
        assert!(state.press(key, EnterKind::Main));
        assert!(state.press(key, EnterKind::Main));
        let release = state.release(key, Some(EnterKind::Main)).unwrap();
        state.settle_release(release);
        assert!(!state.press(key, EnterKind::Main));
    }
    #[test]
    fn blur_or_new_press_invalidates_an_ambiguous_release() {
        let mut state = Presses::default();
        let key = KeyId::Main;
        state.press(key, EnterKind::Main);
        state.claim();
        let release = state.release(key, Some(EnterKind::Main)).unwrap();
        state.blur();
        state.settle_release(release);
        assert!(state.press(key, EnterKind::Main));
        let release = state.release(key, Some(EnterKind::Main)).unwrap();
        state.press(key, EnterKind::Main);
        state.settle_release(release);
        assert!(state.owned.contains(&key));
        let release = state.release(key, Some(EnterKind::Main)).unwrap();
        state.settle_release(release);
        assert!(!state.press(key, EnterKind::Main));
    }
    #[test]
    fn passive_presses_are_claimed_at_dispatch_and_main_keypad_are_independent() {
        let mut state = Presses::default();
        state.press(KeyId::Main, EnterKind::Main);
        state.press(KeyId::Keypad, EnterKind::Keypad);
        state.claim();
        let release = state.release(KeyId::Main, Some(EnterKind::Main)).unwrap();
        state.settle_release(release);
        assert!(!state.press(KeyId::Main, EnterKind::Main));
        assert!(state.press(KeyId::Keypad, EnterKind::Keypad));
    }

    #[test]
    fn unseen_focus_keys_are_owned_independently_until_real_release() {
        let mut state = Presses::default();
        state.claim();
        assert!(state.press(KeyId::Hardware(36), EnterKind::Main));
        assert!(state.press(KeyId::Hardware(104), EnterKind::Keypad));
        let release = state
            .release(KeyId::Hardware(36), Some(EnterKind::Main))
            .unwrap();
        state.settle_release(release);
        assert!(!state.press(KeyId::Hardware(36), EnterKind::Main));
        assert!(state.press(KeyId::Hardware(104), EnterKind::Keypad));
        let release = state
            .release(KeyId::Hardware(104), Some(EnterKind::Keypad))
            .unwrap();
        state.settle_release(release);
        assert!(!state.press(KeyId::Hardware(104), EnterKind::Keypad));
    }

    #[test]
    fn physical_identity_keeps_its_kind_if_event_labels_change_while_held() {
        let mut state = Presses::default();
        state.claim();
        assert!(state.press(KeyId::Hardware(104), EnterKind::Keypad));
        assert!(state.press(KeyId::Hardware(36), EnterKind::Main));
        assert!(state.press(KeyId::Hardware(104), EnterKind::Main));
        let release = state
            .release(KeyId::Hardware(104), Some(EnterKind::Main))
            .unwrap();
        state.settle_release(release);
        assert!(state.press(KeyId::Hardware(36), EnterKind::Main));
        assert!(!state.press(KeyId::Hardware(104), EnterKind::Keypad));
    }

    #[test]
    fn uncertain_focus_does_not_block_ordinary_enter_or_rearm_known_released_kind() {
        let mut state = Presses::default();
        for _ in 0..3 {
            assert!(!state.press(KeyId::Main, EnterKind::Main));
        }
        let release = state.release(KeyId::Main, Some(EnterKind::Main)).unwrap();
        state.settle_release(release);
        state.claim();
        assert!(!state.press(KeyId::Main, EnterKind::Main));
        let release = state.release(KeyId::Main, Some(EnterKind::Main)).unwrap();
        state.blur();
        state.settle_release(release);
        state.claim();
        assert!(state.press(KeyId::Main, EnterKind::Main));
    }

    #[test]
    #[ignore = "requires isolated GTK display and XTest; nonexecuting PTY recorder"]
    fn queued_review_write_claims_enter_at_both_backend_boundaries() {
        use crate::config::{Config, TerminalMode};
        use crate::terminal::{
            BlockTerminal, InitialCommands, PaneProbe, VteInit, VteInput, VteTerminal,
        };
        use relm4::prelude::*;
        use std::io::Write;
        use std::rc::Rc;
        use std::time::{Duration, Instant, SystemTime};
        crate::child_env::capture_inherited_environment().expect("freeze test child environment");
        gtk::init().expect("GTK display");
        relm4::adw::init().expect("adwaita");
        fn pump() {
            for _ in 0..64 {
                if !gtk::glib::MainContext::default().iteration(false) {
                    break;
                }
            }
        }
        for backend in [
            TerminalMode::Vte,
            TerminalMode::Block,
            TerminalMode::Unified,
        ] {
            for mode in ["queued", "queued-keypad"] {
                let fresh = std::env::var_os("ANVIL_QA_FRESH_ENTER").is_some();
                let mode = if fresh {
                    format!("{mode}-fresh")
                } else {
                    mode.to_string()
                };
                reset_for_test();
                let directory = std::env::temp_dir().join(format!(
                    "anvil-queued-enter-{}-{}",
                    std::process::id(),
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                std::fs::create_dir(&directory).unwrap();
                let capture = directory.join("capture");
                let ready = directory.join("ready");
                let stop = directory.join("stop");
                let held = directory.join("held");
                let init = VteInit {
                    config: Rc::new(RefCell::new(Config::safe_defaults())),
                    mode: backend,
                    shell_argv: Rc::new(vec![
                        "python3".into(),
                        format!("{}/scripts/qa-pty-recorder.py", env!("CARGO_MANIFEST_DIR")),
                        capture.to_string_lossy().into_owned(),
                        ready.to_string_lossy().into_owned(),
                        stop.to_string_lossy().into_owned(),
                    ]),
                    working_directory: None,
                    working_directory_external: false,
                    session_id: None,
                    cwd_token: "nonsecret-test-only".into(),
                    initial_commands: InitialCommands::default(),
                    probe: PaneProbe::default(),
                    env_extra: Vec::new(),
                };
                type Fixture = (gtk::Widget, Box<dyn Fn(VteInput)>, Box<dyn std::any::Any>);
                let (widget, send, component): Fixture = if backend == TerminalMode::Vte {
                    let controller = VteTerminal::builder().launch(init).detach();
                    let widget = controller.widget().clone().upcast();
                    let sender = controller.sender().clone();
                    (
                        widget,
                        Box::new(move |message| sender.send(message).unwrap()),
                        Box::new(controller),
                    )
                } else {
                    let controller = BlockTerminal::builder().launch(init).detach();
                    assert!(
                        controller.model().term_view().is_some(),
                        "production backend started: {:?}",
                        controller.model().launch_error()
                    );
                    let widget = controller.widget().clone().upcast();
                    let sender = controller.sender().clone();
                    (
                        widget,
                        Box::new(move |message| sender.send(message).unwrap()),
                        Box::new(controller),
                    )
                };
                let passive = gtk::Entry::new();
                let keys = gtk::EventControllerKey::new();
                keys.set_propagation_phase(gtk::PropagationPhase::Capture);
                keys.connect_key_pressed(|_, key, _, _| {
                    if is_enter(key) {
                        gtk::glib::Propagation::Stop
                    } else {
                        gtk::glib::Propagation::Proceed
                    }
                });
                passive.add_controller(keys);
                let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
                body.append(&passive);
                body.append(&widget);
                let window = gtk::Window::builder()
                    .title("anvil-cross-selection-qa")
                    .child(&body)
                    .default_width(640)
                    .default_height(480)
                    .build();
                watch_window(&window);
                install_widget(&window);
                window.present();
                let deadline = Instant::now() + Duration::from_secs(5);
                while !ready.exists() {
                    pump();
                    assert!(Instant::now() < deadline, "raw recorder did not start");
                    std::thread::sleep(Duration::from_millis(10));
                }
                passive.grab_focus();
                for _ in 0..10 {
                    pump();
                    std::thread::sleep(Duration::from_millis(10));
                }
                let mut driver = std::process::Command::new("python3")
                    .arg(format!(
                        "{}/scripts/qa-enter-ownership.py",
                        env!("CARGO_MANIFEST_DIR")
                    ))
                    .arg(&mode)
                    .arg(&held)
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut queued = false;
                let drop_path = directory.join("inert file 'quoted' 中.txt");
                std::fs::write(&drop_path, b"inert fixture").unwrap();
                let file_drop = std::env::var_os("ANVIL_QA_FILE_DROP").is_some();
                let payload = if file_drop {
                    crate::file_drop::prompt_payload(std::slice::from_ref(&drop_path)).unwrap()
                } else {
                    "echo queued".to_string()
                };
                loop {
                    pump();
                    if held.exists() && !queued {
                        PRESSES.with(|state| {
                            let state = state.borrow();
                            if std::env::var("ANVIL_QA_INHERITED_ENTER").as_deref() != Ok("1") {
                                assert!(!state.pressed.is_empty());
                            }
                            eprintln!(
                                "observed Enter identities before queued insertion: {}",
                                state.pressed.len()
                            );
                            assert!(
                                state.owned.is_empty(),
                                "passive key has not been claimed at dispatch"
                            );
                        });
                        // Deliberately do not claim here: this proves the actual
                        // queued backend update owns the held key at write time.
                        if file_drop {
                            send(VteInput::GrabFocus);
                            send(VteInput::PasteText(payload.clone()));
                        } else {
                            send(VteInput::InsertReviewText(payload.as_bytes().to_vec()));
                            send(VteInput::GrabFocus);
                        }
                        writeln!(driver.stdin.as_mut().unwrap(), "go").unwrap();
                        queued = true;
                    }
                    if let Some(status) = driver.try_wait().unwrap() {
                        assert!(status.success());
                        break;
                    }
                    if Instant::now() >= deadline {
                        let _ = driver.kill();
                        let _ = driver.wait();
                        panic!("XTest queued fixture timeout");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(queued);
                for _ in 0..10 {
                    pump();
                    std::thread::sleep(Duration::from_millis(10));
                }
                let bytes = std::fs::read(&capture).unwrap();
                std::fs::write(&stop, b"stop").unwrap();
                eprintln!(
                    "queued {backend:?} {mode}: {:?}",
                    String::from_utf8_lossy(&bytes)
                );
                let mut expected = payload.into_bytes();
                if fresh {
                    expected.push(b'\r');
                }
                assert_eq!(bytes, expected, "the actual queued write must not submit");
                window.close();
                for _ in 0..10 {
                    pump();
                    std::thread::sleep(Duration::from_millis(10));
                }
                drop(component);
                for path in [&capture, &ready, &stop, &held, &drop_path] {
                    std::fs::remove_file(path).unwrap();
                }
                std::fs::remove_dir(&directory).unwrap();
            }
        }
    }
}
