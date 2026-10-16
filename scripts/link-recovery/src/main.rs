// Hosted-only candidate. No GTK initialization, display, clipboard, or browser.
mod terminal {
    pub mod url_recovery {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/recovery_body.rs"));
    }
    pub mod url {
        use gio::prelude::*;
        use std::{
            cell::{Cell, RefCell},
            rc::Rc,
            time::{Duration, Instant},
        };
        mod gtk {
            pub use glib;
        }
        use super::url_recovery::{LaunchOutcome, OpenFailure, RecoveryOwner};
        include!("extracted.rs");

        #[derive(Clone)]
        struct ProbeOpener {
            state: Rc<RefCell<RecoveryOwner>>,
            replacement: Rc<RefCell<Option<Rc<()>>>>,
            loop_: glib::MainLoop,
            done: Rc<Cell<bool>>,
            ticks: Rc<Cell<u32>>,
            start: Instant,
            case: String,
        }
        impl ProbeOpener {
            fn finish_launch(&self, ticket: Rc<()>, original: String, outcome: LaunchOutcome) {
                assert!(!self.done.replace(true), "duplicate completion");
                assert!(
                    original.as_bytes() == std::fs::read("expected.bin").unwrap(),
                    "original URI bytes changed"
                );
                let expected = match self.case.as_str() {
                    "chooser-cancel" | "client-cancel" => LaunchOutcome::Cancelled,
                    "deny" | "unknown" | "stale" => LaunchOutcome::Failed(OpenFailure::Failed),
                    "activation-denied" => LaunchOutcome::Failed(OpenFailure::PermissionDenied),
                    "missing" => LaunchOutcome::Failed(OpenFailure::MissingHandler),
                    "record" | "portal-success" => LaunchOutcome::Opened,
                    _ => panic!("unknown case"),
                };
                assert_eq!(
                    outcome, expected,
                    "actual GIO completion classified incorrectly"
                );
                let result = self
                    .state
                    .borrow_mut()
                    .complete_launch(&ticket, true, outcome);
                if self.case == "stale" {
                    assert_eq!(result, (false, None));
                    let current = self
                        .replacement
                        .borrow_mut()
                        .take()
                        .expect("replacement ticket");
                    assert!(
                        self.state.borrow_mut().consume(&current),
                        "old callback cleared new ownership"
                    );
                } else if matches!(outcome, LaunchOutcome::Cancelled | LaunchOutcome::Opened) {
                    assert_eq!(result, (true, None));
                } else {
                    assert!(result.0 && result.1.is_some());
                }
                if !matches!(self.case.as_str(), "missing" | "record") {
                    assert!(
                        self.ticks.get() >= 5,
                        "main context did not remain responsive"
                    );
                    assert!(
                        self.start.elapsed() >= Duration::from_millis(200),
                        "not delayed"
                    );
                }
                // Never print raw backend messages or target contents.
                println!(
                    "PASS case={} outcome={:?} ticks={}",
                    self.case,
                    outcome,
                    self.ticks.get()
                );
                self.loop_.quit();
            }
        }
        pub fn run() {
            assert_eq!(std::env::var("ANVIL_HOSTED_FIXTURE").as_deref(), Ok("1"));
            let case = std::env::args().nth(1).expect("case");
            let uri = String::from_utf8(std::fs::read("expected.bin").unwrap()).unwrap();
            let loop_ = glib::MainLoop::new(None, false);
            let context = glib::MainContext::default();
            let _guard = context.acquire().expect("main context ownership");
            let state = Rc::new(RefCell::new(RecoveryOwner::default()));
            let ticket = state.borrow_mut().begin();
            let replacement = Rc::new(RefCell::new(None));
            let ticks = Rc::new(Cell::new(0));
            let tick_count = ticks.clone();
            glib::timeout_add_local(Duration::from_millis(20), move || {
                tick_count.set(tick_count.get() + 1);
                glib::ControlFlow::Continue
            });
            let cancel = gio::Cancellable::new();
            let opener = ProbeOpener {
                state: state.clone(),
                replacement: replacement.clone(),
                loop_: loop_.clone(),
                done: Rc::new(Cell::new(false)),
                ticks,
                start: Instant::now(),
                case: case.clone(),
            };
            let original = uri.clone();
            dispatch_uri(&uri, &cancel, move |outcome| {
                opener.finish_launch(ticket, original, outcome)
            })
            .unwrap();
            if case == "client-cancel" || case == "stale" {
                // Wait until the actual D-Bus receiver saw OpenURI, not a guessed sleep.
                glib::timeout_add_local(Duration::from_millis(10), move || {
                    if !std::path::Path::new("portal.received").exists() {
                        return glib::ControlFlow::Continue;
                    }
                    if case == "client-cancel" {
                        let later = cancel.clone();
                        glib::timeout_add_local_once(Duration::from_millis(250), move || {
                            later.cancel()
                        });
                    } else {
                        *replacement.borrow_mut() = Some(state.borrow_mut().begin());
                    }
                    glib::ControlFlow::Break
                });
            }
            glib::timeout_add_local(Duration::from_secs(8), || panic!("GIO boundary timed out"));
            loop_.run();
        }
    }
}
fn main() {
    terminal::url::run();
}
