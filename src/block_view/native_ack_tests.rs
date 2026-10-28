//! Isolated SDK capability experiment. No production feed, crop or grid uses it.
//!
//! Run the ignored test alone in a fresh process: extension registration must
//! precede every VteTerminal, including terminals constructed by other tests.
use gtk::{glib, prelude::*};
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::CStr;
use std::rc::Rc;
use std::time::{Duration, Instant};
use vte4::{Terminal, TerminalExt};

const PROPERTY: &CStr = c"vte.ext.anvil.nativeack";
const SIGNAL: &CStr = c"termprop-changed::vte.ext.anvil.nativeack";
const MAX_HELD_BYTES: usize = 4096;

type Install = unsafe extern "C" fn(*const libc::c_char, libc::c_int, libc::c_uint) -> libc::c_int;
type GetUint =
    unsafe extern "C" fn(*mut vte4::ffi::VteTerminal, libc::c_int, *mut u64) -> libc::c_int;

#[derive(Clone, Copy)]
struct OptionalApi {
    install: Install,
    get_uint: GetUint,
}

impl OptionalApi {
    fn resolve(mut lookup: impl FnMut(&CStr) -> *mut libc::c_void) -> Option<Self> {
        let install = lookup(c"vte_install_termprop");
        let get_uint = lookup(c"vte_terminal_get_termprop_uint_by_id");
        if install.is_null() || get_uint.is_null() {
            return None;
        }
        // Signatures are from the installed official VTE headers. These are
        // optional lookups, not ELF references that raise the v0_76 minimum.
        Some(unsafe {
            Self {
                install: std::mem::transmute::<*mut libc::c_void, Install>(install),
                get_uint: std::mem::transmute::<*mut libc::c_void, GetUint>(get_uint),
            }
        })
    }

    fn load() -> Option<Self> {
        Self::resolve(|name| unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) })
    }
}

struct Receipt {
    api: OptionalApi,
    id: i32,
    values: Rc<RefCell<VecDeque<u64>>>,
    all_values: Rc<RefCell<Vec<u64>>>,
    inside_signal: Rc<Cell<bool>>,
}

unsafe extern "C" fn termprop_changed(
    terminal: *mut vte4::ffi::VteTerminal,
    _name: *mut libc::c_char,
    data: glib::ffi::gpointer,
) {
    let receipt = unsafe { &*(data as *const Receipt) };
    receipt.inside_signal.set(true);
    let mut value = 0;
    // The ONLY terminal API in this callback is the documented termprop
    // accessor. No cursor, geometry, parent, widget, clone or reset API runs.
    if unsafe { (receipt.api.get_uint)(terminal, receipt.id, &mut value) } != 0 {
        receipt.values.borrow_mut().push_back(value);
        receipt.all_values.borrow_mut().push(value);
    }
    receipt.inside_signal.set(false);
}

fn connect_receipt(terminal: &Terminal, receipt: Receipt) {
    unsafe {
        glib::signal::connect_raw(
            terminal.as_ptr() as *mut _,
            SIGNAL.as_ptr(),
            Some(std::mem::transmute::<
                unsafe extern "C" fn(
                    *mut vte4::ffi::VteTerminal,
                    *mut libc::c_char,
                    glib::ffi::gpointer,
                ),
                unsafe extern "C" fn(),
            >(termprop_changed)),
            Box::into_raw(Box::new(receipt)),
        );
    }
}

fn payload(prefix: &[u8], nonce: Option<u64>) -> Vec<u8> {
    let mut bytes = prefix.to_vec();
    if let Some(nonce) = nonce {
        bytes.extend_from_slice(
            format!("\x1b]666;{}={nonce}\x1b\\", PROPERTY.to_str().unwrap()).as_bytes(),
        );
    }
    bytes
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Ack,
    WrongNonce,
    Timeout,
    Resize,
    Input,
    Unavailable,
}

struct Fence {
    waiting: bool,
    held: Vec<Vec<u8>>,
    original_fed: Vec<u8>,
    initial_grid: (i64, i64),
    initial_input: u64,
    outcome: Option<Outcome>,
    cursor: Option<(i64, i64)>,
    sample_outside_signal: bool,
    suffix_flushes: u32,
}

impl Fence {
    fn finish(&mut self, terminal: &Terminal, outcome: Outcome) {
        if !self.waiting {
            return;
        }
        self.waiting = false;
        self.outcome = Some(outcome);
        self.suffix_flushes += 1;
        for bytes in self.held.drain(..) {
            self.original_fed.extend_from_slice(&bytes);
            terminal.feed(&bytes);
        }
    }
}

fn pump_until(label: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(
            Instant::now() < deadline,
            "native ACK prototype timeout: {label}"
        );
        for _ in 0..256 {
            if !glib::MainContext::default().iteration(false) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn visible(terminal: &Terminal) -> String {
    terminal
        .text_format(vte4::Format::Text)
        .unwrap()
        .to_string()
}

fn run_case(api: Option<(OptionalApi, i32)>, case: Outcome) -> serde_json::Value {
    let prefix = "\x1b]2;Native ACK fixture\x1b\\\x1b[36m$ printf '界🙂'\x1b[0m\r\nprefix-done\r\n"
        .as_bytes();
    // Deliberately split both SGR and UTF-8 across original suffix chunks.
    let suffix = vec![
        b"\x1b[32".to_vec(),
        b"mOUT_\xe7\x95".to_vec(),
        b"\x8c\x1b[0m\r\nACK_SUFFIX_DONE\r\n".to_vec(),
    ];
    assert!(suffix.iter().map(Vec::len).sum::<usize>() <= MAX_HELD_BYTES);
    let expected_original: Vec<u8> = prefix
        .iter()
        .copied()
        .chain(suffix.iter().flatten().copied())
        .collect();
    let nonce = (uuid::Uuid::new_v4().as_u128() as u64).max(1);
    let pty = Rc::new(crate::pty::OwnedPty::from_openpty(Some(true)).unwrap());
    let kernel_before = pty.test_slave_winsize();
    let terminal = Terminal::new();
    let oracle = Terminal::new();
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.set_homogeneous(true);
    for widget in [&terminal, &oracle] {
        widget.set_font(Some(&gtk::pango::FontDescription::from_string(
            "Monospace 12px",
        )));
        widget.set_size(64, 12);
        widget.set_scrollback_lines(100);
        widget.set_hexpand(true);
        widget.set_vexpand(true);
        row.append(widget);
    }
    let window = gtk::Window::builder()
        .title("Anvil native ACK prototype — test fixture")
        .default_width(1050)
        .default_height(340)
        .child(&row)
        .build();
    window.present();
    pump_until("two original native surfaces mapped", || {
        terminal.is_mapped() && oracle.is_mapped() && terminal.width() > 100
    });
    assert_eq!(terminal.column_count(), oracle.column_count());
    assert_eq!(terminal.row_count(), oracle.row_count());
    let grid_before = (terminal.column_count(), terminal.row_count());
    let titles = Rc::new(Cell::new(0_u32));
    let count = titles.clone();
    terminal.connect_window_title_changed(move |_| count.set(count.get() + 1));
    oracle.feed(prefix);
    // Only the text oracle's preparation uses fixture text readiness. It is
    // never used as the prototype's parser receipt or projection authority.
    pump_until("unmarked prefix oracle parsed", || {
        visible(&oracle).contains("prefix-done")
    });
    let prefix_text = visible(&oracle);
    let prefix_cursor = oracle.cursor_position();
    let prefix_title = oracle.window_title();
    let pending = Rc::new(RefCell::new(VecDeque::new()));
    let observations = Rc::new(RefCell::new(Vec::new()));
    let inside_signal = Rc::new(Cell::new(false));
    if let Some((api, id)) = api {
        connect_receipt(
            &terminal,
            Receipt {
                api,
                id,
                values: pending.clone(),
                all_values: observations.clone(),
                inside_signal: inside_signal.clone(),
            },
        );
    }
    let input_generation = Rc::new(Cell::new(0_u64));
    let fence = Rc::new(RefCell::new(Fence {
        waiting: true,
        held: suffix.clone(),
        original_fed: prefix.to_vec(),
        initial_grid: grid_before,
        initial_input: 0,
        outcome: None,
        cursor: None,
        sample_outside_signal: false,
        suffix_flushes: 0,
    }));
    let commit_count = Rc::new(Cell::new(0_u32));
    let (weak_fence, writer, generation, commits) = (
        Rc::downgrade(&fence),
        pty.clone(),
        input_generation.clone(),
        commit_count.clone(),
    );
    terminal.connect_commit(move |terminal, text, size| {
        assert_eq!(text.len(), size as usize, "fixture native commit length");
        generation.set(generation.get() + 1);
        commits.set(commits.get() + 1);
        if let Some(fence) = weak_fence.upgrade() {
            fence.borrow_mut().finish(terminal, Outcome::Input);
        }
        writer.try_write_bytes(text.as_bytes()).unwrap();
    });
    let marker = match case {
        Outcome::Timeout | Outcome::Unavailable => None,
        Outcome::WrongNonce => Some(nonce.wrapping_add(1)),
        _ => Some(nonce),
    };
    terminal.feed(&payload(prefix, marker));
    let timer_fired = Rc::new(Cell::new(false));
    let (weak_fence, weak_terminal, fired) = (
        Rc::downgrade(&fence),
        terminal.downgrade(),
        timer_fired.clone(),
    );
    let timer = glib::timeout_add_local_once(
        if case == Outcome::Timeout {
            Duration::from_millis(30)
        } else {
            Duration::from_secs(2)
        },
        move || {
            fired.set(true);
            if let (Some(fence), Some(terminal)) = (weak_fence.upgrade(), weak_terminal.upgrade()) {
                fence.borrow_mut().finish(&terminal, Outcome::Timeout);
            }
        },
    );
    match case {
        Outcome::Resize => {
            // The test-only resize invalidation happens at request time, not
            // from a guessed cursor/adjustment origin after reflow.
            fence.borrow_mut().finish(&terminal, Outcome::Resize);
            window.set_default_size(1170, 340);
        }
        Outcome::Input => {
            // Explicit native commit-signal fault injection, not an IME or
            // physical-keyboard claim. It proves the PTY ledger is live too.
            terminal.emit_by_name::<()>("commit", &[&"z", &1_u32]);
        }
        Outcome::Unavailable => fence.borrow_mut().finish(&terminal, Outcome::Unavailable),
        _ => {}
    }
    let mut sample_scheduled = false;
    pump_until("nonce receipt or ordered native fallback", || {
        if !fence.borrow().waiting {
            return true;
        }
        let value = pending.borrow_mut().pop_front();
        if let Some(value) = value {
            if value != nonce {
                fence.borrow_mut().finish(&terminal, Outcome::WrongNonce);
            } else if !sample_scheduled {
                sample_scheduled = true;
                let expected_text = prefix_text.clone();
                let expected_title = prefix_title.clone();
                let (weak_fence, weak_terminal, inside, generation) = (
                    Rc::downgrade(&fence),
                    terminal.downgrade(),
                    inside_signal.clone(),
                    input_generation.clone(),
                );
                glib::idle_add_local_once(move || {
                    let (Some(fence), Some(terminal)) =
                        (weak_fence.upgrade(), weak_terminal.upgrade())
                    else {
                        return;
                    };
                    let mut state = fence.borrow_mut();
                    if !state.waiting {
                        return;
                    }
                    assert!(
                        !inside.get(),
                        "cursor must never be read in the termprop callback"
                    );
                    if state.initial_input != generation.get() {
                        state.finish(&terminal, Outcome::Input);
                    } else if state.initial_grid != (terminal.column_count(), terminal.row_count())
                    {
                        state.finish(&terminal, Outcome::Resize);
                    } else {
                        // No following native bytes have been fed: this is a
                        // native parser-prefix cursor, not a painted/pixel or
                        // ring-to-adjustment coordinate calibration claim.
                        assert_eq!(state.original_fed, prefix);
                        state.cursor = Some(terminal.cursor_position());
                        state.sample_outside_signal = true;
                        assert_eq!(state.cursor, Some(prefix_cursor));
                        assert_eq!(visible(&terminal), expected_text);
                        assert_eq!(terminal.window_title(), expected_title);
                        state.finish(&terminal, Outcome::Ack);
                    }
                });
            }
        }
        !fence.borrow().waiting
    });
    if !timer_fired.get() {
        timer.remove();
    }
    for chunk in &suffix {
        oracle.feed(chunk);
    }
    pump_until("held original suffix parsed once in order", || {
        visible(&terminal).contains("ACK_SUFFIX_DONE") && visible(&terminal) == visible(&oracle)
    });
    if case == Outcome::Resize {
        pump_until("real GTK resize applied", || {
            window.width() == 1170 && (terminal.column_count(), terminal.row_count()) != grid_before
        });
        assert_eq!(terminal.column_count(), oracle.column_count());
    }
    assert_eq!(terminal.window_title(), prefix_title);
    assert_eq!(
        titles.get(),
        1,
        "the private marker must not publish title changes"
    );
    let state = fence.borrow();
    assert_eq!(state.outcome, Some(case));
    assert_eq!(state.original_fed, expected_original);
    assert_eq!(state.suffix_flushes, 1);
    assert!(state.held.is_empty());
    assert_eq!(state.cursor.is_some(), case == Outcome::Ack);
    assert_eq!(
        pty.test_slave_winsize(),
        kernel_before,
        "the prototype never changes PTY dimensions"
    );
    let input = pty.drain_test_slave(Duration::from_millis(30));
    if case == Outcome::Input {
        assert_eq!(input, b"z");
        assert_eq!(commit_count.get(), 1);
    } else {
        assert!(
            input.is_empty(),
            "native marker/feed must never write to the PTY"
        );
        assert_eq!(commit_count.get(), 0);
    }
    if matches!(case, Outcome::Ack | Outcome::WrongNonce) {
        assert!(
            !observations.borrow().is_empty(),
            "real VTE must emit a termprop receipt"
        );
    }
    let report = serde_json::json!({
        "case": case, "outcome": state.outcome, "nonce": nonce,
        "native_observed_values": &*observations.borrow(),
        "native_cursor_sample": state.cursor, "sample_outside_signal": state.sample_outside_signal,
        "held_suffix_bytes": suffix.iter().map(Vec::len).sum::<usize>(), "held_limit": MAX_HELD_BYTES,
        "original_bytes_fed_exactly": true, "suffix_flushes": state.suffix_flushes,
        "native_grid_before": grid_before, "native_grid_after": [terminal.column_count(),terminal.row_count()],
        "pty_grid_before": kernel_before, "pty_grid_after": pty.test_slave_winsize(),
        "pty_bytes": input, "commit_count": commit_count.get(), "title_change_count": titles.get(),
        "visible_text_equals_unmarked_native_oracle": true, "window_title_unchanged": true,
        "resize_input_hooks": "test-only explicit invalidation; no production wiring",
        "input_case": "explicit native commit signal positive control; not physical keyboard/IME proof"
    });
    drop(state);
    window.close();
    report
}

#[test]
fn missing_optional_abi_never_marks_the_original_prefix() {
    assert!(OptionalApi::resolve(|_| std::ptr::null_mut()).is_none());
    assert_eq!(
        payload(b"original\x1b[32mbytes", None),
        b"original\x1b[32mbytes"
    );
}

#[test]
#[ignore = "requires native DISPLAY; run this test ALONE before any other VteTerminal in its process"]
fn native_termprop_nonce_ack_and_ordered_fallback_isolated_process() {
    let args: Vec<_> = std::env::args().collect();
    assert!(
        args.iter().any(|arg| arg == "--exact")
            && args.iter().any(|arg| arg == "block_view::native_ack_tests::native_termprop_nonce_ack_and_ordered_fallback_isolated_process"),
        "run this ignored registration experiment as the sole exact test in a fresh process"
    );
    gtk::init().unwrap();
    let native_version = unsafe {
        [
            vte4::ffi::vte_get_major_version(),
            vte4::ffi::vte_get_minor_version(),
            vte4::ffi::vte_get_micro_version(),
        ]
    };
    // No Terminal or font-measurement Terminal has been created in this
    // exact-test process. Registration is once, before the first instance.
    let api = OptionalApi::load();
    let installed = api.map(|api| {
        // Official vteenums.h: VTE_PROPERTY_UINT=3, FLAG_NONE=0.
        let id = unsafe { (api.install)(PROPERTY.as_ptr(), 3, 0) };
        assert!(
            id >= 0,
            "SDK capability present but extension registration failed"
        );
        (api, id)
    });
    let mut results = Vec::new();
    if let Some(api) = installed {
        for case in [
            Outcome::Ack,
            Outcome::WrongNonce,
            Outcome::Timeout,
            Outcome::Resize,
            Outcome::Input,
        ] {
            results.push(run_case(Some(api), case));
        }
    }
    results.push(run_case(None, Outcome::Unavailable));
    eprintln!(
        "NATIVE_ACK_PROTOTYPE {}",
        serde_json::json!({
            "native_ack_verified": installed.is_some(), "optional_v0_78_symbols_present": installed.is_some(),
        "native_vte_version": native_version,
            "cargo_minimum_unchanged": "v0_76", "registration_before_first_terminal": true,
            "handler_terminal_api": ["vte_terminal_get_termprop_uint_by_id"],
            "no_production_crop_or_gutter": true, "no_pty_resize_calls": true,
            "actual_v0_76_runtime_not_tested": true, "results": results
        })
    );
}
