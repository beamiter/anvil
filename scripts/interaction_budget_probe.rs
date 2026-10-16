//! Benchmark-only observer. Compiled ONLY by interaction-budget-overlay.patch.
//! Normal main.rs does not declare this module; there is deliberately no Cargo feature.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;

use relm4::gtk::{self, glib, prelude::*};
use relm4::ComponentSender;
use serde::Deserialize;
use serde_json::{json, Value};
use vte4::prelude::*;

const LIMIT: usize = 100_000;
const MARKER_LIMIT: usize = 10_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Setup,
    Active,
    Idle,
    Finished,
}

struct State {
    directory: Option<PathBuf>,
    request: u32,
    phase: Phase,
    dispatch_id: u64,
    current_dispatch: Option<u64>,
    dispatch: Vec<(u64, u64)>,
    consumer: Vec<(u64, u64)>,
    consumed_bytes: u64,
    idle_dispatches: u64,
    idle_empty: u64,
    idle_consumer: u64,
    idle_app_updates: u64,
    idle_observer_callbacks: u64,
    vte_spawns: u64,
    fallback: bool,
    overflow: bool,
    tails: BTreeMap<i32, Vec<u8>>,
    pending: BTreeMap<i32, u32>,
    seen: BTreeMap<i32, u32>,
    painted: Vec<(i32, u32, u64)>,
    geometry: Vec<Value>,
    last_geometry: BTreeMap<i32, (i32, i32, i64, i64)>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            directory: None,
            request: 0,
            phase: Phase::Setup,
            dispatch_id: 0,
            current_dispatch: None,
            dispatch: Vec::with_capacity(LIMIT),
            consumer: Vec::with_capacity(LIMIT),
            consumed_bytes: 0,
            idle_dispatches: 0,
            idle_empty: 0,
            idle_consumer: 0,
            idle_app_updates: 0,
            idle_observer_callbacks: 0,
            vte_spawns: 0,
            fallback: false,
            overflow: false,
            tails: BTreeMap::new(),
            pending: BTreeMap::new(),
            seen: BTreeMap::new(),
            painted: Vec::with_capacity(MARKER_LIMIT),
            geometry: Vec::with_capacity(MARKER_LIMIT),
            last_geometry: BTreeMap::new(),
        }
    }
}
thread_local! { static STATE: RefCell<State> = RefCell::new(State::default()); }

/// CLOCK_MONOTONIC is shared with the external XTest driver, unlike Instant's epoch.
fn now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid writable timespec; CLOCK_MONOTONIC is a constant.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    assert_eq!(result, 0, "benchmark monotonic clock unavailable");
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

pub(crate) struct Sample {
    start: u64,
    id: u64,
    parent: Option<u64>,
    phase: Phase,
    consumer: bool,
    has_data: bool,
}

pub(crate) fn dispatch() -> Sample {
    STATE.with(|cell| {
        let mut s = cell.borrow_mut();
        let id = s.dispatch_id;
        s.dispatch_id += 1;
        let parent = s.current_dispatch.replace(id);
        Sample {
            start: now_ns(),
            id,
            parent,
            phase: s.phase,
            consumer: false,
            has_data: false,
        }
    })
}

impl Sample {
    pub(crate) fn data(&mut self) {
        self.has_data = true;
    }
}

pub(crate) fn consumer(pid: i32, bytes: &[u8]) -> Sample {
    STATE.with(|cell| {
        let mut s = cell.borrow_mut();
        let id = s.current_dispatch.unwrap_or(u64::MAX);
        let phase = s.phase;
        if phase == Phase::Active {
            s.consumed_bytes += bytes.len() as u64;
            // Store only the 20-byte boundary tail; never persist terminal contents.
            // This observer recognizes synthetic numeric markers, not OSC/trust metadata.
            let mut combined = s.tails.remove(&pid).unwrap_or_default();
            combined.extend_from_slice(bytes);
            let mut newest = None;
            for window in combined.windows(10) {
                if &window[..4] == b"A04K" && window[4..10].iter().all(u8::is_ascii_digit) {
                    let seq = window[4..10]
                        .iter()
                        .fold(0u32, |n, b| n * 10 + (b - b'0') as u32);
                    if seq > 0 {
                        newest = Some(seq);
                    }
                }
            }
            if let Some(seq) = newest {
                if seq > *s.seen.get(&pid).unwrap_or(&0) {
                    s.pending.insert(pid, seq);
                }
            }
            let tail = combined[combined.len().saturating_sub(20)..].to_vec();
            s.tails.insert(pid, tail);
        }
        Sample {
            start: now_ns(),
            id,
            parent: None,
            phase,
            consumer: true,
            has_data: true,
        }
    })
}

impl Drop for Sample {
    fn drop(&mut self) {
        let elapsed = now_ns().saturating_sub(self.start);
        STATE.with(|cell| {
            let mut s = cell.borrow_mut();
            if self.phase == Phase::Active {
                let values = if self.consumer {
                    &mut s.consumer
                } else {
                    &mut s.dispatch
                };
                if values.len() < LIMIT {
                    values.push((self.id, elapsed));
                } else {
                    s.overflow = true;
                }
            } else if self.phase == Phase::Idle {
                if self.consumer {
                    s.idle_consumer += 1;
                } else {
                    s.idle_dispatches += 1;
                    if !self.has_data {
                        s.idle_empty += 1;
                    }
                }
            }
            if !self.consumer {
                s.current_dispatch = self.parent;
            }
        });
    }
}

pub(crate) fn fallback() {
    STATE.with(|s| s.borrow_mut().fallback = true);
}
pub(crate) fn vte_spawn() {
    STATE.with(|s| s.borrow_mut().vte_spawns += 1);
}
pub(crate) fn app_update() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.phase == Phase::Idle {
            s.idle_app_updates += 1;
        }
    });
}

struct Surface {
    terminal: glib::WeakRef<vte4::Terminal>,
    root: glib::WeakRef<gtk::Widget>,
    pid: i32,
    signal: Option<(gtk::gdk::FrameClock, glib::SignalHandlerId)>,
}
thread_local! { static SURFACES: RefCell<Vec<Surface>> = const { RefCell::new(Vec::new()) }; }

pub(crate) fn register_surface(terminal: &vte4::Terminal, root: &gtk::Widget, pid: i32) {
    SURFACES.with(|surfaces| {
        surfaces.borrow_mut().push(Surface {
            terminal: terminal.downgrade(),
            root: root.downgrade(),
            pid,
            signal: None,
        })
    });
}

fn set_frame_observers(enabled: bool) {
    SURFACES.with(|surfaces| {
        for surface in surfaces.borrow_mut().iter_mut() {
            if let Some((clock, id)) = surface.signal.take() {
                clock.disconnect(id);
            }
            if !enabled {
                continue;
            }
            let Some(terminal) = surface.terminal.upgrade() else {
                continue;
            };
            let Some(clock) = terminal.frame_clock() else {
                continue;
            };
            let weak = surface.terminal.clone();
            let weak_root = surface.root.clone();
            let pid = surface.pid;
            let signal = clock.connect_after_paint(move |_| {
                let active = STATE.with(|s| {
                    let mut s = s.borrow_mut();
                    if s.phase == Phase::Idle {
                        s.idle_observer_callbacks += 1;
                    }
                    s.phase == Phase::Active
                });
                if !active {
                    return;
                }
                let (Some(terminal), Some(root)) = (weak.upgrade(), weak_root.upgrade()) else {
                    return;
                };
                if !terminal.is_mapped() {
                    return;
                }
                let stamp = now_ns();
                let geometry = (
                    root.width(),
                    root.height(),
                    terminal.column_count(),
                    terminal.row_count(),
                );
                let pending = STATE.with(|cell| {
                    let mut s = cell.borrow_mut();
                    if s.last_geometry.get(&pid) != Some(&geometry) {
                        s.last_geometry.insert(pid, geometry);
                        if s.geometry.len() < MARKER_LIMIT {
                            s.geometry
                                .push(json!({"pid": pid, "ns": stamp, "width": geometry.0,
                                "height": geometry.1, "columns": geometry.2, "rows": geometry.3}));
                        } else {
                            s.overflow = true;
                        }
                    }
                    s.pending.get(&pid).copied()
                });
                let Some(seq) = pending else {
                    return;
                };
                let (_, row) = terminal.cursor_position();
                // Bounded visible rows only; do not scan scrollback or store captured text.
                let start = row.saturating_sub(terminal.row_count().min(64));
                let text = terminal
                    .text_range_format(vte4::Format::Text, start, 0, row + 1, 0)
                    .0;
                let marker = format!("A04K{seq:06}");
                if text.as_ref().is_some_and(|text| text.contains(&marker)) {
                    STATE.with(|cell| {
                        let mut s = cell.borrow_mut();
                        s.pending.remove(&pid);
                        s.seen.insert(pid, seq);
                        if s.painted.len() < MARKER_LIMIT {
                            s.painted.push((pid, seq, stamp));
                        } else {
                            s.overflow = true;
                        }
                    });
                }
            });
            surface.signal = Some((clock, signal));
        }
    });
}

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Snapshot {
        request: u32,
    },
    Split {
        request: u32,
        pane_index: usize,
        horizontal: bool,
    },
    NewTab {
        request: u32,
    },
    SelectTab {
        request: u32,
        tab_id: u64,
    },
    Active {
        request: u32,
    },
    Idle {
        request: u32,
    },
    Finish {
        request: u32,
    },
}
impl Command {
    fn request(&self) -> u32 {
        match *self {
            Self::Snapshot { request }
            | Self::Split { request, .. }
            | Self::NewTab { request }
            | Self::SelectTab { request, .. }
            | Self::Active { request }
            | Self::Idle { request }
            | Self::Finish { request } => request,
        }
    }
}

/// Only installed in the explicitly patched benchmark binary, in a private XDG tree.
pub(crate) fn install(sender: &ComponentSender<crate::AppModel>) {
    let path = std::env::var_os("ANVIL_INTERACTION_BUDGET_DIR")
        .map(PathBuf::from)
        .expect("instrumented binary requires its private benchmark directory");
    let metadata = std::fs::symlink_metadata(&path).expect("benchmark directory missing");
    // SAFETY: geteuid takes no arguments and has no memory effects.
    let uid = unsafe { libc::geteuid() };
    assert!(
        path.is_absolute()
            && metadata.is_dir()
            && metadata.uid() == uid
            && metadata.mode() & 0o077 == 0,
        "benchmark directory must be absolute, owned and mode 0700"
    );
    STATE.with(|s| s.borrow_mut().directory = Some(path));
    let sender = Box::into_raw(Box::new(sender.clone())).cast::<std::ffi::c_void>();
    // SAFETY: GLib calls this source on the owning main context and owns sender
    // until the matching destroy notifier. This does not install a Rust signal handler.
    unsafe {
        g_unix_signal_add_full(0, libc::SIGUSR1, signal_ready, sender, signal_destroy);
    }
    let directory = STATE
        .with(|s| s.borrow().directory.clone())
        .expect("probe installed");
    let mut ready = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join("ready.json"))
        .expect("create benchmark readiness");
    serde_json::to_writer(&mut ready, &json!({"pid": std::process::id()}))
        .expect("write readiness");
}

pub(crate) fn handle(model: &mut crate::AppModel, sender: &ComponentSender<crate::AppModel>) {
    let directory = STATE
        .with(|s| s.borrow().directory.clone())
        .expect("probe installed");
    let input = directory.join("command.json");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(input)
        .expect("owned benchmark command missing");
    let mut bytes = Vec::new();
    use std::io::Read;
    file.by_ref()
        .take(4097)
        .read_to_end(&mut bytes)
        .expect("read benchmark command");
    assert!(bytes.len() <= 4096, "benchmark command too large");
    let command: Command = serde_json::from_slice(&bytes).expect("invalid benchmark command");
    if matches!(&command, Command::Split { .. } | Command::NewTab { .. }) {
        assert!(
            STATE.with(|s| s.borrow().phase == Phase::Setup),
            "pane setup is frozen during measurement"
        );
    }
    let request = command.request();
    let include_samples = matches!(&command, Command::Finish { .. });
    let fresh = STATE.with(|s| {
        let mut s = s.borrow_mut();
        if request <= s.request {
            false
        } else {
            s.request = request;
            true
        }
    });
    if !fresh {
        return;
    }
    match command {
        Command::Snapshot { .. } => {}
        Command::Split {
            pane_index,
            horizontal,
            ..
        } => {
            let tab = model
                .tabs
                .get(model.active)
                .expect("benchmark active tab missing");
            assert!(pane_index < tab.panes.len(), "benchmark pane index invalid");
            let delta = pane_index as i32 - tab.active_pane as i32;
            model.cycle_pane_focus(delta);
            model.split_active(
                if horizontal {
                    gtk::Orientation::Horizontal
                } else {
                    gtk::Orientation::Vertical
                },
                sender,
            );
        }
        Command::NewTab { .. } => model.add_tab(None, sender),
        Command::SelectTab { tab_id, .. } => model.select_tab(tab_id, sender),
        Command::Active { .. } => {
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                assert!(s.phase == Phase::Setup);
                s.phase = Phase::Active;
            });
            set_frame_observers(true);
        }
        Command::Idle { .. } => {
            set_frame_observers(false);
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                assert!(s.phase == Phase::Active);
                s.phase = Phase::Idle;
            });
        }
        Command::Finish { .. } => {
            set_frame_observers(false);
            STATE.with(|s| s.borrow_mut().phase = Phase::Finished);
        }
    }
    let panes: Vec<_> = model
        .tabs
        .iter()
        .map(|tab| {
            json!({"tab_id": tab.id,
        "panes": tab.panes.iter().map(|pane| {
            let widget = pane.widget();
            let rect = widget.compute_bounds(&model.window);
            let grid = pane.terminal.grid_size();
            json!({"pane_id": pane.id, "pid": pane.probe.shell_pid.get(),
                "block": pane.mode == crate::config::TerminalMode::Block, "mapped": widget.is_mapped(),
                "columns": grid.0, "rows": grid.1,
                "bounds": rect.map(|r| [r.x(), r.y(), r.width(), r.height()])})
        }).collect::<Vec<_>>()})
        })
        .collect();
    let report = STATE.with(|cell| {
        let s = cell.borrow();
        json!({"request": request, "monotonic_ns": now_ns(), "tabs": panes,
            "window_size": [model.window.width(), model.window.height()], "active_tab_index": model.active, "session_persistence": model.session_persistence,
            "dividers": model.tabs.get(model.active).map(|tab| dividers(&tab.holder.clone().upcast(), &model.window.clone().upcast())),
            "dispatch_ids": s.dispatch.iter().take(if include_samples { LIMIT } else { 0 }).map(|v| v.0).collect::<Vec<_>>(),
            "dispatch_ms": s.dispatch.iter().take(if include_samples { LIMIT } else { 0 }).map(|v| v.1 as f64 / 1_000_000.0).collect::<Vec<_>>(),
            "consumer_dispatch_ids": s.consumer.iter().take(if include_samples { LIMIT } else { 0 }).map(|v| v.0).collect::<Vec<_>>(),
            "consumer_ms": s.consumer.iter().take(if include_samples { LIMIT } else { 0 }).map(|v| v.1 as f64 / 1_000_000.0).collect::<Vec<_>>(),
            "consumed_bytes": s.consumed_bytes, "idle_dispatches": s.idle_dispatches,
            "idle_pty_empty_polls": s.idle_empty, "idle_consumers": s.idle_consumer,
            "idle_app_updates": s.idle_app_updates, "instrumentation_idle_callbacks": s.idle_observer_callbacks,
            "vte_spawn_count": s.vte_spawns, "timer_fallback": s.fallback,
            "overflow": s.overflow, "painted_markers": if include_samples { json!(s.painted) } else { Value::Null },
            "geometry_events": if include_samples { json!(s.geometry) } else { Value::Null },
            "metric": "after_paint_proxy_not_display"})
    });
    let temporary = directory.join("response.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .expect("create benchmark response");
    serde_json::to_writer(&mut file, &report).expect("write benchmark response");
    drop(file);
    std::fs::rename(temporary, directory.join("response.json"))
        .expect("publish benchmark response");
}

unsafe extern "C" {
    fn g_unix_signal_add_full(
        priority: i32,
        signum: i32,
        callback: extern "C" fn(*mut std::ffi::c_void) -> i32,
        data: *mut std::ffi::c_void,
        destroy: extern "C" fn(*mut std::ffi::c_void),
    ) -> u32;
}
extern "C" fn signal_ready(data: *mut std::ffi::c_void) -> i32 {
    // SAFETY: install allocated this exact sender; GLib retains it through this callback.
    let sender = unsafe { &*data.cast::<ComponentSender<crate::AppModel>>() };
    sender.input(crate::app_msg::AppMsg::InteractionBudgetSignal);
    1
}
extern "C" fn signal_destroy(data: *mut std::ffi::c_void) {
    // SAFETY: GLib destroys the source once, releasing its uniquely owned sender.
    unsafe {
        drop(Box::from_raw(
            data.cast::<ComponentSender<crate::AppModel>>(),
        ));
    }
}

fn dividers(root: &gtk::Widget, window: &gtk::Widget) -> Vec<Value> {
    fn visit(widget: &gtk::Widget, window: &gtk::Widget, output: &mut Vec<Value>) {
        if let Some(paned) = widget.downcast_ref::<gtk::Paned>() {
            if let (Some(start), Some(end)) = (paned.start_child(), paned.end_child()) {
                if let (Some(a), Some(b)) =
                    (start.compute_bounds(window), end.compute_bounds(window))
                {
                    let horizontal = paned.orientation() == gtk::Orientation::Horizontal;
                    let (x, y) = if horizontal {
                        ((a.x() + a.width() + b.x()) / 2.0, a.y() + a.height() / 2.0)
                    } else {
                        (a.x() + a.width() / 2.0, (a.y() + a.height() + b.y()) / 2.0)
                    };
                    output.push(json!({"x": x, "y": y, "horizontal": horizontal, "position": paned.position()}));
                }
            }
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            visit(&current, window, output);
            child = current.next_sibling();
        }
    }
    let mut result = Vec::new();
    visit(root, window, &mut result);
    result
}
