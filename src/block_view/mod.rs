use gtk::gdk::RGBA;
use gtk::pango::FontDescription;
use gtk::prelude::*;
use gtk::{glib, Orientation, ScrolledWindow};
use relm4::gtk;
use std::cell::{Cell, OnceCell, Ref, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime};
use vte4::Terminal;
use vte4::TerminalExt;

use crate::config::Config;
use crate::parser::{
    ColorKind, CommandMeta, KeyboardProtocolQuery, Parser, ParserConfig, ParserEvent,
};
use crate::pty::OwnedPty;
use crate::pty_input;
use crate::terminal::kitty_graphics;

mod alt_screen;
mod ansi;
mod blocks;
mod cross_selection;
mod css;
mod export;
mod find;
mod history;
#[allow(dead_code)]
mod palette;
mod scroll;
mod selection_hold;
mod unified_chrome;
mod zone_history;
pub(crate) use alt_screen::*;
pub(crate) use ansi::*;
pub(crate) use blocks::*;
pub(crate) use cross_selection::*;
pub(crate) use css::*;
pub(crate) use export::SessionExportFormat;
pub(crate) use find::*;
#[allow(unused_imports)]
pub(crate) use palette::*;
pub(crate) use scroll::*;
use selection_hold::SelectionFeedHold;

// ── perf profiling (env JTERM_PROF=1) ───────────────────────────────────────
pub(crate) fn prof_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("JTERM_PROF").is_ok())
}

// A random per-process 32-bit namespace prevents concurrently running Anvil
// processes from emitting the same persisted ids. The low 32 bits remain a
// checked monotonic sequence (over four billion completed blocks per process).
static BLOCK_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
static BLOCK_ID_NAMESPACE: OnceLock<u64> = OnceLock::new();
const BLOCK_ID_SEQUENCE_LIMIT: u64 = 1_u64 << 32;

fn process_block_id_namespace() -> u64 {
    *BLOCK_ID_NAMESPACE.get_or_init(|| {
        let mut random = [0_u8; std::mem::size_of::<u32>()];
        // SAFETY: `random` is a writable buffer of the exact supplied length;
        // nonblocking entropy failure falls through to a time/pid mix.
        let read = unsafe {
            nix::libc::getrandom(
                random.as_mut_ptr().cast(),
                random.len(),
                nix::libc::GRND_NONBLOCK,
            )
        };
        let namespace = if read == random.len() as isize {
            u32::from_ne_bytes(random)
        } else {
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let mut mixed = nanos
                ^ u64::from(std::process::id())
                ^ (&BLOCK_ID_COUNTER as *const AtomicU64 as usize as u64);
            // SplitMix64 finalizer: diffuse the fallback's time, pid, and ASLR
            // inputs before selecting the namespace bits.
            mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (mixed ^ (mixed >> 31)) as u32
        };
        u64::from(namespace.max(1)) << 32
    })
}

fn claim_next_unused_block_id(
    reserved: &mut HashSet<u64>,
    mut next_candidate: impl FnMut() -> u64,
) -> u64 {
    loop {
        let candidate = next_candidate();
        if !reserved.remove(&candidate) {
            return candidate;
        }
    }
}

/// Why a review-gated command can or cannot be written to the live Block
/// prompt. Agent controls use this richer status to explain the exact recovery
/// step without weakening the clean, idle-prompt boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandPromptStatus {
    Ready,
    HasInput,
    Running,
    Fullscreen,
    Initializing,
    ShellIntegrationUnavailable,
}

impl CommandPromptStatus {
    pub(crate) fn is_ready(self) -> bool {
        self == Self::Ready
    }

    pub(crate) fn short_label(self) -> &'static str {
        match self {
            Self::Ready => "Prompt ready",
            Self::HasInput => "Prompt has input",
            Self::Running => "Command running",
            Self::Fullscreen => "Full-screen app active",
            Self::Initializing => "Prompt initializing",
            Self::ShellIntegrationUnavailable => "Shell integration required",
        }
    }

    pub(crate) fn blocked_message(self) -> &'static str {
        match self {
            Self::Ready => "The pinned Block prompt is ready.",
            Self::HasInput => {
                "The pinned shell prompt already contains input. Clear it and press Enter to reach a fresh prompt, then try again."
            }
            Self::Running => {
                "A command is still running in the pinned Block pane. Wait for it to finish and for a fresh prompt, then try again."
            }
            Self::Fullscreen => {
                "A full-screen terminal application owns the pinned pane. Exit it before inserting or approving a command."
            }
            Self::Initializing => {
                "The pinned Block prompt is still initializing. Wait for the shell prompt, then try again."
            }
            Self::ShellIntegrationUnavailable => {
                "Automatic execution requires a direct local bash/zsh pane with the bundled token-aware shell integration. Remote and Flatpak host-bridge panes remain insert-only."
            }
        }
    }
}

fn next_block_id(reserved: &RefCell<HashSet<u64>>) -> u64 {
    let mut reserved = reserved.borrow_mut();
    claim_next_unused_block_id(&mut reserved, || {
        let sequence = BLOCK_ID_COUNTER
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current
                    .checked_add(1)
                    .filter(|next| *next <= BLOCK_ID_SEQUENCE_LIMIT)
            })
            .unwrap_or_else(|_| panic!("completed-block id sequence exhausted"));
        process_block_id_namespace() | sequence
    })
}

const ZONE_MARKER_CLOSE: &[u8] = b"\x1b]8;;\x1b\\";

/// Per-pane OSC 8 marker framing for Unified's persistent VTE.
///
/// The nonce comes from Linux `getrandom(2)` only. If strong randomness is
/// unavailable, marker injection stays disabled: a predictable fallback would
/// let guest output forge this pane's record boundaries and is worse than
/// temporarily exposing no record-specific marker at all.
///
/// RIS/reset invalidation deliberately remains outside this increment. It has
/// to evict marker authority and retained record ranges together.
#[derive(Debug)]
struct ZoneMarkerInjector {
    nonce: Option<[u8; 16]>,
    open: Option<OpenZoneMarker>,
}

#[derive(Debug)]
struct OpenZoneMarker {
    id: u64,
    bytes: Rc<[u8]>,
}

impl ZoneMarkerInjector {
    fn from_system_entropy() -> Self {
        let nonce = secure_zone_marker_nonce();
        if nonce.is_none() {
            log::warn!("unified zone markers disabled: strong per-pane randomness is unavailable");
        }
        Self { nonce, open: None }
    }

    #[cfg(test)]
    fn with_nonce(nonce: [u8; 16]) -> Self {
        Self {
            nonce: Some(nonce),
            open: None,
        }
    }

    #[cfg(test)]
    fn disabled() -> Self {
        Self {
            nonce: None,
            open: None,
        }
    }

    fn begin_zone(&mut self, zone_id: u64) {
        if self.open.as_ref().is_some_and(|open| open.id == zone_id) {
            return;
        }
        let Some(nonce) = self.nonce else {
            self.open = None;
            return;
        };
        let mut nonce_hex = String::with_capacity(32);
        for byte in nonce {
            use std::fmt::Write as _;
            let _ = write!(nonce_hex, "{byte:02x}");
        }
        self.open = Some(OpenZoneMarker {
            id: zone_id,
            bytes: format!("\x1b]8;;block://{nonce_hex}/{zone_id}\x1b\\")
                .into_bytes()
                .into(),
        });
    }

    fn close_zone(&mut self, zone_id: Option<u64>) {
        match (self.open.take(), zone_id) {
            (Some(open), Some(zone_id)) if open.id == zone_id => {}
            (Some(_), None) => {}
            (Some(open), Some(zone_id)) => {
                log::debug!(
                    "unified zone marker close mismatch: requested={zone_id} open={}",
                    open.id
                );
            }
            (None, _) => {}
        }
    }

    fn open_bytes(&self) -> Option<Rc<[u8]>> {
        self.open.as_ref().map(|open| open.bytes.clone())
    }

    fn nonce(&self) -> Option<[u8; 16]> {
        self.nonce
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingZone {
    Prompt(u64),
    Command(u64),
}

impl PendingZone {
    fn id(self) -> u64 {
        match self {
            Self::Prompt(id) | Self::Command(id) => id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PromptZonePlan {
    completed_record_id: Option<u64>,
    prompt_id: u64,
}

/// Select the id owned by an accepted OSC 133 `A`. Repeated idle-prompt
/// redraws reuse their id; a completed foreground/background record consumes
/// the prior id and the following prompt receives a fresh global id.
fn plan_prompt_zone(
    pending: Option<PendingZone>,
    completes_record: bool,
    mut next_id: impl FnMut() -> u64,
) -> PromptZonePlan {
    if completes_record {
        PromptZonePlan {
            completed_record_id: Some(pending.map(PendingZone::id).unwrap_or_else(&mut next_id)),
            prompt_id: next_id(),
        }
    } else {
        PromptZonePlan {
            completed_record_id: None,
            prompt_id: pending.map(PendingZone::id).unwrap_or_else(next_id),
        }
    }
}

fn prompt_zone_to_reopen_after_alt(
    restored_state: BlockState,
    pending: Option<PendingZone>,
) -> Option<u64> {
    match (restored_state, pending) {
        (BlockState::AwaitingCommand, Some(PendingZone::Prompt(id))) => Some(id),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn secure_zone_marker_nonce() -> Option<[u8; 16]> {
    let mut nonce = [0_u8; 16];
    let mut filled = 0;
    while filled < nonce.len() {
        // SAFETY: the suffix beginning at `filled` is writable for exactly
        // the length passed to the kernel and remains alive for the call.
        let read = unsafe {
            nix::libc::getrandom(
                nonce[filled..].as_mut_ptr().cast(),
                nonce.len() - filled,
                nix::libc::GRND_NONBLOCK,
            )
        };
        if read > 0 {
            filled += read as usize;
            continue;
        }
        if read < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return None;
    }
    Some(nonce)
}

#[cfg(not(target_os = "linux"))]
fn secure_zone_marker_nonce() -> Option<[u8; 16]> {
    None
}

/// Approximate the vertical positions of failed finished blocks within the
/// complete scrollback history. The bounded tail caps the number of Cairo marks
/// for very long sessions while preserving the newest failures, which are
/// usually the most useful navigation hints. Computing positions still scans
/// the retained block metadata once.
fn failed_block_marker_fractions_from_entries(
    entries: impl IntoIterator<Item = (u64, bool)>,
) -> Vec<f64> {
    const MAX_FAILURE_MARKERS: usize = 1024;

    let mut top = 0_u64;
    let mut markers = VecDeque::new();
    for (height, failed) in entries {
        if failed {
            if markers.len() == MAX_FAILURE_MARKERS {
                markers.pop_front();
            }
            markers.push_back(top);
        }
        top = top.saturating_add(height.max(1));
    }
    if top == 0 {
        return Vec::new();
    }

    markers
        .into_iter()
        .map(|marker_top| (marker_top as f64 / top as f64).clamp(0.0, 1.0))
        .collect()
}

fn failed_block_marker_fractions(blocks: &VecDeque<BlockData>) -> Vec<f64> {
    failed_block_marker_fractions_from_entries(blocks.iter().map(|block| {
        (
            block.estimated_height.max(1) as u64,
            jterm_core::block_contract::classify_completed(Some(&block.cmd), block.exit_code)
                .is_failed(),
        )
    }))
}

#[cfg(test)]
fn failed_block_marker_fractions_legacy(blocks: &VecDeque<BlockData>) -> Vec<f64> {
    const MAX_FAILURE_MARKERS: usize = 1024;

    let total_height = blocks.iter().fold(0_u64, |total, block| {
        total.saturating_add(block.estimated_height.max(1) as u64)
    });
    if total_height == 0 {
        return Vec::new();
    }

    let mut top = 0_u64;
    let mut markers = VecDeque::new();
    for block in blocks {
        if jterm_core::block_contract::classify_completed(Some(&block.cmd), block.exit_code)
            .is_failed()
        {
            if markers.len() == MAX_FAILURE_MARKERS {
                markers.pop_front();
            }
            markers.push_back((top as f64 / total_height as f64).clamp(0.0, 1.0));
        }
        top = top.saturating_add(block.estimated_height.max(1) as u64);
    }

    markers.into()
}

type FailureMarkerRedraw = Rc<dyn Fn()>;

/// Mutate the history metadata and schedule its marker overlay only after the
/// mutable borrow has been released. The draw callback reads the same RefCell,
/// so keeping this ordering explicit also makes future synchronous redraw
/// implementations safe.
fn mutate_block_data_and_redraw<R>(
    block_data: &RefCell<VecDeque<BlockData>>,
    redraw: &dyn Fn(),
    mutate: impl FnOnce(&mut VecDeque<BlockData>) -> R,
) -> R {
    let result = {
        let mut block_data = block_data.borrow_mut();
        mutate(&mut block_data)
    };
    redraw();
    result
}

/// Give an icon-only button a stable tooltip and an explicit accessible name.
/// GTK symbolic icons come from the active icon theme, so these controls do not
/// depend on glyphs supplied only by patched terminal fonts.
fn set_icon_button(button: &gtk::Button, icon_name: &str, label: &str) {
    button.set_icon_name(icon_name);
    button.set_tooltip_text(Some(label));
    button.update_property(&[gtk::accessible::Property::Label(label)]);
}

fn icon_button(icon_name: &str, label: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon_name);
    set_icon_button(&button, icon_name, label);
    button
}

/// Update the jump-to-bottom FAB to show an unread-block badge: just the
/// symbolic icon when nothing is pending, icon + count (clamped to "99+")
/// otherwise.
fn set_jump_fab_label(fab: &gtk::Button, unread: u32) {
    let content = gtk::Box::new(Orientation::Horizontal, 6);
    content.append(&gtk::Image::from_icon_name("go-bottom-symbolic"));
    if unread > 0 {
        let n = if unread > 99 {
            "99+".to_string()
        } else {
            unread.to_string()
        };
        content.append(&gtk::Label::new(Some(&n)));
        let accessible = format!("Jump to latest, {n} unread blocks");
        fab.update_property(&[gtk::accessible::Property::Label(&accessible)]);
    } else {
        fab.update_property(&[gtk::accessible::Property::Label("Jump to latest")]);
    }
    fab.set_child(Some(&content));
}

/// Unread blocks are the newest suffix of the retained finished list. Prefix
/// retention must subtract only the part of that suffix it actually evicted.
fn unread_after_prefix_eviction(total_before: usize, unread: u32, evicted: usize) -> u32 {
    let unread = (unread as usize).min(total_before);
    let read_prefix = total_before.saturating_sub(unread);
    let evicted_unread = evicted.min(total_before).saturating_sub(read_prefix);
    u32::try_from(unread.saturating_sub(evicted_unread)).unwrap_or(u32::MAX)
}

fn unread_after_index_removal(total_before: usize, unread: u32, removed: usize) -> u32 {
    let unread = (unread as usize).min(total_before);
    let unread_start = total_before.saturating_sub(unread);
    let remaining = if removed < total_before && removed >= unread_start {
        unread.saturating_sub(1)
    } else {
        unread
    };
    u32::try_from(remaining).unwrap_or(u32::MAX)
}

/// Whether a key press seen while keyboard focus is stranded on a finished
/// block should hand focus back to the live prompt.
///
/// Only typing-shaped keys recover focus. Ctrl/Alt/Super chords stay on their
/// normal dispatch paths (window shortcuts run in an earlier Capture stage and
/// never get here, but unbound chords like the Ctrl+C interrupt fallback still
/// pass through this controller), Tab keeps GTK focus navigation, and
/// reading/navigation keys keep whatever scroll or selection meaning they have
/// — block find deliberately lands focus on the picked block so the user can
/// keep reading there.
fn stranded_focus_key_recovers(keyval: gtk::gdk::Key, modifiers: gtk::gdk::ModifierType) -> bool {
    use gtk::gdk::Key;

    if modifiers.intersects(
        gtk::gdk::ModifierType::CONTROL_MASK
            | gtk::gdk::ModifierType::ALT_MASK
            | gtk::gdk::ModifierType::SUPER_MASK
            | gtk::gdk::ModifierType::META_MASK,
    ) {
        return false;
    }

    !matches!(
        keyval,
        // Modifier presses themselves.
        Key::Shift_L
            | Key::Shift_R
            | Key::Control_L
            | Key::Control_R
            | Key::Alt_L
            | Key::Alt_R
            | Key::Meta_L
            | Key::Meta_R
            | Key::Super_L
            | Key::Super_R
            | Key::Hyper_L
            | Key::Hyper_R
            | Key::Caps_Lock
            | Key::Num_Lock
            | Key::Scroll_Lock
            | Key::ISO_Level3_Shift
            | Key::ISO_Level5_Shift
            | Key::Mode_switch
            // Focus navigation.
            | Key::Tab
            | Key::ISO_Left_Tab
            // Reading/navigation keys.
            | Key::Up
            | Key::Down
            | Key::Left
            | Key::Right
            | Key::KP_Up
            | Key::KP_Down
            | Key::KP_Left
            | Key::KP_Right
            | Key::Page_Up
            | Key::Page_Down
            | Key::KP_Page_Up
            | Key::KP_Page_Down
            | Key::Home
            | Key::End
            | Key::KP_Home
            | Key::KP_End
            | Key::Menu
    )
}

/// Focused widgets that own their keystrokes even though they live inside the
/// block pane: text entries (the per-block output filter row, search entries),
/// popover contents (context menus), and buttons for their activation keys.
fn focused_widget_keeps_key(focused: &gtk::Widget, keyval: gtk::gdk::Key) -> bool {
    use gtk::gdk::Key;

    if focused.is::<gtk::Editable>() || focused.is::<gtk::TextView>() {
        return true;
    }
    if focused.ancestor(gtk::Popover::static_type()).is_some() {
        return true;
    }
    if matches!(
        keyval,
        Key::Return | Key::KP_Enter | Key::ISO_Enter | Key::space
    ) && (focused.is::<gtk::Button>() || focused.is::<gtk::CheckButton>())
    {
        return true;
    }
    false
}

fn sample_output_for_event(output: &str) -> String {
    const MAX_CHARS: usize = 32 * 1024;
    if output.len() <= MAX_CHARS {
        return output.to_string();
    }
    let half = MAX_CHARS / 2;
    let head_end = output
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= half)
        .last()
        .unwrap_or(0);
    let tail_start = output
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i >= output.len().saturating_sub(half))
        .unwrap_or(output.len());
    format!(
        "{}\n... [{} bytes elided] ...\n{}",
        &output[..head_end],
        tail_start.saturating_sub(head_end),
        &output[tail_start..]
    )
}

/// Normalize cells captured after the trusted PromptEnd anchor.
///
/// Never strip a text prefix merely because it resembles the visible prompt:
/// `$HOME` under a `$` prompt and `git status` under a `git` prompt are valid
/// commands. Text equality cannot distinguish prompt furniture from input.
fn normalize_captured_command(captured: &str, _prompt: &str) -> String {
    captured.trim().to_string()
}

fn command_id_uses_shell_token(id: &str, token: &str) -> bool {
    !token.is_empty()
        && id
            .strip_prefix(token)
            .and_then(|suffix| suffix.strip_prefix('-'))
            .is_some_and(|sequence| {
                !sequence.is_empty() && sequence.bytes().all(|byte| byte.is_ascii_digit())
            })
}

fn shell_argv_uses_jsh(argv: &[String]) -> bool {
    argv.iter().any(|argument| {
        argument.split_ascii_whitespace().any(|word| {
            let word = word
                .trim_matches(|ch: char| matches!(ch, '\'' | '"' | ';' | '(' | ')' | '[' | ']'));
            std::path::Path::new(word)
                .file_name()
                .and_then(|name| name.to_str())
                == Some("jsh")
        })
    })
}

/// Only direct, interactive bundled bash/zsh startup can consume the private
/// descriptor before user code. Wrappers, remote commands and `-c` execution
/// cannot provide the same inherited-FD and prompt-lifecycle guarantees.
fn shell_argv_supports_agent_ids(argv: &[String]) -> bool {
    let direct_shell = argv
        .first()
        .and_then(|argument| std::path::Path::new(argument).file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "bash" | "zsh"));
    let runs_one_command = argv.iter().skip(1).any(|argument| {
        argument == "--command"
            || (argument.starts_with('-')
                && !argument.starts_with("--")
                && argument[1..].bytes().any(|byte| byte == b'c'))
    });
    direct_shell && !runs_one_command && !shell_argv_uses_jsh(argv)
}

const MAX_CAPABILITY_OSC_BYTES: usize = 128;
const MAX_LOCAL_APC_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
struct ShellCapabilityObserver {
    state: CapabilityOscState,
    collecting_prompt: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum CapabilityOscState {
    #[default]
    Ground,
    Escape,
    Osc(Vec<u8>),
    OscEscape(Vec<u8>),
    Discard,
    DiscardEscape,
}

impl ShellCapabilityObserver {
    /// Observe the raw stream without replacing the shared terminal parser.
    /// Core now recognizes OSC 7771 itself (`AgentIntegrationReady`, never
    /// forwarded to the VTE); this observer remains the trust authority
    /// because it advances in reset-splitter order, so a same-chunk RIS
    /// invalidates pre-reset trust before the suffix bytes are observed.
    fn feed(&mut self, bytes: &[u8], expected: &str, ready: &Cell<bool>) {
        let mut index = 0usize;
        while index < bytes.len() {
            // ESC is the only byte that leaves Ground. Bulk-skip ordinary output
            // instead of taking/replacing the enum once per byte; long compiler
            // logs otherwise make this trust observer more expensive than the
            // parser and raw capture it accompanies.
            if matches!(&self.state, CapabilityOscState::Ground) {
                match memchr::memchr(0x1b, &bytes[index..]) {
                    Some(offset) => {
                        index += offset + 1;
                        self.state = CapabilityOscState::Escape;
                        continue;
                    }
                    None => break,
                }
            }

            let byte = bytes[index];
            index += 1;
            let state = std::mem::take(&mut self.state);
            self.state = match state {
                CapabilityOscState::Ground => unreachable!("Ground is handled by the fast path"),
                CapabilityOscState::Escape => match byte {
                    b']' => CapabilityOscState::Osc(Vec::new()),
                    0x1b => CapabilityOscState::Escape,
                    _ => CapabilityOscState::Ground,
                },
                CapabilityOscState::Osc(mut payload) => match byte {
                    0x07 => {
                        self.finish_osc(&payload, expected, ready);
                        CapabilityOscState::Ground
                    }
                    0x1b => CapabilityOscState::OscEscape(payload),
                    _ if payload.len() < MAX_CAPABILITY_OSC_BYTES => {
                        payload.push(byte);
                        CapabilityOscState::Osc(payload)
                    }
                    _ => CapabilityOscState::Discard,
                },
                CapabilityOscState::OscEscape(payload) => match byte {
                    b'\\' => {
                        self.finish_osc(&payload, expected, ready);
                        CapabilityOscState::Ground
                    }
                    // Deliberately more lenient than the core parser this
                    // observer shadows: core aborts the string on ESC +
                    // non-ST and drops the payload, while a repeated ESC
                    // here keeps the payload alive so
                    // `\x1b]7771;<tok>\x1b\x1b\\` still finishes. Harmless:
                    // core's AgentIntegrationReady arm is ignored (this
                    // observer is the trust authority), and the token is a
                    // secret only the real shell can complete, so the extra
                    // leniency cannot be exploited to forge readiness.
                    0x1b => CapabilityOscState::OscEscape(payload),
                    _ => CapabilityOscState::Discard,
                },
                CapabilityOscState::Discard => {
                    if byte == 0x1b {
                        CapabilityOscState::DiscardEscape
                    } else if byte == 0x07 {
                        CapabilityOscState::Ground
                    } else {
                        CapabilityOscState::Discard
                    }
                }
                CapabilityOscState::DiscardEscape => match byte {
                    b'\\' => CapabilityOscState::Ground,
                    0x1b => CapabilityOscState::DiscardEscape,
                    _ => CapabilityOscState::Discard,
                },
            };
        }
    }

    fn finish_osc(&mut self, payload: &[u8], expected: &str, ready: &Cell<bool>) {
        if payload.starts_with(b"133;A") && payload.get(5).is_none_or(|byte| *byte == b';') {
            self.collecting_prompt = true;
            ready.set(false);
            return;
        }
        if payload.starts_with(b"133;B") && payload.get(5).is_none_or(|byte| *byte == b';') {
            self.collecting_prompt = false;
            return;
        }
        let Some(announced) = payload.strip_prefix(b"7771;") else {
            return;
        };
        if self.collecting_prompt
            && expected.len() == 32
            && expected.bytes().all(|byte| byte.is_ascii_hexdigit())
            && announced == expected.as_bytes()
        {
            ready.set(true);
        }
    }
}

fn reviewed_submission_matches(
    shell_command: Option<&str>,
    rendered_command: &str,
    approved_command: &str,
    identity_feed_tainted: bool,
) -> bool {
    if identity_feed_tainted {
        return false;
    }
    if let Some(shell_command) = shell_command {
        return shell_command == approved_command;
    }
    rendered_command == approved_command
        || rendered_command.strip_suffix('\n') == Some(approved_command)
}

/// Only cell-neutral presentation bytes are allowed between the separately
/// queued Enter and the shell's CommandStart mark.
fn reviewed_pre_command_bytes_are_identity_neutral(mut bytes: &[u8]) -> bool {
    while let Some((&byte, rest)) = bytes.split_first() {
        if matches!(byte, b'\r' | b'\n') {
            bytes = rest;
            continue;
        }
        if !bytes.starts_with(b"\x1b[") {
            return false;
        }
        let Some(final_offset) = bytes[2..]
            .iter()
            .position(|byte| (0x40..=0x7e).contains(byte))
        else {
            return false;
        };
        let final_index = final_offset + 2;
        let params = &bytes[2..final_index];
        let final_byte = bytes[final_index];
        let sgr = final_byte == b'm';
        let bracketed_paste_mode = params == b"?2004" && matches!(final_byte, b'h' | b'l');
        if !sgr && !bracketed_paste_mode {
            return false;
        }
        bytes = &bytes[final_index + 1..];
    }
    true
}

/// Stand-in command text for a block whose shell reported that it *had* a
/// command line and dropped it for size, when the screen scrape came back empty
/// too. Recorded as the block's command so its output is still filed as a
/// command block instead of as commandless background output.
pub(crate) const TRUNCATED_COMMAND_PLACEHOLDER: &str = "[command too long to report]";
const UNAVAILABLE_COMMAND_PLACEHOLDER: &str = "(command capture unavailable)";

/// Where a finished block's command text came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandTextSource {
    /// The shell attached the command line to its OSC 133 `C` packet (jsh does).
    ShellReported,
    /// The shell had a command line but exceeded the packet budget, so only the
    /// screen scrape is left. Distinct from [`Self::Screen`]: a command really
    /// did run, so an empty capture must not be read as "no command".
    ScreenAfterTruncation,
    /// The shell emits bare marks, so the screen is the only source there is.
    Screen,
}

/// Resolve the command line a finished block records.
///
/// The shell's own copy wins over the screen scrape, which can only ever show
/// what the renderer painted: a wrapped line, jsh's autosuggestion ghost text,
/// or a right-hand prompt all land inside the captured range, and a `feed()`
/// that has not been flushed yet leaves it empty.
fn resolve_command_text(
    reported: Option<&str>,
    command_truncated: bool,
    scraped: &str,
) -> (String, CommandTextSource) {
    if let Some(command) = reported {
        return (command.to_string(), CommandTextSource::ShellReported);
    }
    if command_truncated {
        let text = if scraped.trim().is_empty() {
            TRUNCATED_COMMAND_PLACEHOLDER.to_string()
        } else {
            scraped.to_string()
        };
        return (text, CommandTextSource::ScreenAfterTruncation);
    }
    (scraped.to_string(), CommandTextSource::Screen)
}

/// Lower a possibly-unreported status to the single `i32` that shared APIs still
/// require: `jterm_core::command_history`, `jterm_core::notify`, and jagent's
/// block context and observation turns.
///
/// `0` is the least-bad stand-in for all four, because every one of them treats
/// non-zero as "this command failed" — a desktop notification marked critical, a
/// palette entry in the failure filter, an agent told to fix something. Claiming
/// a failure nobody observed is worse than claiming an outcome. The distinction
/// survives wherever this app owns the presentation (the block header, the
/// markdown export, the inactive-tab styling); widening those APIs to
/// `Option<i32>` is the follow-up this function exists to make greppable.
pub(crate) fn exit_code_for_i32_api(exit_code: Option<i32>) -> i32 {
    exit_code.unwrap_or(0)
}

/// Duration recorded for a finished block.
///
/// The shell's own measurement wins. The local fallback timer starts when this
/// process *noticed* the CommandStart mark and stops at the next PromptStart, so
/// it also contains the shell's post-command work plus whatever latency the GTK
/// dispatch that delivered those marks added.
fn block_duration_ms(
    shell_reported: Option<u64>,
    started: Option<SystemTime>,
    ended: SystemTime,
) -> Option<u64> {
    shell_reported.or_else(|| {
        started
            .and_then(|start| ended.duration_since(start).ok())
            .map(|elapsed| elapsed.as_millis() as u64)
    })
}

const MAX_RECALLED_COMMAND_BYTES: usize = 256 * 1024;
const MAX_TYPED_COMMAND_SHADOW_BYTES: usize = MAX_RECALLED_COMMAND_BYTES;
const MAX_PROMPT_CAPTURE_BYTES: usize = 64 * 1024;
const MAX_SELECTED_MARKDOWN_BYTES: usize = 32 * 1024 * 1024;
const MAX_COMMAND_HISTORY_ENTRIES: usize = 2_000;

fn recalled_command_is_safe(command: &str) -> bool {
    command.len() <= MAX_RECALLED_COMMAND_BYTES
        && !crate::review_input::contains_noncontrol_visual_spoofing(command)
}

fn agent_command_is_safe(command: &str) -> bool {
    crate::agent::local_agent_command_issue(command).is_none()
}

/// Append terminal-rendered text while retaining only its newest complete UTF-8
/// suffix. Prompt capture only consumes the last visible line, so keeping the
/// tail is both the useful behavior and a hard memory bound when a forged or
/// broken integration never sends PromptEnd.
fn append_bounded_text_tail(buffer: &mut String, text: &str, max_bytes: usize) {
    if max_bytes == 0 {
        buffer.clear();
        return;
    }

    if text.len() >= max_bytes {
        let mut start = text.len() - max_bytes;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        buffer.clear();
        buffer.push_str(&text[start..]);
        return;
    }

    let overflow = buffer
        .len()
        .checked_add(text.len())
        .map(|length| length.saturating_sub(max_bytes))
        .unwrap_or(buffer.len());
    if overflow != 0 {
        let mut start = overflow.min(buffer.len());
        while !buffer.is_char_boundary(start) {
            start += 1;
        }
        buffer.drain(..start);
    }
    buffer.push_str(text);
}

/// Maintain a fail-closed shadow of the shell editor. Once it exceeds the
/// bound, replace it with an immutable non-empty sentinel until the next
/// prompt. Keeping a partial prefix/tail would let backspaces empty the local
/// shadow while the real readline buffer still contains untracked text, which
/// could incorrectly make the prompt look safe for Agent execution.
fn append_typed_command_shadow(buffer: &mut String, text: &str) {
    if buffer == TRUNCATED_COMMAND_PLACEHOLDER || text.is_empty() {
        return;
    }
    let fits = buffer
        .len()
        .checked_add(text.len())
        .is_some_and(|length| length <= MAX_TYPED_COMMAND_SHADOW_BYTES);
    if fits {
        buffer.push_str(text);
    } else {
        buffer.clear();
        buffer.push_str(TRUNCATED_COMMAND_PLACEHOLDER);
    }
}

fn pop_typed_command_shadow(buffer: &mut String) {
    if buffer != TRUNCATED_COMMAND_PLACEHOLDER {
        buffer.pop();
    }
}

/// anvil's paste policy for text this app puts on the shell's prompt (block
/// recall, the history palette): strip terminal controls even though the text
/// came from local capture, because a spoofed shell-integration stream can also
/// populate that capture. Multiline text is kept only when the shell advertised
/// DECSET 2004 — an unframed newline would submit each line as its own command.
fn prompt_insert_policy() -> pty_input::PastePolicy {
    // anvil currently exact-pins a released jterm_core whose prompt-insert
    // default predates this hardening, so keep the call-site override until the
    // staged jterm_core release can be pinned.
    let mut policy =
        pty_input::PastePolicy::prompt_insert(pty_input::UnbracketedMultiline::FirstLineOnly);
    policy.strip_controls = true;
    policy
}

/// Encode clipboard text for the shell PTY.
///
/// anvil's clipboard policy: de-fang controls (a clipboard is untrusted text,
/// and an escape sequence in it would drive the terminal rather than the shell)
/// and truncate a multiline payload to its first line when the shell has not
/// advertised DECSET 2004. Paste-marker removal is not part of the policy — the
/// encoder always does it, which is the fix for a clipboard that closes the
/// bracketed-paste frame early and has its remainder executed.
fn build_clipboard_paste(text: &str, bracketed_paste: bool) -> pty_input::Paste {
    pty_input::encode_paste(
        text,
        pty_input::PasteModes {
            bracketed: bracketed_paste,
        },
        pty_input::PastePolicy::clipboard(pty_input::UnbracketedMultiline::FirstLineOnly),
    )
}

/// Encode a finished command for insertion at the live prompt.
///
/// The framing, the marker removal and the first-line fallback all live in
/// [`pty_input::encode_prompt_insert`] now; `clear_line_first` is unconditional
/// there because whatever the user has already typed is not represented by any
/// flag this app owns, so a conditional `Ctrl+U` appends instead of replacing.
pub(crate) fn build_command_recall(command: &str, bracketed_paste: bool) -> pty_input::Paste {
    let modes = pty_input::PasteModes {
        bracketed: bracketed_paste,
    };
    if !recalled_command_is_safe(command) {
        // Return the encoder's canonical empty value: in particular, do not
        // emit a bare Ctrl+U that would erase text already at the prompt.
        return pty_input::encode_prompt_insert("", modes, prompt_insert_policy(), true);
    }
    pty_input::encode_prompt_insert(command, modes, prompt_insert_policy(), true)
}

/// Collect commands from a block selection in terminal order. Background
/// blocks are intentionally skipped: they have output but no command to put
/// back into the editor. A newline between commands creates one editable
/// multiline buffer when bracketed paste is available.
fn selected_command_text<'a, I>(blocks: I, selected: &HashSet<u64>) -> String
where
    I: IntoIterator<Item = (u64, &'a str)>,
{
    let mut output = String::new();
    for (id, command) in blocks {
        if !selected.contains(&id) || command.trim().is_empty() {
            continue;
        }
        let separator = usize::from(!output.is_empty());
        let Some(next_len) = output
            .len()
            .checked_add(separator)
            .and_then(|length| length.checked_add(command.len()))
        else {
            return String::new();
        };
        if next_len > MAX_RECALLED_COMMAND_BYTES {
            // Do not return a partial selection: it could be a syntactically
            // different command than the selection the user reviewed.
            return String::new();
        }
        if separator != 0 {
            output.push('\n');
        }
        output.push_str(command);
    }
    output
}

fn append_bounded_section(
    output: &mut String,
    separator: &str,
    part: &str,
    max_bytes: usize,
) -> bool {
    let Some(next_len) = output
        .len()
        .checked_add(separator.len())
        .and_then(|length| length.checked_add(part.len()))
    else {
        return false;
    };
    if next_len > max_bytes {
        return false;
    }
    output.push_str(separator);
    output.push_str(part);
    true
}

/// Markdown for the block-selection right-click copy: every selected block in
/// terminal order, falling back to the right-clicked block when the selection
/// set does not cover it (e.g. no selection registered yet).
fn selected_blocks_markdown<'a, I>(blocks: I, selected: &HashSet<u64>, clicked_id: u64) -> String
where
    I: IntoIterator<Item = &'a BlockData>,
{
    let mut output = String::new();
    let mut selected_count = 0usize;
    let mut clicked_part: Option<String> = None;
    for block in blocks {
        if selected.contains(&block.id) {
            selected_count += 1;
            let part = block.to_markdown();
            let separator = if output.is_empty() { "" } else { "---\n\n" };
            if !append_bounded_section(&mut output, separator, &part, MAX_SELECTED_MARKDOWN_BYTES) {
                // A partial export can misrepresent which commands/results the
                // user selected, so fail the entire aggregation atomically.
                return String::new();
            }
        } else if block.id == clicked_id {
            clicked_part = Some(block.to_markdown());
        }
    }
    if selected_count == 0 {
        return clicked_part
            .filter(|part| part.len() <= MAX_SELECTED_MARKDOWN_BYTES)
            .unwrap_or_default();
    }
    output
}

fn recall_selected_commands_at_prompt(
    pty: &OwnedPty,
    pty_synced: &Cell<bool>,
    typed_cmd: &RefCell<String>,
    state: BlockState,
    finished: &[FinishedBlock],
    selected: &HashSet<u64>,
    bracketed_paste: bool,
) -> bool {
    let command = selected_command_text(
        finished
            .iter()
            .map(|block| (block.id, block.cmd_text.as_str())),
        selected,
    );
    recall_command_at_prompt(
        pty,
        pty_synced,
        typed_cmd,
        state,
        false,
        &command,
        bracketed_paste,
    )
}

/// Mirror text this app wrote to the shell into the live editor shadow.
///
/// anvil reconstructs the typed line from the live VTE's `commit` signal, which
/// a clipboard paste never travels through: block mode writes to its own PTY
/// directly. Without this the shadow misses everything the user pasted, so the
/// idle input cell keeps the height of a line the shell no longer has and the
/// background-output heuristic keeps treating the shell's echo of the paste as
/// asynchronous output.
///
/// Only text written at an idle prompt belongs in the shadow: while a command
/// runs, the same bytes are that program's stdin, not an edited command line.
fn record_external_input(
    state: BlockState,
    text: &str,
    typed_cmd: &RefCell<String>,
    pty_synced: &Cell<bool>,
    idle_input_dirty: &Cell<bool>,
) {
    if state != BlockState::AwaitingCommand || text.is_empty() {
        return;
    }
    idle_input_dirty.set(true);
    // The shell's line buffer now holds text this app put there, so a later
    // recall must replace the line rather than append to it.
    pty_synced.set(true);
    append_typed_command_shadow(&mut typed_cmd.borrow_mut(), text);
}

fn classify_command_prompt_status(
    state: BlockState,
    fullscreen: bool,
    idle_input_dirty: bool,
    pty_synced: bool,
    typed_command_empty: bool,
) -> CommandPromptStatus {
    if fullscreen || state == BlockState::AltScreen {
        return CommandPromptStatus::Fullscreen;
    }
    match state {
        BlockState::AwaitingCommand => {
            if idle_input_dirty || pty_synced || !typed_command_empty {
                CommandPromptStatus::HasInput
            } else {
                CommandPromptStatus::Ready
            }
        }
        BlockState::CollectingOutput | BlockState::PostCommand => CommandPromptStatus::Running,
        BlockState::RawFallback => CommandPromptStatus::ShellIntegrationUnavailable,
        BlockState::Idle | BlockState::CollectingPrompt => CommandPromptStatus::Initializing,
        BlockState::AltScreen => CommandPromptStatus::Fullscreen,
    }
}

/// Replace the current editable shell line with a finished command.
///
/// Refuse to write while a command or full-screen program owns the PTY. Besides
/// preventing accidental input injection, keeping `typed_cmd` synchronized makes
/// the compact live cell expand correctly for recalled multiline commands.
pub(crate) fn recall_command_at_prompt(
    pty: &OwnedPty,
    pty_synced: &Cell<bool>,
    typed_cmd: &RefCell<String>,
    state: BlockState,
    agent_submission_pending: bool,
    command: &str,
    bracketed_paste: bool,
) -> bool {
    if state != BlockState::AwaitingCommand || agent_submission_pending {
        return false;
    }

    let recall = build_command_recall(command, bracketed_paste);
    if recall.is_empty() {
        return false;
    }

    // One write: the Ctrl+U, the framing and the body are a single payload, so
    // the PTY boundary sees a whole frame and no other writer can interleave
    // between the line kill and the text that replaces it.
    pty.write_bytes(&recall.bytes);
    *typed_cmd.borrow_mut() = recall.echo_text;
    pty_synced.set(true);
    true
}

/// Dynamic OSC 10/11/12 color overrides for one pane.
///
/// The parser passes the original set/reset bytes through, so the live VTE
/// recolors itself natively; this struct only remembers the values so that a
/// later OSC color QUERY reports what the app set instead of the static theme
/// (vim `background=` probes, theme-switching tools), and so finished-block
/// VTEs created after the change match the recolored live view. OSC
/// 110/111/112 resets a slot back to the theme.
#[derive(Clone, Copy, Default)]
pub(crate) struct DynamicColors {
    foreground: Option<RGBA>,
    background: Option<RGBA>,
    cursor: Option<RGBA>,
}

impl DynamicColors {
    /// Record a dynamic color SET. Unparseable specs are ignored: the raw
    /// bytes still passed through to the VTE, which applies its own parse, so
    /// dropping the tracked value merely keeps the theme answer for queries.
    fn set(&mut self, kind: ColorKind, spec: &str) {
        let Some(rgba) = parse_color_spec(spec) else {
            return;
        };
        if let Some(slot) = self.slot_mut(kind) {
            *slot = Some(rgba);
        }
    }

    /// Drop a dynamic override (OSC 110/111/112) so queries fall back to the
    /// theme color again.
    fn reset(&mut self, kind: ColorKind) {
        if let Some(slot) = self.slot_mut(kind) {
            *slot = None;
        }
    }

    fn get(&self, kind: ColorKind) -> Option<RGBA> {
        match kind {
            ColorKind::Foreground => self.foreground,
            ColorKind::Background => self.background,
            ColorKind::Cursor => self.cursor,
            // OSC 4 palette sets are not tracked (VTE owns them natively).
            ColorKind::Palette(_) => None,
        }
    }

    fn slot_mut(&mut self, kind: ColorKind) -> Option<&mut Option<RGBA>> {
        match kind {
            ColorKind::Foreground => Some(&mut self.foreground),
            ColorKind::Background => Some(&mut self.background),
            ColorKind::Cursor => Some(&mut self.cursor),
            ColorKind::Palette(_) => None,
        }
    }

    /// Clone the theme `config` with any dynamic overrides substituted, so a
    /// finished-block VTE created after an OSC 10/11/12 change matches the
    /// natively recolored live VTE instead of flashing back to theme colors.
    fn overlay(&self, config: &Config) -> Config {
        let mut config = config.clone();
        if let Some(fg) = self.foreground {
            config.foreground = fg;
        }
        if let Some(bg) = self.background {
            config.background = bg;
        }
        if let Some(cursor) = self.cursor {
            config.cursor = cursor;
        }
        config
    }
}

/// Per-pane handle to the tracked dynamic colors. The reader loop, the
/// undo-clear rebuild, and the theme-apply path all mutate/read the same cell,
/// so a color set by the app is reflected everywhere a block is built.
type DynamicColorsRc = Rc<Cell<DynamicColors>>;

/// The config a finished-block VTE must be built with: the pane theme with any
/// live OSC 10/11/12 overrides substituted. Shared by the reader's
/// block-finished path and `undo_clear_blocks`, so restored blocks cannot end
/// up theme-colored next to correctly recolored neighbors.
fn finished_block_config(dynamic: &DynamicColorsRc, config: &Config) -> Config {
    dynamic.get().overlay(config)
}

/// Drop every tracked dynamic override. An explicit user theme change repaints
/// the live VTE and all snapshot VTEs from the theme, so keeping the app's OSC
/// 10/11/12 values would leave color queries reporting a color nothing on
/// screen still uses. Most terminals reset dynamic colors on a theme change for
/// exactly this reason; an app that cares can set its colors again.
fn clear_dynamic_colors(dynamic: &DynamicColorsRc) {
    dynamic.set(DynamicColors::default());
}

/// Parse an OSC 10/11/12 color spec. `RGBA::parse` covers the hex forms,
/// CSS/X11 names, and `rgb()`/`rgba()`, but not XParseColor's
/// `rgb:<r>/<g>/<b>` (1–4 hex digits per channel) — the canonical form apps
/// send — so normalize that one by hand first.
fn parse_color_spec(spec: &str) -> Option<RGBA> {
    if let Some(body) = spec.strip_prefix("rgb:") {
        let mut channels = body.split('/');
        let (r, g, b) = (channels.next()?, channels.next()?, channels.next()?);
        if channels.next().is_some() {
            return None;
        }
        return Some(RGBA::new(
            x11_channel(r)?,
            x11_channel(g)?,
            x11_channel(b)?,
            1.0,
        ));
    }
    RGBA::parse(spec).ok()
}

/// One XParseColor hex channel scaled to 0.0..=1.0: `"f"` and `"ffff"` both
/// mean full intensity (value / (16^len - 1)).
fn x11_channel(text: &str) -> Option<f32> {
    if text.is_empty() || text.len() > 4 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(text, 16).ok()?;
    let max = (1u32 << (4 * text.len() as u32)) - 1;
    Some(value as f32 / max as f32)
}

fn build_color_query_reply(config: &Config, dynamic: DynamicColors, kind: ColorKind) -> String {
    let rgba = dynamic.get(kind).unwrap_or_else(|| match kind {
        ColorKind::Foreground => config.foreground,
        ColorKind::Background => config.background,
        ColorKind::Cursor => config.cursor,
        ColorKind::Palette(idx) => {
            let (r, g, b) = crate::terminal::ansi::ansi256_to_rgb(idx, &config.palette);
            RGBA::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
        }
    });
    let r = (rgba.red() * 65535.0) as u16;
    let g = (rgba.green() * 65535.0) as u16;
    let b = (rgba.blue() * 65535.0) as u16;
    match kind {
        ColorKind::Foreground => format!("\x1b]10;rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\"),
        ColorKind::Background => format!("\x1b]11;rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\"),
        ColorKind::Cursor => format!("\x1b]12;rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\"),
        ColorKind::Palette(idx) => {
            format!("\x1b]4;{idx};rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\")
        }
    }
}

fn build_keyboard_query_reply(
    query: KeyboardProtocolQuery,
    cursor_col: i64,
    cursor_row: i64,
) -> String {
    match query {
        KeyboardProtocolQuery::KittyQuery => "\x1b[?0u".to_string(),
        KeyboardProtocolQuery::ModifyOtherKeysQuery => "\x1b[>4;0m".to_string(),
        KeyboardProtocolQuery::PrimaryDeviceAttributes => "\x1b[?1;2c".to_string(),
        KeyboardProtocolQuery::SecondaryDeviceAttributes => "\x1b[>0;0;0c".to_string(),
        KeyboardProtocolQuery::TertiaryDeviceAttributes => "\x1bP!|00000000\x1b\\".to_string(),
        KeyboardProtocolQuery::XtVersion => {
            format!("\x1bP>|anvil {}\x1b\\", env!("CARGO_PKG_VERSION"))
        }
        KeyboardProtocolQuery::DeviceStatus => "\x1b[0n".to_string(),
        KeyboardProtocolQuery::CursorPosition => format!(
            "\x1b[{};{}R",
            cursor_row.saturating_add(1).max(1),
            cursor_col.saturating_add(1).max(1)
        ),
    }
}

type SelectedBlockIds = Rc<RefCell<std::collections::HashSet<u64>>>;

/// Apply the Warp-style multi-selection model to every finished block. All
/// selected blocks get a light outline; the active edge gets the stronger outline
/// and owns the persistent quick-action row.
fn sync_finished_block_selection(
    finished: &[FinishedBlock],
    selected_block_ids: &SelectedBlockIds,
    selected_block_id: &Rc<Cell<Option<u64>>>,
) {
    let selected = selected_block_ids.borrow();
    let active = selected_block_id.get();
    for block in finished {
        let is_selected = selected.contains(&block.id);
        if is_selected {
            block.widget().add_css_class("block-selected");
        } else {
            block.widget().remove_css_class("block-selected");
        }

        let is_active = active == Some(block.id);
        if is_active {
            block.widget().add_css_class("block-selection-active");
            block.action_box.set_visible(true);
        } else {
            block.widget().remove_css_class("block-selection-active");
            if !block.widget().has_css_class("block-hovered") {
                block.action_box.set_visible(false);
            }
        }
    }
}

fn clear_finished_block_selection(
    finished: &[FinishedBlock],
    selected_block_ids: &SelectedBlockIds,
    selected_block_id: &Rc<Cell<Option<u64>>>,
    selection_anchor_id: &Rc<Cell<Option<u64>>>,
) {
    selected_block_ids.borrow_mut().clear();
    selected_block_id.set(None);
    selection_anchor_id.set(None);
    sync_finished_block_selection(finished, selected_block_ids, selected_block_id);
}

fn replace_finished_block_selection(
    finished: &[FinishedBlock],
    selected_block_ids: &SelectedBlockIds,
    selected_block_id: &Rc<Cell<Option<u64>>>,
    selection_anchor_id: &Rc<Cell<Option<u64>>>,
    new_id: Option<u64>,
) {
    {
        let mut selected = selected_block_ids.borrow_mut();
        selected.clear();
        if let Some(id) = new_id {
            selected.insert(id);
        }
    }
    selected_block_id.set(new_id);
    selection_anchor_id.set(new_id);
    sync_finished_block_selection(finished, selected_block_ids, selected_block_id);
}

/// Make `id` the active edge without discarding an existing multi-selection.
/// Right-click uses this so opening actions on one selected block does not collapse
/// a range the user just built.
fn activate_finished_block_selection(
    finished: &[FinishedBlock],
    selected_block_ids: &SelectedBlockIds,
    selected_block_id: &Rc<Cell<Option<u64>>>,
    selection_anchor_id: &Rc<Cell<Option<u64>>>,
    id: u64,
) {
    if !selected_block_ids.borrow().contains(&id) {
        replace_finished_block_selection(
            finished,
            selected_block_ids,
            selected_block_id,
            selection_anchor_id,
            Some(id),
        );
        return;
    }
    selected_block_id.set(Some(id));
    selection_anchor_id.set(Some(id));
    sync_finished_block_selection(finished, selected_block_ids, selected_block_id);
}

fn toggle_finished_block_selection(
    finished: &[FinishedBlock],
    selected_block_ids: &SelectedBlockIds,
    selected_block_id: &Rc<Cell<Option<u64>>>,
    selection_anchor_id: &Rc<Cell<Option<u64>>>,
    id: u64,
) {
    let removed = {
        let mut selected = selected_block_ids.borrow_mut();
        if selected.remove(&id) {
            true
        } else {
            selected.insert(id);
            false
        }
    };

    if removed {
        let active_missing = selected_block_id
            .get()
            .is_some_and(|active| !selected_block_ids.borrow().contains(&active));
        if selected_block_id.get() == Some(id) || active_missing {
            let fallback = {
                let selected = selected_block_ids.borrow();
                finished
                    .iter()
                    .rev()
                    .find(|block| selected.contains(&block.id))
                    .map(|block| block.id)
            };
            selected_block_id.set(fallback);
        }
        let anchor_missing = selection_anchor_id
            .get()
            .is_some_and(|anchor| !selected_block_ids.borrow().contains(&anchor));
        if selection_anchor_id.get() == Some(id) || anchor_missing {
            selection_anchor_id.set(selected_block_id.get());
        }
    } else {
        selected_block_id.set(Some(id));
        selection_anchor_id.set(Some(id));
    }
    sync_finished_block_selection(finished, selected_block_ids, selected_block_id);
}

fn selected_id_range(ids: &[u64], anchor: u64, target: u64) -> Vec<u64> {
    let Some(anchor_index) = ids.iter().position(|id| *id == anchor) else {
        return vec![target];
    };
    let Some(target_index) = ids.iter().position(|id| *id == target) else {
        return vec![target];
    };
    let (start, end) = if anchor_index <= target_index {
        (anchor_index, target_index)
    } else {
        (target_index, anchor_index)
    };
    ids[start..=end].to_vec()
}

/// Pick the next index from an ascending `marked` list relative to the current
/// position: strictly before/after `cur` in the travel direction, wrapping to
/// the far end when nothing remains in that direction. `None` only when
/// `marked` is empty.
fn step_marked_indices(marked: &[usize], cur: Option<usize>, direction: i32) -> Option<usize> {
    if direction < 0 {
        marked
            .iter()
            .rev()
            .find(|&&idx| cur.map(|c| idx < c).unwrap_or(true))
            .copied()
            .or_else(|| marked.last().copied())
    } else {
        marked
            .iter()
            .find(|&&idx| cur.map(|c| idx > c).unwrap_or(true))
            .copied()
            .or_else(|| marked.first().copied())
    }
}

/// Resolve stable marked ids through current document order before applying
/// the existing previous/next wrapping behavior.
fn step_marked_record_ids(
    record_ids: &[u64],
    marked_ids: &[u64],
    current_id: Option<u64>,
    direction: i32,
) -> Option<u64> {
    let marked_ids: HashSet<u64> = marked_ids.iter().copied().collect();
    let marked_indices: Vec<usize> = record_ids
        .iter()
        .enumerate()
        .filter_map(|(index, id)| marked_ids.contains(id).then_some(index))
        .collect();
    let current = current_id.and_then(|id| record_ids.iter().position(|record| *record == id));
    step_marked_indices(&marked_indices, current, direction).map(|index| record_ids[index])
}

fn select_finished_block_range(
    finished: &[FinishedBlock],
    selected_block_ids: &SelectedBlockIds,
    selected_block_id: &Rc<Cell<Option<u64>>>,
    selection_anchor_id: &Rc<Cell<Option<u64>>>,
    target: u64,
) {
    let anchor = selection_anchor_id
        .get()
        .or_else(|| selected_block_id.get())
        .unwrap_or(target);
    let ordered_ids: Vec<u64> = finished.iter().map(|block| block.id).collect();
    let range = selected_id_range(&ordered_ids, anchor, target);
    {
        let mut selected = selected_block_ids.borrow_mut();
        selected.clear();
        selected.extend(range);
    }
    selected_block_id.set(Some(target));
    selection_anchor_id.set(Some(anchor));
    sync_finished_block_selection(finished, selected_block_ids, selected_block_id);
}

fn remove_finished_block_from_selection(
    finished: &[FinishedBlock],
    selected_block_ids: &SelectedBlockIds,
    selected_block_id: &Rc<Cell<Option<u64>>>,
    selection_anchor_id: &Rc<Cell<Option<u64>>>,
    removed_id: u64,
) {
    selected_block_ids.borrow_mut().remove(&removed_id);
    let active_missing = selected_block_id
        .get()
        .is_some_and(|active| !selected_block_ids.borrow().contains(&active));
    if selected_block_id.get() == Some(removed_id) || active_missing {
        let fallback = {
            let selected = selected_block_ids.borrow();
            finished
                .iter()
                .rev()
                .find(|block| selected.contains(&block.id))
                .map(|block| block.id)
        };
        selected_block_id.set(fallback);
    }
    let anchor_missing = selection_anchor_id
        .get()
        .is_some_and(|anchor| !selected_block_ids.borrow().contains(&anchor));
    if selection_anchor_id.get() == Some(removed_id) || anchor_missing {
        selection_anchor_id.set(selected_block_id.get());
    }
    sync_finished_block_selection(finished, selected_block_ids, selected_block_id);
}

struct BlockRemovalRefs<'a> {
    selected_ids: &'a SelectedBlockIds,
    selected: &'a Rc<Cell<Option<u64>>>,
    anchor: &'a Rc<Cell<Option<u64>>>,
    bookmarks: &'a Rc<RefCell<HashSet<u64>>>,
    visible_indices: &'a Rc<RefCell<HashSet<usize>>>,
    failure_marker_redraw: &'a dyn Fn(),
    unread_count: &'a Rc<Cell<u32>>,
    jump_fab: &'a gtk::Button,
}

fn plan_completed_block_retention_with_restored(
    restored: &[BlockData],
    finished: &[FinishedBlock],
    max_blocks: usize,
) -> CompletedBlockRetentionPlan {
    let candidates: Vec<(u64, usize)> = restored
        .iter()
        .map(|block| (block.id, block.estimated_restored_retained_bytes()))
        .chain(
            finished
                .iter()
                .map(|block| (block.id, block.estimated_retained_bytes())),
        )
        .collect();
    completed_block_retention_plan(&candidates, max_blocks, MAX_COMPLETED_BLOCK_RETAINED_BYTES)
}

fn plan_completed_block_retention_with_newest(
    finished: &[FinishedBlock],
    newest_id: u64,
    newest_estimated_bytes: usize,
    max_blocks: usize,
) -> CompletedBlockRetentionPlan {
    let candidates: Vec<(u64, usize)> = finished
        .iter()
        .map(|block| (block.id, block.estimated_retained_bytes()))
        .chain(std::iter::once((newest_id, newest_estimated_bytes)))
        .collect();
    completed_block_retention_plan(&candidates, max_blocks, MAX_COMPLETED_BLOCK_RETAINED_BYTES)
}

fn log_completed_block_retention(context: &str, plan: CompletedBlockRetentionPlan) {
    if plan.byte_budget_evictions > 0 {
        log::info!(
            "{context}: completed-block byte budget evicted {} additional oldest block(s) ({} total with count limit); retained {} block(s), estimated {} bytes of {}",
            plan.byte_budget_evictions,
            plan.evict_prefix,
            plan.retained_count,
            plan.retained_estimated_bytes,
            MAX_COMPLETED_BLOCK_RETAINED_BYTES,
        );
    }
    if plan.newest_exceeds_byte_budget {
        log::warn!(
            "{context}: newest completed block alone is estimated at {} bytes, above the {}-byte per-pane cap; retaining it by newest-wins policy",
            plan.retained_estimated_bytes,
            MAX_COMPLETED_BLOCK_RETAINED_BYTES,
        );
    }
}

fn clear_find_handles(
    _finished_blocks: &Rc<RefCell<Vec<FinishedBlock>>>,
    active_vte: &Terminal,
    find_state: &Rc<RefCell<FindState>>,
) {
    find::clear_find_state(find_state.as_ref(), active_vte);
}

/// Remove an oldest prefix and every piece of state indexed by those blocks.
/// The widget pool strips the heavyweight child tree on every release.
fn evict_finished_block_prefix(
    requested: usize,
    finished_blocks: &Rc<RefCell<Vec<FinishedBlock>>>,
    block_data: &Rc<RefCell<VecDeque<BlockData>>>,
    block_list: &gtk::Box,
    widget_pool: &Rc<RefCell<WidgetPool>>,
    refs: BlockRemovalRefs<'_>,
) -> usize {
    let (total_before, evicted): (usize, Vec<_>) = {
        let mut finished = finished_blocks.borrow_mut();
        let total_before = finished.len();
        let count = requested.min(finished.len());
        (total_before, finished.drain(..count).collect())
    };
    if evicted.is_empty() {
        return 0;
    }

    let evicted_count = evicted.len();
    let unread = unread_after_prefix_eviction(total_before, refs.unread_count.get(), evicted_count);
    refs.unread_count.set(unread);
    set_jump_fab_label(refs.jump_fab, unread);
    let evicted_ids: HashSet<_> = evicted.iter().map(|block| block.id).collect();
    mutate_block_data_and_redraw(block_data, refs.failure_marker_redraw, |blocks| {
        let prefixes_match = blocks
            .iter()
            .take(evicted_count)
            .map(|block| block.id)
            .eq(evicted.iter().map(|block| block.id));
        if !prefixes_match {
            log::error!(
                "completed-block retention found desynchronized widget/data prefixes (widgets={}, data={})",
                evicted_count,
                blocks.len(),
            );
        }
        debug_assert!(prefixes_match);
        blocks.drain(..evicted_count.min(blocks.len()));
    });
    refs.selected_ids
        .borrow_mut()
        .retain(|id| !evicted_ids.contains(id));
    let finished = finished_blocks.borrow();
    let selected = refs.selected_ids.borrow();
    let fallback = finished
        .iter()
        .rev()
        .find(|block| selected.contains(&block.id))
        .map(|block| block.id);
    if refs
        .selected
        .get()
        .is_some_and(|id| evicted_ids.contains(&id) || !selected.contains(&id))
    {
        refs.selected.set(fallback);
    }
    if refs
        .anchor
        .get()
        .is_some_and(|id| evicted_ids.contains(&id) || !selected.contains(&id))
    {
        refs.anchor.set(refs.selected.get());
    }
    drop(selected);
    sync_finished_block_selection(&finished, refs.selected_ids, refs.selected);
    drop(finished);

    refs.bookmarks
        .borrow_mut()
        .retain(|id| !evicted_ids.contains(id));
    {
        let mut visible = refs.visible_indices.borrow_mut();
        *visible = visible
            .iter()
            .filter_map(|&index| index.checked_sub(evicted_count))
            .collect();
    }
    for block in evicted {
        let widget = block.widget().clone();
        block_list.remove(&widget);
        widget_pool.borrow_mut().release(widget);
    }
    block_list.queue_allocate();
    evicted_count
}

/// Bring a selected block into the upper third of the viewport. `compute_point`
/// returns viewport-relative coordinates, so add the current adjustment before
/// calculating the new absolute scroll position.
fn scroll_finished_block_into_view(block: &FinishedBlock, scroll: &ScrolledWindow) {
    let widget = block.widget().clone();
    let scroll = scroll.clone();
    glib::idle_add_local_once(move || {
        if let Some(point) = widget.compute_point(&scroll, &gtk::graphene::Point::new(0.0, 0.0)) {
            let adj = scroll.vadjustment();
            let max_value = (adj.upper() - adj.page_size()).max(adj.lower());
            let target = adj.value() + point.y() as f64 - adj.page_size() / 3.0;
            adj.set_value(target.clamp(adj.lower(), max_value));
        }
    });
}

/// HOME/END move through the outer history canvas. END repeats for a few layout
/// passes because virtualized blocks regain height as they enter the viewport.
fn scroll_history_to_edge(scroll: &ScrolledWindow, bottom: bool) {
    let adj = scroll.vadjustment();
    if !bottom {
        adj.set_value(adj.lower());
        return;
    }
    adj.set_value((adj.upper() - adj.page_size()).max(adj.lower()));
    let scroll = scroll.clone();
    let tries = Rc::new(Cell::new(0u8));
    glib::idle_add_local(move || {
        if tries.get() >= 12 {
            return glib::ControlFlow::Break;
        }
        tries.set(tries.get() + 1);
        let adj = scroll.vadjustment();
        let before = adj.value();
        let target = (adj.upper() - adj.page_size()).max(adj.lower());
        adj.set_value(target);
        if (adj.value() - before).abs() < 1.0 {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

/// Move the Warp-style active selection by one item. Plain arrows collapse a
/// multi-selection back to the newly active block; Shift+arrows use the range
/// helper below to expand or contract instead.
fn move_finished_block_selection(
    finished: &[FinishedBlock],
    selected_block_ids: &SelectedBlockIds,
    selected_block_id: &Rc<Cell<Option<u64>>>,
    selection_anchor_id: &Rc<Cell<Option<u64>>>,
    scroll: &ScrolledWindow,
    direction: i32,
) -> bool {
    if finished.is_empty() || direction == 0 {
        return false;
    }
    let current = selected_block_id
        .get()
        .and_then(|id| finished.iter().position(|block| block.id == id));
    let target = if direction < 0 {
        match current {
            None => Some(finished.len() - 1),
            Some(0) => Some(0),
            Some(index) => Some(index - 1),
        }
    } else {
        match current {
            None => return false,
            Some(index) if index + 1 >= finished.len() => None,
            Some(index) => Some(index + 1),
        }
    };
    let target_id = target.and_then(|index| finished.get(index).map(|block| block.id));
    replace_finished_block_selection(
        finished,
        selected_block_ids,
        selected_block_id,
        selection_anchor_id,
        target_id,
    );
    if let Some(index) = target {
        if let Some(block) = finished.get(index) {
            scroll_finished_block_into_view(block, scroll);
        }
    }
    true
}

fn extend_finished_block_selection(
    finished: &[FinishedBlock],
    selected_block_ids: &SelectedBlockIds,
    selected_block_id: &Rc<Cell<Option<u64>>>,
    selection_anchor_id: &Rc<Cell<Option<u64>>>,
    scroll: &ScrolledWindow,
    direction: i32,
) -> bool {
    if finished.is_empty() || direction == 0 {
        return false;
    }
    let Some(current) = selected_block_id
        .get()
        .and_then(|id| finished.iter().position(|block| block.id == id))
    else {
        return false;
    };
    let target = if direction < 0 {
        current.saturating_sub(1)
    } else {
        (current + 1).min(finished.len() - 1)
    };
    let Some(block) = finished.get(target) else {
        return false;
    };
    select_finished_block_range(
        finished,
        selected_block_ids,
        selected_block_id,
        selection_anchor_id,
        block.id,
    );
    scroll_finished_block_into_view(block, scroll);
    true
}

fn scroll_selected_finished_block_edge(
    finished: &[FinishedBlock],
    selected_block_id: &Rc<Cell<Option<u64>>>,
    scroll: &ScrolledWindow,
    bottom: bool,
) -> bool {
    let Some(id) = selected_block_id.get() else {
        return false;
    };
    let Some(block) = finished.iter().find(|block| block.id == id) else {
        return false;
    };
    block.scroll_to_edge(scroll, bottom);
    true
}

/// Install Warp's Linux selection gestures. Plain click makes one active block,
/// Shift-click selects a contiguous range, and Ctrl+Shift-click toggles a block.
/// Modifier clicks work across the card; plain clicks remain header-only so VTE
/// output keeps native text selection.
fn install_finished_block_selection(
    block: &FinishedBlock,
    active: &Rc<RefCell<ActiveBlock>>,
    finished_blocks: &Rc<RefCell<Vec<FinishedBlock>>>,
    selected_block_ids: &SelectedBlockIds,
    selected_block_id: &Rc<Cell<Option<u64>>>,
    selection_anchor_id: &Rc<Cell<Option<u64>>>,
) {
    let active_for_click = active.clone();
    let header_for_click = block.header_row.clone();
    let finished_blocks_for_select = finished_blocks.clone();
    let selected_ids_for_click = selected_block_ids.clone();
    let selected_for_click = selected_block_id.clone();
    let anchor_for_click = selection_anchor_id.clone();
    let this_id = block.id;
    let left_click = gtk::GestureClick::new();
    left_click.set_button(1);
    left_click.set_propagation_phase(gtk::PropagationPhase::Capture);
    left_click.connect_pressed(move |gesture, n_press, _, y| {
        if n_press != 1 {
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        }
        let state = gesture.current_event_state();
        let ctrl = state.contains(gtk::gdk::ModifierType::CONTROL_MASK);
        let shift = state.contains(gtk::gdk::ModifierType::SHIFT_MASK);
        let over_terminal_surface = y > header_for_click.height() as f64;
        if !over_terminal_surface || shift {
            active_for_click.borrow().grab_focus();
            let finished = finished_blocks_for_select.borrow();
            if ctrl && shift {
                toggle_finished_block_selection(
                    &finished,
                    &selected_ids_for_click,
                    &selected_for_click,
                    &anchor_for_click,
                    this_id,
                );
            } else if shift {
                select_finished_block_range(
                    &finished,
                    &selected_ids_for_click,
                    &selected_for_click,
                    &anchor_for_click,
                    this_id,
                );
            } else {
                replace_finished_block_selection(
                    &finished,
                    &selected_ids_for_click,
                    &selected_for_click,
                    &anchor_for_click,
                    Some(this_id),
                );
            }
        }
        // A modifier click on command/output is a block-selection gesture,
        // not the start of a native VTE text selection. Header clicks still
        // proceed so collapse/action buttons receive their own sequence.
        gesture.set_state(if shift && over_terminal_surface {
            gtk::EventSequenceState::Claimed
        } else {
            gtk::EventSequenceState::Denied
        });
    });
    block.widget().add_controller(left_click);
}

impl BlockBackend {
    /// Install the right-click context menu on a finished block.
    ///
    /// Live-finalize path only: `undo_clear_blocks` and session restore
    /// deliberately rebuild blocks without this menu. Inherent rather than
    /// part of [`RenderBackend`]: the menu is Block-mode chrome, and the
    /// `*_for_menu` clones below are the closure's own captures — the
    /// per-finalize context bundle that used to re-clone them is gone.
    ///
    /// `finished_widget` is the root widget of the finished block; the
    /// right-click gesture attaches there.
    fn install_finished_block_context_menu(
        &self,
        finished_widget: gtk::Box,
        finished_menu_clone: FinishedBlock,
        block_id: u64,
    ) {
        let block_data_for_export = self.block_data_for_cb.clone();
        let finished_blocks_for_menu = self.finished_blocks_for_cb.clone();
        let block_list_for_menu = self.block_list_rc.clone();
        let vte_for_copy = self.active_vte.clone();
        let pty_for_rerun_menu = self.pty_for_init.clone();
        let pty_synced_for_rerun_menu = self.pty_synced_rc.clone();
        let bracketed_paste_for_rerun_menu = self.bracketed_paste_rc.clone();
        let typed_cmd_for_rerun_menu = self.typed_cmd_rc.clone();
        let armed_agent_for_rerun_menu = self.armed_agent_execution_rc.clone();
        let bstate_for_rerun_menu = self.bstate_rc.clone();
        let active_for_rerun_menu = self.active_rc.clone();
        let selected_ids_for_menu = self.selected_block_ids_rc.clone();
        let selected_for_menu = self.selected_block_id_rc.clone();
        let anchor_for_menu = self.selection_anchor_id_rc.clone();
        let bookmarks_for_menu = self.bookmarks_rc.clone();
        let block_scroll_for_menu = self.block_scroll_rc.clone();
        let visible_for_menu = self.visible_indices_rc.clone();
        let widget_pool_for_menu = self.widget_pool_for_cb.clone();
        let ask_ai_cbs_for_menu = self.ask_ai_about_block_cbs.clone();
        let failure_marker_redraw_for_menu = self.failure_marker_redraw.clone();
        let unread_for_menu = self.unread_count_rc.clone();
        let jump_fab_for_menu = self.jump_fab.clone();

        let right_click = gtk::GestureClick::new();
        right_click.set_button(3);

        right_click.connect_pressed(move |gesture, _n_press, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            {
                let finished = finished_blocks_for_menu.borrow();
                activate_finished_block_selection(
                    &finished,
                    &selected_ids_for_menu,
                    &selected_for_menu,
                    &anchor_for_menu,
                    block_id,
                );
            }

            let popover = gtk::Popover::new();
            let widget: &gtk::Widget =
                &finished_menu_clone.widget().clone().upcast::<gtk::Widget>();
            popover.set_parent(widget);
            popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.set_has_arrow(false);

            let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
            vbox.add_css_class("menu");

            let make_item = |label: &str| -> gtk::Button {
                let btn = gtk::Button::with_label(label);
                btn.set_has_frame(false);
                btn.set_halign(gtk::Align::Fill);
                if let Some(child) = btn.child() {
                    child.set_halign(gtk::Align::Start);
                }
                btn.add_css_class("flat");
                btn
            };

            let selected_count = selected_ids_for_menu.borrow().len();
            let has_selected_commands = {
                let selected = selected_ids_for_menu.borrow();
                block_data_for_export
                    .borrow()
                    .iter()
                    .any(|block| selected.contains(&block.id) && !block.cmd.trim().is_empty())
            };

            if has_selected_commands {
                let item = make_item(if selected_count > 1 {
                    "Copy Commands"
                } else {
                    "Copy Command"
                });
                let popover_c = popover.clone();
                let block_data_for_copy = block_data_for_export.clone();
                let selected_ids_for_copy = selected_ids_for_menu.clone();
                let vte_for_action = vte_for_copy.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    let selected = selected_ids_for_copy.borrow();
                    let blocks = block_data_for_copy.borrow();
                    let text = selected_command_text(
                        blocks.iter().map(|block| (block.id, block.cmd.as_str())),
                        &selected,
                    );
                    vte_for_action.clipboard().set_text(&text);
                });
                vbox.append(&item);
            }

            {
                let item = make_item("Ask AI About Block");
                let popover_c = popover.clone();
                let finished_for_ai = finished_menu_clone.clone();
                let block_data_for_ai = block_data_for_export.clone();
                let callbacks_for_ai = ask_ai_cbs_for_menu.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    let output = finished_for_ai
                        .with_stripped_output(|text| crate::ai::truncate_for_context(text, 80));
                    let data = block_data_for_ai.borrow();
                    let record = data.iter().find(|block| block.id == block_id);
                    let truncated =
                        output.contains("lines elided") || output.contains("bytes elided");
                    let context = crate::ai::BlockContext {
                        cmd: finished_for_ai.cmd_text.clone(),
                        output,
                        cwd: record.and_then(|block| block.cwd.clone()),
                        exit_code: exit_code_for_i32_api(record.and_then(|block| block.exit_code)),
                        truncated,
                    };
                    for callback in callbacks_for_ai.borrow().iter() {
                        callback(context.clone());
                    }
                });
                vbox.append(&item);
            }
            {
                let item = make_item(if selected_count > 1 {
                    "Copy Outputs"
                } else {
                    "Copy Output"
                });
                let popover_c = popover.clone();
                let block_data_for_copy = block_data_for_export.clone();
                let selected_ids_for_copy = selected_ids_for_menu.clone();
                let vte_for_action = vte_for_copy.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    let selected = selected_ids_for_copy.borrow();
                    let blocks = block_data_for_copy.borrow();
                    let text = blocks
                        .iter()
                        .filter(|block| selected.contains(&block.id))
                        .map(|block| strip_ansi(&block.output))
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    vte_for_action.clipboard().set_text(&text);
                });
                vbox.append(&item);
            }

            {
                let item = make_item(if selected_count > 1 {
                    "Copy Blocks"
                } else {
                    "Copy Block"
                });
                let popover_c = popover.clone();
                let block_data_for_copy = block_data_for_export.clone();
                let selected_ids_for_copy = selected_ids_for_menu.clone();
                let vte_for_action = vte_for_copy.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    let selected = selected_ids_for_copy.borrow();
                    let blocks = block_data_for_copy.borrow();
                    let text = blocks
                        .iter()
                        .filter(|block| selected.contains(&block.id))
                        .map(|block| {
                            block_clipboard_text(&block.cmd, &strip_ansi(&block.output), false)
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    vte_for_action.clipboard().set_text(&text);
                });
                vbox.append(&item);
            }

            {
                let item = make_item("Scroll to Top of Block");
                let popover_c = popover.clone();
                let finished_for_scroll = finished_menu_clone.clone();
                let scroll_for_action = block_scroll_for_menu.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    finished_for_scroll.scroll_to_edge(&scroll_for_action, false);
                });
                vbox.append(&item);
            }
            if finished_menu_clone.long_output {
                let item = make_item("Jump to Bottom of Block");
                let popover_c = popover.clone();
                let finished_for_scroll = finished_menu_clone.clone();
                let scroll_for_action = block_scroll_for_menu.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    finished_for_scroll.scroll_to_edge(&scroll_for_action, true);
                });
                vbox.append(&item);
            }
            {
                let item = make_item("Toggle Output Filter");
                let popover_c = popover.clone();
                let finished_for_filter = finished_menu_clone.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    (finished_for_filter.toggle_filter)();
                });
                vbox.append(&item);
            }
            {
                let bookmarked = bookmarks_for_menu.borrow().contains(&block_id);
                let item = make_item(if bookmarked {
                    "Remove Bookmark"
                } else {
                    "Bookmark Block"
                });
                let popover_c = popover.clone();
                let finished_for_bookmark = finished_menu_clone.clone();
                let bookmarks_for_action = bookmarks_for_menu.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    let mut marks = bookmarks_for_action.borrow_mut();
                    let now_bookmarked = if marks.remove(&block_id) {
                        false
                    } else {
                        marks.insert(block_id);
                        true
                    };
                    finished_for_bookmark
                        .bookmark_star
                        .set_visible(now_bookmarked);
                    if now_bookmarked {
                        finished_for_bookmark
                            .widget()
                            .add_css_class("block-bookmarked");
                    } else {
                        finished_for_bookmark
                            .widget()
                            .remove_css_class("block-bookmarked");
                    }
                });
                vbox.append(&item);
            }
            vbox.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

            {
                let item = make_item(if selected_count > 1 {
                    "Copy Blocks as Markdown"
                } else {
                    "Copy Block as Markdown"
                });
                let popover_c = popover.clone();
                let block_data_for_md = block_data_for_export.clone();
                let selected_ids_for_md = selected_ids_for_menu.clone();
                let vte_for_action = vte_for_copy.clone();
                let block_id_md = block_id;
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    let selected = selected_ids_for_md.borrow();
                    let blocks = block_data_for_md.borrow();
                    let text = selected_blocks_markdown(blocks.iter(), &selected, block_id_md);
                    vte_for_action.clipboard().set_text(&text);
                });
                vbox.append(&item);
            }

            {
                let item = make_item("Export as JSON");
                let popover_c = popover.clone();
                let block_data_for_json = block_data_for_export.clone();
                let vte_for_json = vte_for_copy.clone();
                let block_id_json = block_id;
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    let blocks = block_data_for_json.borrow();
                    if let Some(block) = blocks.iter().find(|b| b.id == block_id_json) {
                        let json = block.to_json();
                        vte_for_json.clipboard().set_text(&json);
                    }
                });
                vbox.append(&item);
            }

            if has_selected_commands {
                let item = make_item(if selected_count > 1 {
                    "Insert Commands at Prompt"
                } else {
                    "Insert Command at Prompt"
                });
                let popover_c = popover.clone();
                let finished_for_rerun = finished_blocks_for_menu.clone();
                let selected_ids_for_rerun = selected_ids_for_menu.clone();
                let selected_for_rerun = selected_for_menu.clone();
                let anchor_for_rerun = anchor_for_menu.clone();
                let pty_for_action = pty_for_rerun_menu.clone();
                let pty_synced_for_action = pty_synced_for_rerun_menu.clone();
                let bracketed_paste_for_action = bracketed_paste_for_rerun_menu.clone();
                let typed_cmd_for_action = typed_cmd_for_rerun_menu.clone();
                let armed_agent_for_action = armed_agent_for_rerun_menu.clone();
                let bstate_for_action = bstate_for_rerun_menu.clone();
                let active_for_action = active_for_rerun_menu.clone();
                item.set_sensitive(
                    bstate_for_action.get() == BlockState::AwaitingCommand
                        && armed_agent_for_action.borrow().is_none(),
                );
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    let finished = finished_for_rerun.borrow();
                    let recalled = if armed_agent_for_action.borrow().is_some() {
                        false
                    } else {
                        let selected = selected_ids_for_rerun.borrow();
                        recall_selected_commands_at_prompt(
                            &pty_for_action,
                            &pty_synced_for_action,
                            &typed_cmd_for_action,
                            bstate_for_action.get(),
                            &finished,
                            &selected,
                            bracketed_paste_for_action.get(),
                        )
                    };
                    if recalled {
                        clear_finished_block_selection(
                            &finished,
                            &selected_ids_for_rerun,
                            &selected_for_rerun,
                            &anchor_for_rerun,
                        );
                        active_for_action.borrow().grab_focus();
                    }
                });
                vbox.append(&item);
            }
            vbox.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

            {
                let item = make_item("Export as Markdown");
                let popover_c = popover.clone();
                let block_data_for_md = block_data_for_export.clone();
                let vte_for_md = vte_for_copy.clone();
                let block_id_md = block_id;
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    let blocks = block_data_for_md.borrow();
                    if let Some(block) = blocks.iter().find(|b| b.id == block_id_md) {
                        let markdown = block.to_markdown();
                        vte_for_md.clipboard().set_text(&markdown);
                    }
                });
                vbox.append(&item);
            }

            {
                let item = make_item("Delete Block");
                let popover_c = popover.clone();
                let finished_blocks_for_delete = finished_blocks_for_menu.clone();
                let block_list_for_delete = block_list_for_menu.clone();
                let block_data_for_delete = block_data_for_export.clone();
                let selected_ids_for_delete = selected_ids_for_menu.clone();
                let selected_for_delete = selected_for_menu.clone();
                let anchor_for_delete = anchor_for_menu.clone();
                let bookmarks_for_delete = bookmarks_for_menu.clone();
                let visible_for_delete = visible_for_menu.clone();
                let widget_pool_for_delete = widget_pool_for_menu.clone();
                let failure_marker_redraw_for_delete = failure_marker_redraw_for_menu.clone();
                let unread_for_delete = unread_for_menu.clone();
                let jump_fab_for_delete = jump_fab_for_menu.clone();
                let block_id_del = block_id;
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    let mut blocks = finished_blocks_for_delete.borrow_mut();
                    let removed_pos = blocks.iter().position(|b| b.id == block_id_del);
                    if let Some(pos) = removed_pos {
                        let unread =
                            unread_after_index_removal(blocks.len(), unread_for_delete.get(), pos);
                        unread_for_delete.set(unread);
                        set_jump_fab_label(&jump_fab_for_delete, unread);
                        let block = blocks.remove(pos);
                        let widget = block.widget().clone();
                        block_list_for_delete.remove(&widget);
                        widget_pool_for_delete.borrow_mut().release(widget);
                    }
                    remove_finished_block_from_selection(
                        &blocks,
                        &selected_ids_for_delete,
                        &selected_for_delete,
                        &anchor_for_delete,
                        block_id_del,
                    );
                    // Keep block_data in lockstep with the widget list.
                    mutate_block_data_and_redraw(
                        &block_data_for_delete,
                        failure_marker_redraw_for_delete.as_ref(),
                        |blocks| {
                            if let Some(pos) = removed_pos {
                                let removed = blocks.remove(pos);
                                debug_assert_eq!(
                                    removed.as_ref().map(|block| block.id),
                                    Some(block_id_del),
                                );
                            }
                        },
                    );
                    bookmarks_for_delete.borrow_mut().remove(&block_id_del);
                    // Index-based virtualization must be recalculated after
                    // any removal; retaining the old set can hide the block
                    // that shifted into this slot until the next full scroll.
                    visible_for_delete.borrow_mut().clear();
                    block_list_for_delete.queue_allocate();
                });
                vbox.append(&item);
            }

            popover.set_child(Some(&vbox));
            popover.connect_closed(move |p| {
                p.unparent();
            });
            popover.popup();
        });
        finished_widget.add_controller(right_click);
    }
}

/// Cap on the retained raw output buffer for a single running command. The raw
/// byte buffer used to re-render the finished block grew without bound — a runaway
/// command (`cat /dev/urandom`) could exhaust memory before CommandEnd. When the
/// buffer exceeds this, the oldest bytes are dropped, keeping the most recent tail
/// (the part a finished block actually shows). 8 MiB comfortably covers any normal
/// command's output.
const MAX_RAW_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Keep moving bodies clear of the running-output scrollbar. The overlay
/// itself remains full-width so clipping follows the live terminal exactly.
const LIVE_ORGANISM_RIGHT_GUTTER: i32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LiveOrganismSurfaceMetrics {
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) cell_width: i32,
    pub(crate) cell_height: i32,
    pub(crate) right_gutter: i32,
    pub(crate) alt_screen: bool,
    /// On-screen grid row of the cursor, i.e. the live output growth edge.
    pub(crate) cursor_row: i32,
}

/// Accepted direct-human input, intentionally containing no typed bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HumanInputKind {
    Keyboard,
    Clipboard,
    ProcessControl,
    StickyStop,
}

/// Content-free ownership change for the terminal's alternate screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AltScreenTransition {
    Entered,
    Left,
}

/// Authoritative foreground-command lifecycle event emitted at OSC 133 `C`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandStartedEvent {
    pub(crate) command: String,
    pub(crate) cwd: Option<String>,
}

/// Authoritative foreground-command lifecycle event emitted at OSC 133 `D`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandFinishedEvent {
    pub(crate) command: String,
    pub(crate) cwd: Option<String>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) duration_ms: Option<u64>,
}

/// Minimum rows the live input cell is guaranteed when idle (warp-style compact
/// input): it shrinks to fit the prompt + typed command but never below this, so
/// there is always usable room to type. It grows with multiline input up to the
/// viewport, and is forced to the full viewport only for alt-screen apps.
const MIN_INPUT_ROWS: i32 = 6;

/// The identity and outcome of one completed command. Rendering-only values
/// are deliberately absent: Unified retains this metadata without ever
/// building a Block card payload.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CompletedCommandRecord {
    id: u64,
    /// Empty only for asynchronous output observed at an idle prompt.
    cmd: String,
    exit_code: Option<i32>,
    start_time_ms: Option<u64>,
    end_time_ms: Option<u64>,
    duration_ms: Option<u64>,
    cwd: Option<String>,
    is_background: bool,
}

/// Completed-command observers are metadata-only unless their predicate says
/// this exact completion needs a bounded output sample. The predicate is what
/// lets Anvil's permanent Relm4 bridge request output for a correlated Agent
/// completion without materializing every ordinary Unified command.
type MetadataBlockFinishedCallback =
    dyn Fn(String, Option<i32>, Option<crate::agent::AgentExecutionRef>, Option<u64>);
type OutputBlockFinishedCallback = dyn Fn(
    String,
    Option<i32>,
    Option<String>,
    Option<crate::agent::AgentExecutionRef>,
    Option<u64>,
);

enum BlockFinishedCallback {
    Metadata(Box<MetadataBlockFinishedCallback>),
    ConditionalOutput {
        needs_output: Box<dyn Fn(Option<crate::agent::AgentExecutionRef>) -> bool>,
        callback: Box<OutputBlockFinishedCallback>,
    },
}

type BlockFinishedCallbacks = Rc<RefCell<Vec<BlockFinishedCallback>>>;
type BlockContextCallbacks = Rc<RefCell<Vec<Box<dyn Fn(crate::ai::BlockContext)>>>>;
type CwdCallbacks = Rc<RefCell<Vec<Box<dyn Fn(&str, bool)>>>>;
type AgentExecutionLostCallbacks =
    Rc<RefCell<Vec<Box<dyn Fn(crate::agent::AgentExecutionRef, &'static str)>>>>;
type CommandStartedCallbacks = Rc<RefCell<Vec<Box<dyn Fn(CommandStartedEvent)>>>>;
type CommandFinishedCallbacks = Rc<RefCell<Vec<Box<dyn Fn(CommandFinishedEvent)>>>>;
type HumanInputCallbacks = Rc<RefCell<Vec<Box<dyn Fn(HumanInputKind)>>>>;
type AltScreenCallbacks = Rc<RefCell<Vec<Box<dyn Fn(AltScreenTransition)>>>>;

fn emit_command_started(callbacks: &CommandStartedCallbacks, event: CommandStartedEvent) {
    for callback in callbacks.borrow().iter() {
        callback(event.clone());
    }
}

fn emit_command_finished(callbacks: &CommandFinishedCallbacks, event: CommandFinishedEvent) {
    for callback in callbacks.borrow().iter() {
        callback(event.clone());
    }
}

fn emit_human_input(callbacks: &HumanInputCallbacks, kind: HumanInputKind) {
    for callback in callbacks.borrow().iter() {
        callback(kind);
    }
}

fn emit_alt_screen_transition(callbacks: &AltScreenCallbacks, transition: AltScreenTransition) {
    for callback in callbacks.borrow().iter() {
        callback(transition);
    }
}
pub(crate) type DebugInfo = Vec<(&'static str, Vec<(String, String)>)>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ArmedAgentExecution {
    execution: crate::agent::AgentExecutionRef,
    prompt_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReviewedSubmissionPhase {
    Inserting,
    Submitted,
}

#[derive(Clone, Debug)]
struct ReviewedSubmission {
    command: String,
    execution: Option<crate::agent::AgentExecutionRef>,
    prompt_generation: u64,
    phase: ReviewedSubmissionPhase,
    identity_feed_tainted: bool,
}

const VERIFIED_SUBMISSION_POLL: std::time::Duration = std::time::Duration::from_millis(16);
const VERIFIED_SUBMISSION_MAX_POLLS: u32 = 120;
const REVIEWED_COMMAND_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const VERIFIED_SUBMISSION_LOST: &str =
    "the rendered shell editor did not exactly match the approved command before execution";
const REVIEWED_COMMAND_START_LOST: &str =
    "the shell did not report the exact reviewed command starting before the safety deadline";

fn emit_agent_execution_lost(
    callbacks: &AgentExecutionLostCallbacks,
    execution: crate::agent::AgentExecutionRef,
    reason: &'static str,
) {
    for callback in callbacks.borrow().iter() {
        callback(execution, reason);
    }
}

fn command_capture_range_is_bounded(start_row: i64, end_row: i64, columns: i64) -> bool {
    end_row
        .checked_sub(start_row)
        .and_then(|rows| rows.checked_add(1))
        .filter(|rows| *rows > 0)
        .and_then(|rows| rows.checked_mul(columns.max(1)))
        .and_then(|cells| usize::try_from(cells).ok())
        .is_some_and(|cells| cells <= MAX_RECALLED_COMMAND_BYTES)
}

/// Convert VTE's ring row into the screen-relative row required by DSR 6.
/// Unified never clears its ring, so reporting the raw row would grow without
/// bound over the lifetime of the pane.
fn screen_relative_cpr_row(row: i64, top_row: i64, rows: i64) -> i64 {
    if rows <= 0 {
        return row.max(0);
    }
    (row - top_row).clamp(0, rows - 1)
}

/// Rebase a prompt anchor when the live surface changes grid height between
/// PromptEnd and CommandStart. Block's compact/full `set_size` transition
/// moves the relevant row by this delta; Unified keeps one stable full-size
/// surface and therefore uses the identity policy.
fn rebase_prompt_anchor(anchor: (i64, i64), recorded_rows: i64, current_rows: i64) -> (i64, i64) {
    if recorded_rows <= 0 || current_rows <= 0 {
        return anchor;
    }
    let (col, row) = anchor;
    (col, row.saturating_add(current_rows - recorded_rows).max(0))
}

/// Construction-time anchor policy derived from the same switch that selects
/// the render backend. This value is copied into both the lifecycle backend
/// and the query-only submission surface, so no prompt consumer can drift.
fn prompt_anchor_rebases_on_row_delta(unified: bool) -> bool {
    !unified
}

/// Return the PromptEnd anchor as it exists on this pane's live surface now.
///
/// Command capture, reviewed-submission admission and polling, prompt status,
/// and click-to-place-cursor all route through this helper. The policy bit is
/// selected beside the render backend, so those security-relevant readers
/// cannot disagree about the beginning of the editable line.
fn prompt_anchor_for_surface(
    rebase_on_row_delta: bool,
    provisional: (i64, i64),
    recorded_rows: i64,
    current_rows: i64,
) -> (i64, i64) {
    if rebase_on_row_delta {
        rebase_prompt_anchor(provisional, recorded_rows, current_rows)
    } else {
        provisional
    }
}

fn visible_editor_text(vte: &Terminal, anchor: (i64, i64)) -> Option<String> {
    let (end_col, end_row) = vte.cursor_position();
    let (start_col, start_row) = anchor;
    if !command_capture_range_is_bounded(start_row, end_row, vte.column_count()) {
        return None;
    }
    if (start_row, start_col) == (end_row, end_col) {
        return Some(String::new());
    }
    vte.text_range_format(vte4::Format::Text, start_row, start_col, end_row, end_col)
        .0
        .map(|text| text.to_string())
}

/// Query-only live-surface seam used by the reviewed submission boundary and
/// prompt consumers that must share the render backend's anchor policy.
trait SubmissionSurface {
    fn cursor_position(&self) -> (i64, i64);
    fn row_count(&self) -> i64;
    fn prompt_anchor(&self, provisional: (i64, i64), recorded_rows: i64) -> (i64, i64);
    fn visible_editor_text(&self, anchor: (i64, i64)) -> Option<String>;
    fn suffix_is_empty(&self) -> Option<bool>;
}

struct VteSubmissionSurface {
    vte: Terminal,
    /// Selected beside the render backend: Block rebases across compact/full
    /// grid changes, while Unified keeps one stable viewport-sized grid.
    rebase_on_row_delta: bool,
}

impl SubmissionSurface for VteSubmissionSurface {
    fn cursor_position(&self) -> (i64, i64) {
        self.vte.cursor_position()
    }

    fn row_count(&self) -> i64 {
        self.vte.row_count()
    }

    fn prompt_anchor(&self, provisional: (i64, i64), recorded_rows: i64) -> (i64, i64) {
        prompt_anchor_for_surface(
            self.rebase_on_row_delta,
            provisional,
            recorded_rows,
            self.vte.row_count(),
        )
    }

    fn visible_editor_text(&self, anchor: (i64, i64)) -> Option<String> {
        visible_editor_text(&self.vte, anchor)
    }

    fn suffix_is_empty(&self) -> Option<bool> {
        crate::terminal::click_cursor::verified_suffix_is_empty(&self.vte)
    }
}

fn approved_command_submission_payload(command: &str) -> Result<Vec<u8>, String> {
    crate::review_input::validate(command)
        .map_err(|error| format!("rejected unsafe reviewed command: {error}"))?;
    if command != command.trim() {
        return Err(
            "reviewed execution rejects leading or trailing whitespace; insert it for manual review instead"
                .to_string(),
        );
    }
    Ok(command.as_bytes().to_vec())
}

/// Two-phase execution boundary shared by corrections and Shell Agent. It
/// inserts without Enter, reads the exact rendered editor back from VTE, then
/// admits Enter as a separate ordered write. Any lifecycle ambiguity loses the
/// Agent identity rather than binding a later block to the approval.
#[derive(Clone)]
struct VerifiedSubmissionCtx {
    surface: Rc<dyn SubmissionSurface>,
    bstate: Rc<Cell<BlockState>>,
    pty: Rc<OwnedPty>,
    typed_cmd: Rc<RefCell<String>>,
    idle_input_dirty: Rc<Cell<bool>>,
    pty_synced: Rc<Cell<bool>>,
    prompt_end_pos: Rc<Cell<(i64, i64)>>,
    prompt_anchor_rows: Rc<Cell<i64>>,
    prompt_anchor_ready: Rc<Cell<bool>>,
    prompt_generation: Rc<Cell<u64>>,
    contents_generation: Rc<Cell<u64>>,
    submission: Rc<RefCell<Option<ReviewedSubmission>>>,
    source_id: Rc<RefCell<Option<glib::SourceId>>>,
    armed_agent_execution: Rc<RefCell<Option<ArmedAgentExecution>>>,
    agent_execution_supported: Rc<Cell<bool>>,
    agent_execution_lost_callbacks: AgentExecutionLostCallbacks,
}

impl VerifiedSubmissionCtx {
    fn current_anchor(&self) -> (i64, i64) {
        self.surface
            .prompt_anchor(self.prompt_end_pos.get(), self.prompt_anchor_rows.get())
    }

    fn fail(&self, reason: &'static str) {
        let pending = self.submission.borrow_mut().take();
        self.armed_agent_execution.borrow_mut().take();
        if let Some(execution) = pending.and_then(|submission| submission.execution) {
            emit_agent_execution_lost(&self.agent_execution_lost_callbacks, execution, reason);
        } else {
            log::warn!("reviewed command verification failed closed: {reason}");
        }
    }

    fn cancel_if_pending(&self, reason: &'static str) -> bool {
        if self.submission.borrow().is_none() {
            return false;
        }
        if let Some(source) = self.source_id.borrow_mut().take() {
            source.remove();
        }
        self.fail(reason);
        true
    }

    fn arm_command_start_deadline(&self) {
        let ctx = self.clone();
        let source = glib::timeout_add_local_once(REVIEWED_COMMAND_START_TIMEOUT, move || {
            ctx.source_id.borrow_mut().take();
            let still_submitted =
                ctx.submission.borrow().as_ref().is_some_and(|submission| {
                    submission.phase == ReviewedSubmissionPhase::Submitted
                });
            if still_submitted {
                ctx.fail(REVIEWED_COMMAND_START_LOST);
            }
        });
        *self.source_id.borrow_mut() = Some(source);
    }

    fn begin(
        &self,
        command: &str,
        execution: Option<crate::agent::AgentExecutionRef>,
    ) -> Result<(), String> {
        let payload = approved_command_submission_payload(command)?;
        if execution.is_some() && !self.agent_execution_supported.get() {
            return Err(
                "Shell Agent execution requires the bundled token-aware bash/zsh integration"
                    .to_string(),
            );
        }
        if self.source_id.borrow().is_some() || self.submission.borrow().is_some() {
            return Err("another reviewed command is still being verified".to_string());
        }
        if self.bstate.get() != BlockState::AwaitingCommand
            || !self.prompt_anchor_ready.get()
            || self.idle_input_dirty.get()
            || self.pty_synced.get()
            || self.pty.shell_is_foreground() != Some(true)
        {
            return Err("the shell prompt is no longer verified empty".to_string());
        }
        let anchor = self.current_anchor();
        let cursor = self.surface.cursor_position();
        let suffix_is_empty = self.surface.suffix_is_empty();
        if cursor != anchor || suffix_is_empty != Some(true) {
            return Err("the shell prompt visibly contains input".to_string());
        }

        self.pty
            .try_write_bytes(&payload)
            .map_err(|error| error.to_string())?;
        *self.typed_cmd.borrow_mut() = command.to_string();
        self.idle_input_dirty.set(true);
        self.pty_synced.set(true);
        let prompt_generation = self.prompt_generation.get();
        *self.submission.borrow_mut() = Some(ReviewedSubmission {
            command: command.to_string(),
            execution,
            prompt_generation,
            phase: ReviewedSubmissionPhase::Inserting,
            identity_feed_tainted: false,
        });

        let ctx = self.clone();
        let command = command.to_string();
        let contents_before = self.contents_generation.get();
        let attempts = Rc::new(Cell::new(0_u32));
        let last_observed = Rc::new(Cell::new(None::<(u64, i64, i64)>));
        let stable_polls = Rc::new(Cell::new(0_u8));
        let source = glib::timeout_add_local(VERIFIED_SUBMISSION_POLL, move || {
            let attempt = attempts.get().saturating_add(1);
            attempts.set(attempt);
            let still_current = ctx.submission.borrow().as_ref().is_some_and(|submission| {
                submission.phase == ReviewedSubmissionPhase::Inserting
                    && submission.command == command
                    && submission.execution == execution
                    && submission.prompt_generation == prompt_generation
            });
            if !still_current
                || ctx.bstate.get() != BlockState::AwaitingCommand
                || !ctx.prompt_anchor_ready.get()
                || ctx.prompt_generation.get() != prompt_generation
                || ctx.pty.shell_is_foreground() != Some(true)
                || attempt >= VERIFIED_SUBMISSION_MAX_POLLS
            {
                ctx.source_id.borrow_mut().take();
                ctx.fail(VERIFIED_SUBMISSION_LOST);
                return glib::ControlFlow::Break;
            }

            let contents = ctx.contents_generation.get();
            if contents == contents_before {
                return glib::ControlFlow::Continue;
            }
            let (col, row) = ctx.surface.cursor_position();
            let observed = (contents, col, row);
            if last_observed.get() == Some(observed) {
                stable_polls.set(stable_polls.get().saturating_add(1));
            } else {
                last_observed.set(Some(observed));
                stable_polls.set(0);
                return glib::ControlFlow::Continue;
            }
            if stable_polls.get() < 1 {
                return glib::ControlFlow::Continue;
            }

            let rendered = ctx.surface.visible_editor_text(ctx.current_anchor());
            let suffix_empty = ctx.surface.suffix_is_empty();
            if rendered.as_deref() != Some(command.as_str()) || suffix_empty != Some(true) {
                ctx.source_id.borrow_mut().take();
                ctx.fail(VERIFIED_SUBMISSION_LOST);
                return glib::ControlFlow::Break;
            }

            if let Err(error) = ctx.pty.try_write_bytes(b"\r") {
                log::warn!("verified Enter was not queued: {error}");
                ctx.source_id.borrow_mut().take();
                ctx.fail(VERIFIED_SUBMISSION_LOST);
                return glib::ControlFlow::Break;
            }
            if let Some(submission) = ctx.submission.borrow_mut().as_mut() {
                submission.phase = ReviewedSubmissionPhase::Submitted;
            }
            if let Some(execution) = execution {
                *ctx.armed_agent_execution.borrow_mut() = Some(ArmedAgentExecution {
                    execution,
                    prompt_generation,
                });
            }
            ctx.source_id.borrow_mut().take();
            ctx.arm_command_start_deadline();
            glib::ControlFlow::Break
        });
        *self.source_id.borrow_mut() = Some(source);
        Ok(())
    }

    fn command_start_observed(
        &self,
        shell_command: Option<&str>,
        rendered_command: &str,
        trusted_id: bool,
    ) -> Option<crate::agent::AgentExecutionRef> {
        if let Some(source) = self.source_id.borrow_mut().take() {
            source.remove();
        }
        let Some(submission) = self.submission.borrow_mut().take() else {
            self.armed_agent_execution.borrow_mut().take();
            return None;
        };
        let identity_matches = submission.phase == ReviewedSubmissionPhase::Submitted
            && submission.prompt_generation == self.prompt_generation.get()
            && reviewed_submission_matches(
                shell_command,
                rendered_command,
                &submission.command,
                submission.identity_feed_tainted,
            );
        let armed = take_armed_agent_execution(
            &mut self.armed_agent_execution.borrow_mut(),
            self.prompt_generation.get(),
        );
        let Some(execution) = submission.execution else {
            if !identity_matches {
                log::warn!("reviewed command start did not match the verified insertion");
            }
            return None;
        };
        if identity_matches
            && trusted_id
            && self.pty.shell_is_foreground() == Some(true)
            && armed == Some(execution)
        {
            Some(execution)
        } else {
            emit_agent_execution_lost(
                &self.agent_execution_lost_callbacks,
                execution,
                "the shell command start could not be correlated to the approved command",
            );
            None
        }
    }
}

fn take_armed_agent_execution(
    armed: &mut Option<ArmedAgentExecution>,
    prompt_generation: u64,
) -> Option<crate::agent::AgentExecutionRef> {
    armed
        .take()
        .filter(|armed| armed.prompt_generation == prompt_generation)
        .map(|armed| armed.execution)
}

fn command_end_matches_started_id(started_id: Option<&str>, finished_id: Option<&str>) -> bool {
    started_id.is_some() && started_id == finished_id
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentCommandEndDecision {
    Accept,
    IgnoreUntilShellOwnsForeground,
    AcceptWithoutAgentCorrelation,
}

fn decide_agent_command_end(
    has_agent_execution: bool,
    shell_is_foreground: Option<bool>,
    trusted_matching_id: bool,
) -> AgentCommandEndDecision {
    if !has_agent_execution {
        return AgentCommandEndDecision::Accept;
    }
    if shell_is_foreground == Some(false) {
        return AgentCommandEndDecision::IgnoreUntilShellOwnsForeground;
    }
    if shell_is_foreground != Some(true) || !trusted_matching_id {
        return AgentCommandEndDecision::AcceptWithoutAgentCorrelation;
    }
    AgentCommandEndDecision::Accept
}

fn agent_prompt_boundary_is_trusted(
    active_execution: Option<crate::agent::AgentExecutionRef>,
    shell_is_foreground: Option<bool>,
) -> bool {
    active_execution.is_none() || shell_is_foreground == Some(true)
}

pub struct TermView {
    root: gtk::Box,
    block_scroll: ScrolledWindow,
    block_list: gtk::Box,
    /// Space-occupying card region below the surface. Used by a backend whose
    /// document cannot scroll to a mounted card; empty and hidden otherwise.
    notice_dock: gtk::Box,
    /// The single persistent live VTE (anvil model): prompt + typing + output all
    /// render here natively; finished commands snapshot into styled blocks above.
    active_vte: Terminal,
    active: Rc<RefCell<ActiveBlock>>,
    bstate: Rc<Cell<BlockState>>,
    /// Keystroke shadow used only to size the idle input cell (line count). The
    /// authoritative finished-command text is read off the live VTE at
    /// CommandStart, so this never has to round-trip to display.
    typed_cmd: Rc<RefCell<String>>,
    /// Immutable PromptEnd cursor anchor used for empty-editor verification.
    prompt_end_pos: Rc<Cell<(i64, i64)>>,
    /// Grid height observed beside `prompt_end_pos`; every consumer passes the
    /// pair through the selected surface's anchor policy.
    prompt_anchor_rows: Rc<Cell<i64>>,
    /// VTE feed is asynchronous; approval remains unavailable until a short
    /// post-PromptEnd fence confirms that no input raced the captured anchor.
    prompt_anchor_ready: Rc<Cell<bool>>,
    /// One-shot Agent execution identity armed atomically with its PTY write.
    /// It follows only the next command at the same prompt generation; the
    /// Agent session performs the secondary command-text check at completion.
    armed_agent_execution: Rc<RefCell<Option<ArmedAgentExecution>>>,
    /// True only after a token-aware integration announces the exact private
    /// token inside the current prompt boundary.
    agent_execution_supported: Rc<Cell<bool>>,
    verified_submission: VerifiedSubmissionCtx,
    /// Identity-verified Agent command currently owning the foreground block.
    active_agent_execution: Rc<Cell<Option<crate::agent::AgentExecutionRef>>>,
    /// Set once the user has started editing at an idle prompt, so shell echo is
    /// no longer mistaken for asynchronous background output. Shared with the
    /// reader loop; the clipboard paste path has to set it too, since a paste
    /// never travels through the VTE `commit` signal that normally does.
    idle_input_dirty: Rc<Cell<bool>>,
    /// True while an alt-screen app owns the viewport (finished blocks hidden).
    fullscreen: Rc<Cell<bool>>,
    /// True once the user has scrolled up off the live prompt; while false the
    /// view follows the bottom. Read by the per-frame tick to re-pin the prompt.
    user_scrolled_up: Rc<Cell<bool>>,
    /// Guards programmatic scrolls so the scroll-lock detector doesn't mistake
    /// them for a user drag.
    programmatic_scroll: Rc<Cell<bool>>,
    /// Set only by an app-level pane/tab selection. A later map may fulfill the
    /// request, but background Block panes never focus themselves.
    focus_requested: Rc<Cell<bool>>,
    pty: Rc<OwnedPty>,
    /// Whether programmatic recall has already synchronized the live shell
    /// editor. Shared with the key and block-action paths.
    pty_synced: Rc<Cell<bool>>,
    cwd_callbacks: CwdCallbacks,
    remote_session_callbacks: StrCallbacks,
    exited_callbacks: IntCallbacks,
    bell_callbacks: VoidCallbacks,
    title_callbacks: StrCallbacks,
    activity_callbacks: VoidCallbacks,
    human_input_callbacks: HumanInputCallbacks,
    alt_screen_callbacks: AltScreenCallbacks,
    command_started_callbacks: CommandStartedCallbacks,
    command_finished_callbacks: CommandFinishedCallbacks,
    block_finished_callbacks: BlockFinishedCallbacks,
    ask_ai_about_block_callbacks: BlockContextCallbacks,
    mouse_reporting_mode: Rc<Cell<MouseReportingMode>>,
    /// Whether the shell has enabled DECSET 2004. Clipboard input is written
    /// directly to our PTY, so block mode must apply this wrapper itself.
    bracketed_paste: Rc<Cell<bool>>,
    /// The pane's dynamic OSC 10/11/12 overrides, shared with the reader loop
    /// that records them. Read when rebuilding blocks (undo-clear must match
    /// the recolored live view) and cleared by an explicit theme change.
    dynamic_colors: DynamicColorsRc,
    config: Rc<RefCell<Config>>,
    block_data: Rc<RefCell<VecDeque<BlockData>>>,
    /// Persisted ids this pane's live allocator has not encountered yet. The
    /// loader replaces this bounded set transactionally after a full scan;
    /// ReaderCtx removes ids as the process-wide monotonic counter reaches them.
    reserved_history_block_ids: Rc<RefCell<HashSet<u64>>>,
    /// Queue a repaint whenever block metadata changes, even when GTK's scroll
    /// adjustment geometry happens to remain numerically identical.
    failure_marker_redraw: FailureMarkerRedraw,
    finished_blocks: Rc<RefCell<Vec<FinishedBlock>>>,
    viewport: Rc<RefCell<ViewportState>>,
    widget_pool: Rc<RefCell<WidgetPool>>,
    visible_indices: Rc<RefCell<std::collections::HashSet<usize>>>,
    selected_block_ids: SelectedBlockIds,
    selected_block_id: Rc<Cell<Option<u64>>>,
    /// Stepping cursor for a backend that mounts no finished widget, where
    /// `selected_block_id` — which only ever holds an id that passes the
    /// finished-block lookup — stays `None` for the pane's whole life. Holds
    /// the record this view last navigated to, so next/previous can advance.
    navigated_record_id: Cell<Option<u64>>,
    selection_anchor_id: Rc<Cell<Option<u64>>>,
    bookmarks: Rc<RefCell<std::collections::HashSet<u64>>>,
    /// Blocks removed by the most recent Clear Blocks, kept as data so an
    /// explicit undo can rebuild their widgets. Single-level: a later clear
    /// with content replaces it; cleared again only when consumed by undo.
    cleared_stash: RefCell<Vec<BlockData>>,
    /// Per-path load/save observations move with the pane and never depend on
    /// a transient allocation address.
    history_baselines: RefCell<HashMap<std::path::PathBuf, history::HistoryBaseline>>,
    /// Clear Blocks deletion authorities bound to resolved paths and codecs.
    /// Keep each ordered target armed until that exact replacement succeeds.
    history_explicit_replace_pending: RefCell<VecDeque<history::HistoryTarget>>,
    /// Number shown on the jump-to-latest affordance while history is scrolled
    /// away from the live prompt. Kept on TermView so Clear Blocks can reset
    /// all of the visible block-history state atomically.
    unread_count: Rc<Cell<u32>>,
    jump_fab: gtk::Button,
    /// Compact organism representation inside the sticky running header.
    sticky_organism_slot: gtk::Box,
    /// Find-within-blocks state: every match across the finished blocks plus a
    /// cursor into it, so Ctrl+F highlights all hits and Next/Prev step through
    /// them (Warp's FindWithinBlock). Tags are stripped on close via clear_find.
    find_state: Rc<RefCell<FindState>>,
    current_cwd: Rc<RefCell<String>>,
    /// Per-frame resize tick installed on `root`. Held so it can be removed on
    /// Drop — otherwise the callback runs forever and keeps its Rc captures
    /// (pty/active/vte/vte_box) alive past tab close.
    resize_tick_id: RefCell<Option<gtk::TickCallbackId>>,
    /// Periodic sticky-header refresh. Explicitly removed on tab close so its
    /// GTK captures cannot keep the detached block tree alive.
    sticky_timer_id: RefCell<Option<glib::SourceId>>,
    /// Tracks per-VTE selections so a drag that crosses block boundaries can be
    /// copied as one contiguous string via Ctrl+Shift+C.
    cross_selection: Rc<CrossSelection>,
    /// Defers raw PTY chunks while a user selection covers the streaming live
    /// VTE, then replays them through the original parser/state pipeline.
    selection_feed_hold: Rc<SelectionFeedHold>,
    /// Recompute live sizing and finished-card geometry after font or viewport
    /// metrics change. Keeping the closure here makes programmatic font updates
    /// follow the same refit path as GTK allocation signals.
    layout_active_surface: Rc<dyn Fn()>,
    /// The same backend instance driven by `ReaderCtx`; all completed-record
    /// consumers query this instead of inferring a mode from empty Block lists.
    render_backend: Rc<dyn RenderBackend>,
}

impl Drop for TermView {
    fn drop(&mut self) {
        if let Err(err) = self.save_history() {
            log::warn!("save block history on close: {err}");
        }
        // Outer gesture controllers capture pane-owned Rc state, while child
        // buttons/VTEs own their signal closures. Explicitly sever both sides
        // before the widget tree disappears so closing a pane cannot leave its
        // completed blocks, PTY handles, or VTE grids in a reference cycle.
        let finished_widgets: Vec<gtk::Box> = self
            .finished_blocks
            .borrow_mut()
            .drain(..)
            .map(|block| block.widget().clone())
            .collect();
        for widget in finished_widgets {
            self.block_list.remove(&widget);
            WidgetPool::teardown(&widget);
        }
        if let Some(id) = self.resize_tick_id.borrow_mut().take() {
            id.remove();
        }
        if let Some(id) = self.sticky_timer_id.borrow_mut().take() {
            id.remove();
        }
        if let Some(id) = self.verified_submission.source_id.borrow_mut().take() {
            id.remove();
        }
    }
}

/// Reader-pipeline state private to this pane's PTY event dispatch: nothing
/// outside [`ReaderCtx`] reads or writes these, so they live as plain values
/// behind one `RefCell`. Borrows must stay statement- or tight-block-scoped —
/// never held across the finalize path, a callback fan-out, a layout/PTY-sync
/// call, or a VTE feed.
struct EngineState {
    /// State to restore when an alt-screen app exits (anvil model).
    prev_state: BlockState,
    osc133_depth: u32,
    prompt_buf: String,
    /// Bytes emitted asynchronously after PromptEnd and before the next PromptStart.
    /// Empty-command blocks are inferred from this separate buffer, so no history
    /// schema change is needed.
    background_output: VecDeque<u8>,
    /// Whether [`MAX_RAW_OUTPUT_BYTES`] already discarded bytes off the front
    /// of `background_output`. The retained tail cannot show this, so it is
    /// tracked beside the bytes it describes and reset with them.
    background_output_dropped_front: bool,
    /// Command text read from the live VTE at CommandStart; primary source
    /// for the finished block.
    vte_typed_cmd: String,
    /// Rendered prompt (last non-empty line) captured at PromptEnd, used by the
    /// finalize path since prompt_buf is cleared once the prompt ends.
    prompt_display: String,
    /// Status from the shell's OSC 133 `D` packet. `None` means the shell did
    /// not report one — not that the command succeeded.
    pending_exit_code: Option<i32>,
    /// Duration the shell measured for the running command, when it sends one.
    /// Beats `block_start_time_for_cb`, which starts when this process noticed
    /// the mark.
    shell_duration_ms: Option<u64>,
    execution_id_trusted: bool,
    agent_completion_trusted: bool,
    /// cwd the running command was started in, as the shell reported it at
    /// CommandStart. The pane's tracked cwd has already moved on after a `cd`.
    command_cwd: Option<String>,
    /// The id opened at A and carried through C until the completed record is
    /// finalized at the following A. Opening a marker alone never creates a
    /// record.
    pending_zone: Option<PendingZone>,
    active_alt_screen_mode: Option<u32>,
}

/// The VTE/widget pair that presents one field of a completed record.
struct RecordSearchTarget {
    terminal: Terminal,
    widget: gtk::Widget,
    /// Unified deliberately cannot return a record-specific target until
    /// marker ranges exist. Kept explicit for Block's ordinary card targets.
    uses_live_surface: bool,
}

/// One bounded window inside a native VTE search domain. Windows are ordered
/// exactly as a freshly reset native cursor visits them. Unified presents the
/// viewport-to-tail window first and the wrapped oldest-history prefix second.
struct BackendSearchWindow {
    text: String,
    /// Counting this window does not prove the domain's total match count.
    incomplete: bool,
    /// The native step entering this window must wrap around once.
    initial_wrap: bool,
}

/// One native VTE search domain and bounded plain-text windows used to count
/// its matches. Unified returns exactly one domain for its persistent surface.
struct BackendSearchSurface {
    block_id: u64,
    block_index: usize,
    is_output: bool,
    is_live: bool,
    windows: Vec<BackendSearchWindow>,
    /// Hard-budget charge for extracting every window.
    scanned_bytes: usize,
    /// Clear the native selection/search anchor before entering the selected
    /// window. Unified shares one persistent cursor across successive queries.
    reset_cursor: bool,
    terminal: Terminal,
}

/// Last-resort native search for a persistent surface whose absolute ring
/// rows are not currently trustworthy. It can prove one selected result but
/// cannot claim a total or navigate beyond that representative result.
struct BackendNativeSearchFallback {
    block_id: u64,
    block_index: usize,
    is_output: bool,
    is_live: bool,
    terminal: Terminal,
}

/// One bounded backend snapshot. `incomplete` also covers a deadline reached
/// between surfaces (including before the first), where no individual surface
/// exists on which to carry the partial-state bit.
struct BackendSearchBatch {
    surfaces: Vec<BackendSearchSurface>,
    incomplete: bool,
    native_fallback: Option<BackendNativeSearchFallback>,
}

/// Intersect mounted record ids with one page of search candidates. Walking
/// the document once avoids the former per-hit `record_search_target` lookup,
/// which rescanned the mounted block vector for every rendered result row.
fn mounted_jumpable_records(
    mounted_ids: impl IntoIterator<Item = u64>,
    candidates: &HashSet<(u64, bool)>,
) -> HashSet<(u64, bool)> {
    let mut jumpable = HashSet::with_capacity(candidates.len());
    for block_id in mounted_ids {
        for is_output in [false, true] {
            let candidate = (block_id, is_output);
            if candidates.contains(&candidate) {
                jumpable.insert(candidate);
            }
        }
    }
    jumpable
}

/// Materialize one surface only while the caller's deadline remains live. A
/// post-check keeps the just-created surface usable while preventing another
/// potentially allocating read after the deadline.
fn push_search_surface_before_deadline<T>(
    surfaces: &mut Vec<T>,
    deadline_exhausted: &mut dyn FnMut() -> bool,
    materialize: impl FnOnce() -> T,
) -> bool {
    if deadline_exhausted() {
        return false;
    }
    surfaces.push(materialize());
    !deadline_exhausted()
}

/// Borrowed completed-record storage. Block owns serializable `BlockData`;
/// Unified owns command identity/outcome metadata plus the bounded snapshot
/// table beside it.
enum BackendRecords<'a> {
    Blocks(Ref<'a, VecDeque<BlockData>>),
    Metadata(Ref<'a, UnifiedZoneStore>),
}

#[derive(Clone, Copy)]
enum BackendRecordRef<'a> {
    Block(&'a BlockData),
    Metadata {
        record: &'a CompletedCommandRecord,
        snapshot: Option<&'a ZoneOutputSnapshot>,
    },
}

enum BackendRecordIter<'a> {
    Blocks(std::collections::vec_deque::Iter<'a, BlockData>),
    Metadata {
        records: std::collections::vec_deque::Iter<'a, CompletedCommandRecord>,
        store: &'a UnifiedZoneStore,
    },
}

impl<'a> Iterator for BackendRecordIter<'a> {
    type Item = BackendRecordRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Blocks(records) => records.next().map(BackendRecordRef::Block),
            Self::Metadata { records, store } => {
                records.next().map(|record| BackendRecordRef::Metadata {
                    record,
                    snapshot: store.snapshot(record.id),
                })
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Blocks(records) => records.size_hint(),
            Self::Metadata { records, .. } => records.size_hint(),
        }
    }
}

impl ExactSizeIterator for BackendRecordIter<'_> {}

impl DoubleEndedIterator for BackendRecordIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self {
            Self::Blocks(records) => records.next_back().map(BackendRecordRef::Block),
            Self::Metadata { records, store } => {
                records
                    .next_back()
                    .map(|record| BackendRecordRef::Metadata {
                        record,
                        snapshot: store.snapshot(record.id),
                    })
            }
        }
    }
}

impl BackendRecords<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Blocks(records) => records.len(),
            Self::Metadata(store) => store.records.len(),
        }
    }

    fn iter(&self) -> BackendRecordIter<'_> {
        match self {
            Self::Blocks(records) => BackendRecordIter::Blocks(records.iter()),
            Self::Metadata(store) => BackendRecordIter::Metadata {
                records: store.records.iter(),
                store,
            },
        }
    }

    fn block_data(&self) -> Option<&VecDeque<BlockData>> {
        match self {
            Self::Blocks(records) => Some(records),
            Self::Metadata(_) => None,
        }
    }
}

impl<'a> BackendRecordRef<'a> {
    fn id(self) -> u64 {
        match self {
            Self::Block(record) => record.id,
            Self::Metadata { record, .. } => record.id,
        }
    }

    fn command(self) -> &'a str {
        match self {
            Self::Block(record) => &record.cmd,
            Self::Metadata { record, .. } => &record.cmd,
        }
    }

    fn prompt(self) -> Option<&'a str> {
        match self {
            Self::Block(record) => Some(&record.prompt),
            Self::Metadata { .. } => None,
        }
    }

    /// Completed plain-text output. For a metadata record this is the bounded
    /// finalize-time snapshot; `None` means no output is retained for it —
    /// never an empty stand-in string.
    fn output(self) -> Option<&'a str> {
        match self {
            Self::Block(record) => Some(&record.output),
            Self::Metadata { snapshot, .. } => snapshot.map(|snapshot| snapshot.plain.as_str()),
        }
    }

    fn exit_code(self) -> Option<i32> {
        match self {
            Self::Block(record) => record.exit_code,
            Self::Metadata { record, .. } => record.exit_code,
        }
    }

    fn duration_ms(self) -> Option<u64> {
        match self {
            Self::Block(record) => record.duration_ms,
            Self::Metadata { record, .. } => record.duration_ms,
        }
    }

    fn is_metadata_only(self) -> bool {
        matches!(self, Self::Metadata { .. })
    }
}

/// Output stays in its existing capture owner until a backend or explicit
/// consumer requests the render payload.
enum CapturedFinalizeOutput {
    Foreground(Rc<RefCell<VecDeque<u8>>>),
    Background(VecDeque<u8>),
}

struct BlockRenderPayload {
    prompt: String,
    output_with_ansi: String,
    output_plain: String,
}

#[inline]
fn into_payload_plain_output(stripped: String) -> String {
    stripped
}

fn materialize_plain_output(output_with_ansi: &str) -> String {
    // `strip_ansi` already returns an owned String. Returning it directly
    // transfers that allocation into the payload; calling `to_string()` on it
    // would allocate and copy the complete (up to 8 MiB) output a second time.
    into_payload_plain_output(strip_ansi(output_with_ansi))
}

#[cfg(test)]
fn materialize_plain_output_legacy(output_with_ansi: &str) -> String {
    strip_ansi(output_with_ansi).to_string()
}

/// Object-safe, memoized accessor handed to every backend. Block calls
/// `materialize`; Unified calls only the bounded `output_snapshot`, which
/// never builds or memoizes the full card payload.
trait BlockRenderPayloadAccessor {
    fn materialize(&self) -> &BlockRenderPayload;

    /// Bounded plain-text TAIL of the captured output: at most `max_bytes`
    /// raw bytes are decoded and ANSI-stripped, and `truncated` reports any
    /// byte of the command's output that did not survive into the text.
    /// `None` when the capture is empty or strips to nothing — an absent
    /// snapshot must never be presented as an empty one. Reads the same
    /// engine-owned ring `materialize` consumes, which the engine keeps alive
    /// until the finalize fan-out completes.
    fn output_snapshot(&self, max_bytes: usize) -> Option<ZoneOutputSnapshot>;

    #[cfg(test)]
    fn materialization_counter(&self) -> Rc<Cell<usize>>;
}

struct LazyBlockRenderPayload {
    value: OnceCell<BlockRenderPayload>,
    prompt: RefCell<Option<String>>,
    output: RefCell<Option<CapturedFinalizeOutput>>,
    /// The capture's wrap marker, read from the engine before construction.
    /// Whoever materializes first consumes the ring, and the decoded text
    /// carries no trace of the bytes that fell out of its front.
    dropped_front: bool,
    materializations: Rc<Cell<usize>>,
}

impl LazyBlockRenderPayload {
    fn new(prompt: String, output: CapturedFinalizeOutput, dropped_front: bool) -> Self {
        Self {
            value: OnceCell::new(),
            prompt: RefCell::new(Some(prompt)),
            output: RefCell::new(Some(output)),
            dropped_front,
            materializations: Rc::new(Cell::new(0)),
        }
    }

    #[cfg(test)]
    fn materialization_count(&self) -> usize {
        self.materializations.get()
    }
}

impl BlockRenderPayloadAccessor for LazyBlockRenderPayload {
    fn materialize(&self) -> &BlockRenderPayload {
        self.value.get_or_init(|| {
            self.materializations
                .set(self.materializations.get().saturating_add(1));
            let prompt = self
                .prompt
                .borrow_mut()
                .take()
                .expect("a finalize payload is materialized at most once");
            let output_with_ansi = match self
                .output
                .borrow_mut()
                .take()
                .expect("a finalize payload is materialized at most once")
            {
                CapturedFinalizeOutput::Foreground(output) => live_output_text(&output),
                CapturedFinalizeOutput::Background(mut output) => {
                    String::from_utf8_lossy(output.make_contiguous()).into_owned()
                }
            };
            let output_plain = materialize_plain_output(&output_with_ansi);
            BlockRenderPayload {
                prompt,
                output_with_ansi,
                output_plain,
            }
        })
    }

    fn output_snapshot(&self, max_bytes: usize) -> Option<ZoneOutputSnapshot> {
        // The jsh journal submission runs before backend finalize and
        // materializes the payload, consuming the ring; under jsh that is the
        // path every foreground command takes. The memoized plain text is the
        // same stream decoded, so the tail bound applies to it instead, and
        // the ring's wrap marker is carried separately because it does not
        // survive into that text.
        if let Some(value) = self.value.get() {
            return zone_output_snapshot_from_plain(
                &value.output_plain,
                max_bytes,
                self.dropped_front,
            );
        }
        let mut output = self.output.borrow_mut();
        match output.as_mut()? {
            CapturedFinalizeOutput::Foreground(ring) => zone_output_snapshot_from_ring(
                &mut ring.borrow_mut(),
                self.dropped_front,
                max_bytes,
            ),
            CapturedFinalizeOutput::Background(ring) => {
                zone_output_snapshot_from_ring(ring, self.dropped_front, max_bytes)
            }
        }
    }

    #[cfg(test)]
    fn materialization_counter(&self) -> Rc<Cell<usize>> {
        self.materializations.clone()
    }
}

/// Rendering seam for the OSC 133 block lifecycle. Every statement in the
/// [`ReaderCtx`] handlers that touches a widget-owning handle goes through
/// one of these effect methods, and every widget/surface read that feeds a
/// lifecycle decision is a query method. [`BlockBackend`] is the GTK/VTE
/// implementation; a second implementation (a future unified surface, or a
/// recording stub in tests) reuses the whole lifecycle unchanged.
///
/// Effects must not re-enter the reader dispatch: engine-side `RefCell`
/// borrows are never held across a call into this trait (the discipline the
/// [`EngineState`] doc states).
trait RenderBackend {
    // ── live surface ──
    /// Feed bytes to the live terminal surface (including re-synthesized
    /// `\x1b[?..h/l` alt-screen toggles).
    fn feed_live(&self, bytes: &[u8]);
    /// Open or reassert the prompt zone chosen by the engine at OSC 133 `A`.
    /// Block has no persistent marker surface, so this is a no-op by default.
    fn begin_prompt_zone(&self, _zone_id: u64) {}
    /// Close the A→C prompt marker before command output begins. Backends
    /// without marker-based ranges leave the default no-op in place.
    fn close_prompt_zone(&self, _zone_id: Option<u64>) {}
    /// Invalidate row-address authority immediately before ED3 reaches VTE.
    fn erase_scrollback(&self) {}
    /// Invalidate persistent-surface authority before RIS reaches VTE.
    fn hard_reset(&self) {}
    /// Reset the live surface for the next prompt. `preserve_scrollback` is
    /// Block-surface mechanics (the engine passes the config knob through; a
    /// backend without a persistent live scrollback may ignore it). Does NOT
    /// touch the running command's output snapshot: the raw-output ring is
    /// engine-owned shared state and the engine clears it explicitly.
    fn reset_active_surface(&self, preserve_scrollback: bool);
    /// Focus the live surface once the current main-loop turn finishes, but
    /// only if it already holds focus — the guard is a surface query, so it
    /// belongs on this side of the seam with the effect it gates.
    fn focus_live_deferred(&self);
    /// Lay out the live surface and push the viewport grid to the PTY
    /// synchronously (TIOCSWINSZ before the child's next read).
    fn sync_geometry_to_pty(&self);
    /// Re-run only the live-surface layout (compact vs full-screen).
    fn layout_active_surface(&self);
    // ── completed-record document ──
    fn records(&self) -> BackendRecords<'_>;
    /// Resolve the exact visible surface for one record field. Unified returns
    /// `None` until command markers provide a truthful per-record range.
    fn record_search_target(&self, block_id: u64, is_output: bool) -> Option<RecordSearchTarget>;
    /// Scroll the backend's document to the named completed record, returning
    /// whether an exact, proven location was shown. `false` obliges the caller
    /// to fall back honestly (snapshot view, notice); an implementation must
    /// never scroll to a guessed row.
    fn scroll_to_record(&self, _block_id: u64) -> bool {
        false
    }
    /// Whether [`Self::scroll_to_record`] would find an exact location for
    /// this record right now. Answers from the same proof and moves nothing:
    /// no scroll, no focus, no selection.
    fn can_scroll_to_record(&self, _block_id: u64) -> bool {
        false
    }
    /// Resolve one result page without moving focus or scroll state. The
    /// default retains backend-specific lookup/proof semantics; mounted-card
    /// backends override it to avoid one document scan per candidate.
    fn jumpable_records(&self, candidates: &HashSet<(u64, bool)>) -> HashSet<(u64, bool)> {
        candidates
            .iter()
            .copied()
            .filter(|(block_id, is_output)| {
                self.record_search_target(*block_id, *is_output).is_some()
                    || self.can_scroll_to_record(*block_id)
            })
            .collect()
    }
    /// Snapshot completed text into native search-cursor domains under one
    /// aggregate byte/work ceiling.
    fn completed_search_surfaces(
        &self,
        max_bytes: usize,
        deadline_exhausted: &mut dyn FnMut() -> bool,
    ) -> BackendSearchBatch;
    /// Whether this backend owns the persisted Block-card format. A backend
    /// answering `false` must implement the bounded zone-replay pair below, or
    /// it retains nothing across a restart.
    fn persists_block_history(&self) -> bool {
        true
    }
    /// Bounded, replayable form of this backend's completed zones, newest
    /// last. `None` from a backend whose history is the Block card document.
    fn zone_replay_snapshot(
        &self,
        _max_zones: usize,
        _max_bytes: usize,
    ) -> Option<Vec<zone_history::PersistedZone>> {
        None
    }
    /// Replay a restored session onto the surface and adopt its records,
    /// returning how many zones landed. Ids are issued from this process's
    /// counter: a persisted id must never re-enter marker authority.
    fn replay_zone_snapshot(&self, _zones: Vec<zone_history::PersistedZone>) -> usize {
        0
    }
    fn supports_inline_notices(&self) -> bool {
        true
    }
    /// Whether cards belong in the space-occupying bottom dock instead of the
    /// scrollable document. True for a surface that owns the whole viewport,
    /// where a card in the document exists at a position nothing can reach.
    fn docks_inline_notices(&self) -> bool {
        false
    }
    fn supports_block_mutation(&self) -> bool {
        true
    }
    fn scroll_surface_lines(&self, _lines: i32) -> bool {
        false
    }
    fn debug_name(&self) -> &'static str;
    // ── autoscroll ──
    fn mark_scroll_dirty(&self);
    fn reset_scroll_lock(&self);
    // ── finished blocks ──
    /// Mount a finished command as a history block and reset the live
    /// surface; subsumes the whole ordered finalize sub-step chain.
    fn finalize_block(
        &self,
        record: &CompletedCommandRecord,
        payload: &dyn BlockRenderPayloadAccessor,
    );
    // ── alt-screen chrome ──
    fn enter_alt_screen_chrome(&self);
    fn exit_alt_screen_chrome(&self);
    fn enter_fullscreen(&self);
    fn exit_fullscreen(&self);
    // ── kitty graphics (APC G) ──
    /// Assemble one APC payload. On `Complete` the decoded texture stays
    /// backend-side as the pending admission — textures never cross this
    /// trait — and any previous pending admission is dropped first, so an
    /// admit the engine skipped cannot leak across events. The caller must
    /// write the protocol reply before consuming a `Complete` via
    /// [`Self::kitty_admit_pending`] — clients block on the `i=`-keyed answer.
    /// That reply-between-feed-and-admit ordering is engine-side and holds for
    /// any implementation, including a headless recording one.
    fn kitty_feed(&self, payload: &[u8]) -> kitty_graphics::FeedStatus;
    /// Admit the texture parked by the last `Complete` [`Self::kitty_feed`]
    /// against the per-block image budget. No-op when nothing is pending.
    fn kitty_admit_pending(&self);
    /// Drop half-uploaded chunks and undisplayed images (the empty-command
    /// bail's parity with what `reset_active` used to imply).
    fn reset_kitty_pipeline(&self);
    // ── system ──
    fn set_system_clipboard(&self, text: &str);
    fn desktop_notify(&self, title: Option<&str>, body: &str);
    // ── prompt-anchor settling ──
    /// Schedule the surface-coupled delay that publishes a PromptEnd anchor.
    /// Keeping this behind the backend makes the lifecycle independently
    /// driveable in tests without changing the production settling policy.
    fn schedule_anchor_settle(&self, args: AnchorSettleArgs);
    // ── queries ──
    /// Current cursor position `(col, row)` and live-grid height, sampled
    /// together so the PromptEnd pair describes one surface frame. Rows are
    /// text-buffer (ring) rows, not screen-relative rows.
    fn cursor_and_rows(&self) -> ((i64, i64), i64);
    /// Cursor position `(col, row)` for the DSR 6 cursor-position report
    /// (`ESC[{row+1};{col+1}R`). Block reports the text-buffer row here
    /// (pre-existing quirk kept for compatibility); a correct implementation
    /// reports screen-relative coordinates as the CPR protocol expects.
    fn cursor_position_report(&self) -> (i64, i64);
    /// Rebase the prompt anchor captured at PromptEnd (`provisional`) onto the
    /// surface as it stands at CommandStart. Anchor cells are in the backend's
    /// surface coordinates and each backend owns its rebase policy. Block's
    /// Block rebases by the row-count delta because `layout_active_surface`
    /// changes the live VTE between compact and full grids. Unified keeps one
    /// full-size surface and therefore returns the provisional anchor unchanged.
    ///
    /// The same policy is exposed by [`SubmissionSurface::prompt_anchor`] for
    /// reviewed submission, status and click-to-place-cursor reads.
    fn command_capture_anchor(&self, provisional: (i64, i64), recorded_rows: i64) -> (i64, i64);
    /// The column count finished blocks pre-wrap at (live grid, floored).
    fn grid_cols(&self) -> i64;
    /// The live surface's actual column count. Command-capture bounds use this
    /// rather than the independently floored finished-block width.
    fn live_column_count(&self) -> i64;
    /// Plain text between two grid positions, `None` when the surface has
    /// nothing there. Bounding/normalizing the result stays engine-side.
    fn capture_text_range(
        &self,
        start_row: i64,
        start_col: i64,
        end_row: i64,
        end_col: i64,
    ) -> Option<String>;
}

/// Everything the production PromptEnd settling delay needs. Values and cells
/// are captured engine-side before scheduling; a recording backend may publish
/// `ready` synchronously while the GTK backends retain the real delay.
struct AnchorSettleArgs {
    prompt_generation: u64,
    state: Rc<Cell<BlockState>>,
    dirty: Rc<Cell<bool>>,
    synced: Rc<Cell<bool>>,
    generation: Rc<Cell<u64>>,
    ready: Rc<Cell<bool>>,
}

fn schedule_prompt_anchor_settle(args: AnchorSettleArgs) {
    glib::timeout_add_local_once(std::time::Duration::from_millis(32), move || {
        if args.state.get() == BlockState::AwaitingCommand
            && args.generation.get() == args.prompt_generation
            && !args.dirty.get()
            && !args.synced.get()
        {
            args.ready.set(true);
        }
    });
}

/// The GTK/VTE implementation of [`RenderBackend`] for Block mode: owns the
/// widget-owning handles the reader lifecycle drives. Shared-`Rc` fields keep
/// the `_rc`/`_for_cb` names of the former [`ReaderCtx`] fields they moved
/// from. The lifecycle cells at the bottom are the same `Rc` cells
/// `ReaderCtx` holds — finished-block actions and the context menu rebind
/// them when the user re-runs a command from history.
struct BlockBackend {
    active_rc: Rc<RefCell<ActiveBlock>>,
    /// The live VTE — every byte is fed here; alt-screen toggles feed it 1049h/l.
    active_vte: Terminal,
    /// Shared construction-time anchor policy; the submission surface receives
    /// this same value so no prompt reader can bypass the backend decision.
    rebase_prompt_anchor_on_row_delta: bool,
    block_list_rc: gtk::Box,
    block_scroll_rc: ScrolledWindow,
    jump_fab: gtk::Button,
    scroll_debouncer: ScrollDebouncer,
    failure_marker_redraw: FailureMarkerRedraw,
    finished_blocks_for_cb: Rc<RefCell<Vec<FinishedBlock>>>,
    widget_pool_for_cb: Rc<RefCell<WidgetPool>>,
    find_state_rc: Rc<RefCell<FindState>>,
    visible_indices_rc: Rc<RefCell<std::collections::HashSet<usize>>>,
    fullscreen_rc: Rc<Cell<bool>>,
    selected_block_ids_rc: SelectedBlockIds,
    selected_block_id_rc: Rc<Cell<Option<u64>>>,
    selection_anchor_id_rc: Rc<Cell<Option<u64>>>,
    bookmarks_rc: Rc<RefCell<std::collections::HashSet<u64>>>,
    block_data_for_cb: Rc<RefCell<VecDeque<BlockData>>>,
    unread_count_rc: Rc<Cell<u32>>,
    /// Switches the live surface between compact prompt and full-screen
    /// layouts. PTY geometry is deliberately synchronized separately.
    layout_active_surface: Rc<dyn Fn()>,
    /// Same shared cell as the `ReaderCtx` clone (see the config-aliasing
    /// invariants at the cell's construction).
    config_for_cb: Rc<RefCell<Config>>,
    /// Written engine-side (OSC 10/11/12 set/reset), read at finalize so a
    /// finished block matches a recolored live VTE.
    dynamic_colors_rc: DynamicColorsRc,
    /// Same PTY as the `ReaderCtx` clone: geometry sync and finished-block
    /// re-run writes here, protocol replies and foreground queries engine-side.
    pty_for_init: Rc<OwnedPty>,
    /// Menu-only ("Ask AI About Block"); the block-finished and
    /// agent-execution-lost fan-outs are engine policy and live on
    /// [`ReaderCtx`] alone.
    ask_ai_about_block_cbs: BlockContextCallbacks,
    // Lifecycle cells rebound by finished-block actions / the context menu.
    bstate_rc: Rc<Cell<BlockState>>,
    typed_cmd_rc: Rc<RefCell<String>>,
    armed_agent_execution_rc: Rc<RefCell<Option<ArmedAgentExecution>>>,
    bracketed_paste_rc: Rc<Cell<bool>>,
    pty_synced_rc: Rc<Cell<bool>>,
    /// Kitty graphics (APC G) — multi-chunk uploads assemble here; completed
    /// textures wait against the running command until its block finishes.
    /// The byte counter enforces the shared per-block budget so a runaway
    /// shell cannot balloon RSS between prompts. Backend methods are the only
    /// users post-split, so the group lives here as plain cells.
    kitty_assembler: RefCell<kitty_graphics::Assembler>,
    kitty_pending_images: RefCell<Vec<gtk::gdk::Texture>>,
    kitty_pending_bytes: Cell<usize>,
    /// Texture decoded by the last `Complete` `kitty_feed`, parked until the
    /// engine has written the protocol reply and calls `kitty_admit_pending`.
    /// Kept backend-side so `gdk::Texture` never crosses [`RenderBackend`];
    /// cleared at the start of every feed, by `reset_kitty_pipeline`, and in
    /// finalize's kitty tail, so a skipped admit cannot leak a stale texture
    /// across events.
    kitty_pending_admission: RefCell<Option<gtk::gdk::Texture>>,
}

/// Per-zone snapshot ceiling: finalize reads at most this many raw bytes from
/// the TAIL of the engine's output ring, and the stripped text it retains is
/// bounded by the same figure. A longer output loses its front, never its
/// middle.
const MAX_ZONE_SNAPSHOT_BYTES: usize = 64 * 1024;

/// Aggregate ceiling across every retained zone snapshot in one pane. Past it
/// the OLDEST snapshots are evicted first; their records stay, and a record
/// whose snapshot is gone honestly reports no output again.
const MAX_TOTAL_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;

/// Bounded plain-text tail of one completed command's output, captured at
/// finalize from the engine-owned raw ring. Pre-injector PTY bytes only: zone
/// marker OSC 8 frames are a separate terminal feed and never enter engine
/// capture, so they can never appear here.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ZoneOutputSnapshot {
    plain: String,
    /// Whether any byte of this command's output failed to survive into
    /// `plain`: the per-zone tail cut, or a ring the raw-output bound had
    /// already wrapped before finalize.
    truncated: bool,
}

/// Unified's completed-command document: chronological metadata records plus
/// bounded output snapshots keyed by record id. One cell holds both so a
/// records borrow can resolve a record's snapshot without a second borrow.
/// Snapshots are strictly optional satellites — every mutation path removes
/// snapshot bytes without touching the record they belonged to.
struct UnifiedZoneStore {
    records: VecDeque<CompletedCommandRecord>,
    snapshots: HashMap<u64, ZoneOutputSnapshot>,
    /// Running total of the retained snapshots' `plain` byte lengths, the
    /// quantity [`MAX_TOTAL_SNAPSHOT_BYTES`] bounds.
    snapshot_bytes: usize,
}

impl UnifiedZoneStore {
    fn new() -> Self {
        Self {
            records: VecDeque::new(),
            snapshots: HashMap::new(),
            snapshot_bytes: 0,
        }
    }

    fn snapshot(&self, id: u64) -> Option<&ZoneOutputSnapshot> {
        self.snapshots.get(&id)
    }

    fn insert_snapshot(&mut self, id: u64, snapshot: ZoneOutputSnapshot) {
        self.remove_snapshot(id);
        self.snapshot_bytes = self.snapshot_bytes.saturating_add(snapshot.plain.len());
        self.snapshots.insert(id, snapshot);
    }

    fn remove_snapshot(&mut self, id: u64) {
        if let Some(snapshot) = self.snapshots.remove(&id) {
            self.snapshot_bytes = self.snapshot_bytes.saturating_sub(snapshot.plain.len());
        }
    }

    /// Shed snapshot text, oldest record first, until the aggregate fits
    /// `max_total_bytes`. Records are never removed here: eviction only ever
    /// takes bytes, so the newest commands keep their output.
    fn enforce_snapshot_budget(&mut self, max_total_bytes: usize) {
        if self.snapshot_bytes <= max_total_bytes {
            return;
        }
        let ids: Vec<u64> = self.records.iter().map(|record| record.id).collect();
        for id in ids {
            if self.snapshot_bytes <= max_total_bytes {
                break;
            }
            self.remove_snapshot(id);
        }
    }
}

/// What mounting a card in the bottom dock must do, given where the widget is
/// parented right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DockMount {
    /// Unparented: the dock takes it.
    Append,
    /// Already docked. Re-pinning exists to keep a card beside the prompt in a
    /// scrolling document; the dock is always beside the prompt, so leave it.
    Keep,
    /// Parented elsewhere. Appending would reparent a widget the other region
    /// still owns, so refuse and let the caller fall back.
    Refuse,
}

fn dock_mount_decision(parent: Option<&gtk::Widget>, dock: &gtk::Widget) -> DockMount {
    match parent {
        None => DockMount::Append,
        Some(parent) if parent == dock => DockMount::Keep,
        Some(_) => DockMount::Refuse,
    }
}

/// Append a completed record to the Unified zone table, dropping the oldest
/// entries past `max_zones`. A drained record takes its snapshot with it.
fn record_unified_zone(
    zones: &mut UnifiedZoneStore,
    record: CompletedCommandRecord,
    max_zones: usize,
) -> Vec<u64> {
    zones.records.push_back(record);
    let max_zones = max_zones.max(1);
    if zones.records.len() > max_zones {
        let drained: Vec<u64> = zones
            .records
            .drain(..zones.records.len() - max_zones)
            .map(|record| record.id)
            .collect();
        for id in &drained {
            zones.remove_snapshot(*id);
        }
        return drained;
    }
    Vec::new()
}

/// TAIL cut of raw captured bytes. The cut may land inside an escape sequence
/// whose introducer lies before it; resuming after that sequence keeps its
/// parameter bytes from surfacing as literal text. UTF-8 continuation bytes at
/// the cut are skipped for the same reason. Returns the tail and whether
/// anything was cut.
fn bounded_output_tail(bytes: &[u8], max_bytes: usize) -> (&[u8], bool) {
    if bytes.len() <= max_bytes {
        return (bytes, false);
    }
    let mut start = bytes.len() - max_bytes;
    // The last ESC before the cut is the only sequence that can contain it:
    // these skippers never nest, and a later ESC would have ended it. The skip
    // must never consume the whole buffer — an OSC terminated on the final
    // byte, or never terminated at all, would otherwise leave an empty tail,
    // which is indistinguishable from a command that printed nothing.
    if let Some(esc) = memchr::memrchr(0x1b, &bytes[..start]) {
        let end = ansi::skip_escape_sequence(bytes, esc);
        if end > start && end < bytes.len() {
            start = end;
        }
    }
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }
    (&bytes[start..], true)
}

/// The same cut with no escape-sequence adjustment: the fallback for a tail
/// whose adjusted form strips to nothing. Literal parameter characters are a
/// poorer record than clean text, but a far better one than no snapshot.
fn unadjusted_output_tail(bytes: &[u8], max_bytes: usize) -> &[u8] {
    let mut start = bytes.len().saturating_sub(max_bytes);
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }
    &bytes[start..]
}

/// Bounded snapshot from the raw ring — the metadata (Unified) finalize path.
/// `dropped_front` is the ring's own wrap marker, which the retained bytes
/// cannot carry. The ring stays usable afterwards: `make_contiguous`
/// rearranges in place, and the engine clears the ring once fan-out completes.
fn zone_output_snapshot_from_ring(
    ring: &mut VecDeque<u8>,
    dropped_front: bool,
    max_bytes: usize,
) -> Option<ZoneOutputSnapshot> {
    if ring.is_empty() {
        return None;
    }
    let bytes = ring.make_contiguous();
    let (tail, cut) = bounded_output_tail(bytes, max_bytes);
    let plain = strip_ansi(&String::from_utf8_lossy(tail));
    // The bound is re-applied to the stripped text, which is the quantity the
    // retention budget charges and the reader sees.
    if let Some(snapshot) = zone_output_snapshot_from_plain(&plain, max_bytes, cut || dropped_front)
    {
        return Some(snapshot);
    }
    if !cut {
        return None;
    }
    // Nothing visible survived the escape-sequence adjustment; a snapshot of
    // stray parameter characters still reports this command's output, while
    // `None` here would claim it produced none.
    let plain = strip_ansi(&String::from_utf8_lossy(unadjusted_output_tail(
        bytes, max_bytes,
    )));
    zone_output_snapshot_from_plain(&plain, max_bytes, cut || dropped_front)
}

/// The same tail bound applied to already-stripped plain text: the payload
/// another consumer materialized first (the ring is consumed by then), and the
/// second, post-strip bound of the ring path. `already_truncated` carries
/// every byte known to be missing before this text existed — the ring's own
/// wrap marker — because none of it is visible in the text.
///
/// Trimmed like `BlockData.output`, so the two record kinds present one
/// whitespace convention to search and export.
fn zone_output_snapshot_from_plain(
    output_plain: &str,
    max_bytes: usize,
    already_truncated: bool,
) -> Option<ZoneOutputSnapshot> {
    let cut = output_plain.len() > max_bytes;
    let mut start = output_plain.len().saturating_sub(max_bytes);
    while start < output_plain.len() && !output_plain.is_char_boundary(start) {
        start += 1;
    }
    let plain = output_plain[start..].trim();
    if plain.is_empty() {
        return None;
    }
    Some(ZoneOutputSnapshot {
        plain: plain.to_string(),
        truncated: cut || already_truncated,
    })
}

/// The sole Unified live-feed wrapper. Marker bytes are separate terminal
/// feeds, so guest bytes are not copied and injected bytes cannot enter the
/// engine-owned prompt/output buffers.
fn feed_vte_with_zone_marker(
    vte: &Terminal,
    zone_marker: &RefCell<ZoneMarkerInjector>,
    bytes: &[u8],
) {
    feed_with_zone_marker(zone_marker, bytes, |part| vte.feed(part));
}

fn feed_with_zone_marker(
    zone_marker: &RefCell<ZoneMarkerInjector>,
    bytes: &[u8],
    mut feed: impl FnMut(&[u8]),
) {
    let open = zone_marker.borrow().open_bytes();
    if let Some(open) = open {
        feed(&open);
    }
    feed(bytes);
}

/// End marker authority and close any guest-open OSC 8 hyperlink on every
/// accepted C, even when entropy failure disabled our own marker.
fn close_zone_marker(
    zone_marker: &RefCell<ZoneMarkerInjector>,
    zone_id: Option<u64>,
    mut feed: impl FnMut(&[u8]),
) {
    zone_marker.borrow_mut().close_zone(zone_id);
    feed(ZONE_MARKER_CLOSE);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalResetKind {
    EraseScrollback,
    HardReset,
}

#[derive(Debug, PartialEq, Eq)]
enum ResetAwareParserPart {
    Bytes(Vec<u8>),
    /// ANSI control string kept away from the shared parser. Feed it to
    /// capture/VTE without letting payload CSI/OSC become lifecycle events
    /// or shell-capability observations.
    OpaqueBytes(Vec<u8>),
    /// A fully framed APC payload captured locally. Core now frames APC on
    /// strict ST as well; the local capture is retained (pending a dedicated
    /// equivalence check) so payload bytes never reach the shared parser.
    ApcSequence(Vec<u8>),
    /// Byte-exact ED3/RIS barrier. The sequence is fed to core in its own
    /// part so the reset's invalidation is ordered between the capability
    /// observations of the prefix and suffix parts; core emits the matching
    /// ParserEvent barrier when the bytes are parsed.
    Reset {
        kind: TerminalResetKind,
        bytes: Vec<u8>,
    },
}

#[derive(Debug, Default)]
enum ResetAwareParserState {
    #[default]
    Ground,
    Escape,
    Csi(Vec<u8>),
    ControlString {
        bel_terminates: bool,
        after_escape: bool,
        bypass_parser: bool,
        capture_apc: bool,
        apc_payload: Vec<u8>,
        apc_overflow: bool,
    },
}

#[derive(Debug, Default)]
struct ResetAwareParserSplitter {
    state: ResetAwareParserState,
}

fn csi_parameter_is_ed3(params: &[u8]) -> bool {
    !params.is_empty()
        && params.iter().all(u8::is_ascii_digit)
        && params.iter().fold(0_u32, |value, digit| {
            value
                .saturating_mul(10)
                .saturating_add(u32::from(*digit - b'0'))
        }) == 3
}

fn flush_reset_passthrough(parts: &mut Vec<ResetAwareParserPart>, passthrough: &mut Vec<u8>) {
    if !passthrough.is_empty() {
        parts.push(ResetAwareParserPart::Bytes(std::mem::take(passthrough)));
    }
}

fn flush_opaque_passthrough(parts: &mut Vec<ResetAwareParserPart>, opaque: &mut Vec<u8>) {
    if !opaque.is_empty() {
        parts.push(ResetAwareParserPart::OpaqueBytes(std::mem::take(opaque)));
    }
}

/// Split the raw PTY stream before pinned jterm_core parses it.
///
/// Only an unresolved ESC or CSI is retained across chunks (bounded by core's
/// own 4096-byte malformed-CSI ceiling). Every ordinary/control-string byte is
/// forwarded exactly once while a small framing state prevents reset-looking
/// text inside OSC/DCS/APC/PM/SOS from becoming a lifecycle side effect.
impl ResetAwareParserSplitter {
    /// Ordinary ground-state text needs no framing: every sequence this shim
    /// recognizes starts with ESC. Let the caller borrow that chunk directly
    /// instead of allocating a one-element parts vector and copying all bytes
    /// into a passthrough buffer before the shared parser copies them again.
    fn can_forward_borrowed(&self, bytes: &[u8]) -> bool {
        matches!(&self.state, ResetAwareParserState::Ground)
            && memchr::memchr(0x1b, bytes).is_none()
    }

    fn feed(&mut self, bytes: &[u8]) -> Vec<ResetAwareParserPart> {
        let mut parts = Vec::new();
        let mut passthrough = Vec::with_capacity(bytes.len());
        let mut opaque = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            match std::mem::take(&mut self.state) {
                ResetAwareParserState::Ground => match memchr::memchr(0x1b, &bytes[index..]) {
                    Some(offset) => {
                        flush_opaque_passthrough(&mut parts, &mut opaque);
                        passthrough.extend_from_slice(&bytes[index..index + offset]);
                        index += offset + 1;
                        self.state = ResetAwareParserState::Escape;
                    }
                    None => {
                        flush_opaque_passthrough(&mut parts, &mut opaque);
                        passthrough.extend_from_slice(&bytes[index..]);
                        index = bytes.len();
                        self.state = ResetAwareParserState::Ground;
                    }
                },
                ResetAwareParserState::Escape => {
                    let Some(&introducer) = bytes.get(index) else {
                        self.state = ResetAwareParserState::Escape;
                        break;
                    };
                    index += 1;
                    match introducer {
                        b'c' => {
                            flush_reset_passthrough(&mut parts, &mut passthrough);
                            parts.push(ResetAwareParserPart::Reset {
                                kind: TerminalResetKind::HardReset,
                                bytes: b"\x1bc".to_vec(),
                            });
                            self.state = ResetAwareParserState::Ground;
                        }
                        b'[' => {
                            self.state = ResetAwareParserState::Csi(b"\x1b[".to_vec());
                        }
                        b']' | b'P' | b'_' | b'^' | b'X' => {
                            // Core now frames DCS/APC/PM/SOS on strict ST with
                            // abort-and-reprocess recovery, but the bypass
                            // stays: bytes routed around the parser can emit
                            // no parser events and cannot register as
                            // shell-capability authority. Only OSC remains
                            // parser-owned because Anvil needs its real
                            // OSC 133/52/notification events.
                            let bypass_parser = introducer != b']';
                            let capture_apc = introducer == b'_';
                            if bypass_parser && !capture_apc {
                                flush_reset_passthrough(&mut parts, &mut passthrough);
                                opaque.extend_from_slice(&[0x1b, introducer]);
                            } else if !capture_apc {
                                passthrough.extend_from_slice(&[0x1b, introducer]);
                            }
                            self.state = ResetAwareParserState::ControlString {
                                bel_terminates: introducer == b']',
                                after_escape: false,
                                bypass_parser,
                                capture_apc,
                                apc_payload: Vec::new(),
                                apc_overflow: false,
                            };
                        }
                        _ => {
                            passthrough.extend_from_slice(&[0x1b, introducer]);
                            self.state = ResetAwareParserState::Ground;
                        }
                    }
                }
                ResetAwareParserState::Csi(mut sequence) => {
                    let byte = bytes[index];
                    index += 1;
                    sequence.push(byte);
                    if (0x40..=0x7e).contains(&byte) {
                        let kind = (byte == b'J'
                            && csi_parameter_is_ed3(&sequence[2..sequence.len() - 1]))
                        .then_some(TerminalResetKind::EraseScrollback);
                        if let Some(kind) = kind {
                            flush_reset_passthrough(&mut parts, &mut passthrough);
                            parts.push(ResetAwareParserPart::Reset {
                                kind,
                                bytes: sequence,
                            });
                        } else {
                            passthrough.extend_from_slice(&sequence);
                        }
                        self.state = ResetAwareParserState::Ground;
                    } else if sequence.len() > 4098 {
                        // Match pinned core: after more than 4096 parameter /
                        // intermediary bytes, recover to Ground and pass the
                        // malformed CSI without treating later text as params.
                        passthrough.extend_from_slice(&sequence);
                        self.state = ResetAwareParserState::Ground;
                    } else {
                        self.state = ResetAwareParserState::Csi(sequence);
                    }
                }
                ResetAwareParserState::ControlString {
                    bel_terminates,
                    mut after_escape,
                    bypass_parser,
                    capture_apc,
                    mut apc_payload,
                    mut apc_overflow,
                } => {
                    let byte = bytes[index];
                    if after_escape && byte != b'\\' {
                        // A non-ST escape aborts the old control string. Keep
                        // the ESC pending and reinterpret this byte as its
                        // introducer, so RIS/CSI after malformed strings keep
                        // their real stream order without duplicating bytes.
                        flush_opaque_passthrough(&mut parts, &mut opaque);
                        flush_reset_passthrough(&mut parts, &mut passthrough);
                        self.state = ResetAwareParserState::Escape;
                        continue;
                    }
                    index += 1;
                    if after_escape {
                        if capture_apc {
                            if !apc_overflow {
                                parts.push(ResetAwareParserPart::ApcSequence(apc_payload));
                            }
                        } else if bypass_parser {
                            opaque.extend_from_slice(b"\x1b\\");
                        } else {
                            passthrough.extend_from_slice(b"\x1b\\");
                        }
                        self.state = ResetAwareParserState::Ground;
                    } else if bel_terminates && byte == 0x07 {
                        if bypass_parser {
                            opaque.push(byte);
                        } else {
                            passthrough.push(byte);
                        }
                        self.state = ResetAwareParserState::Ground;
                    } else if byte == 0x1b {
                        // Hold one ambiguous ESC across chunks until ST vs an
                        // aborting new escape sequence is known.
                        after_escape = true;
                        self.state = ResetAwareParserState::ControlString {
                            bel_terminates,
                            after_escape,
                            bypass_parser,
                            capture_apc,
                            apc_payload,
                            apc_overflow,
                        };
                    } else {
                        if capture_apc {
                            if apc_payload.len() < MAX_LOCAL_APC_PAYLOAD_BYTES {
                                apc_payload.push(byte);
                            } else {
                                apc_payload.clear();
                                apc_overflow = true;
                            }
                        } else if bypass_parser {
                            opaque.push(byte);
                        } else {
                            passthrough.push(byte);
                        }
                        self.state = ResetAwareParserState::ControlString {
                            bel_terminates,
                            after_escape: false,
                            bypass_parser,
                            capture_apc,
                            apc_payload,
                            apc_overflow,
                        };
                    }
                }
            }
        }
        flush_opaque_passthrough(&mut parts, &mut opaque);
        flush_reset_passthrough(&mut parts, &mut passthrough);
        parts
    }
}

/// One long-lived full-size VTE driven by the normal OSC 133 lifecycle, with
/// no finished-block widgets or other block chrome.
struct UnifiedBackend {
    vte: Terminal,
    /// Same construction-time policy bit as the SubmissionSurface. Unified's
    /// stable full-size grid selects `false`.
    rebase_prompt_anchor_on_row_delta: bool,
    active_rc: Rc<RefCell<ActiveBlock>>,
    block_scroll_rc: ScrolledWindow,
    layout_active_surface: Rc<dyn Fn()>,
    config_for_cb: Rc<RefCell<Config>>,
    pty_for_init: Rc<OwnedPty>,
    /// The in-memory zone table: completed records for find/export/history
    /// palettes, each with an optional bounded output snapshot captured at
    /// finalize. The terminal text remains on `vte`, so no finished widget is
    /// mounted.
    zones: Rc<RefCell<UnifiedZoneStore>>,
    find_state_for_cb: Rc<RefCell<FindState>>,
    /// Per-pane fail-closed marker state. Marker bytes are fed only inside the
    /// backend, never through the parser or any engine capture buffer.
    zone_marker: Rc<RefCell<ZoneMarkerInjector>>,
    /// Probe/paint half of Unified zone chrome. Marker authority is separate
    /// from completed records so eviction cannot delete history metadata.
    chrome: unified_chrome::UnifiedChrome,
    /// Shared with the engine so a replayed zone draws from the same id
    /// sequence a live one does; a persisted id is never reused.
    reserved_history_block_ids: Rc<RefCell<HashSet<u64>>>,
    kitty_assembler: RefCell<kitty_graphics::Assembler>,
}

/// Captures the shared handles the PTY reader/exit callbacks need, so
/// `TermView::new` does not carry the reader closure inline. Widget-owning
/// handles live behind [`ReaderCtx::backend`]; the fields here are the parse
/// pipeline, the engine state, and the shared lifecycle cells.
struct ReaderCtx {
    /// Every widget/surface effect and query goes through this seam.
    backend: Rc<dyn RenderBackend>,
    bstate_rc: Rc<Cell<BlockState>>,
    engine: RefCell<EngineState>,
    /// Bounded raw-output ring for the running command — engine-owned shared
    /// state, deliberately NOT a backend query: the reader appends during
    /// CollectingOutput, CommandStart and the finalize path clear it, and
    /// [`ActiveBlock`] holds a clone only so live-find can snapshot it
    /// (`output_text`). Shared as an `Rc` cell rather than living in
    /// [`EngineState`] for exactly that one reader.
    live_raw_output_rc: Rc<RefCell<VecDeque<u8>>>,
    /// Whether [`MAX_RAW_OUTPUT_BYTES`] already discarded bytes off the front
    /// of the ring above. The retained tail cannot show this, and a consumer
    /// that snapshots the tail must be able to report that the stream was
    /// longer than what survives; cleared with the ring by
    /// [`ReaderCtx::clear_live_raw_output`].
    live_raw_output_dropped_rc: Rc<Cell<bool>>,
    /// Command-scoped proof that ED3 or RIS invalidated VTE's row mapping.
    /// Unlike raw-output presence, this is set before the reset bytes are fed
    /// and remains authoritative while VTE applies them asynchronously.
    live_extent_force_full_rc: Rc<Cell<bool>>,
    /// Keystroke-shadow input line, used only as a fallback if the VTE-text
    /// capture at CommandStart returns empty.
    typed_cmd_rc: Rc<RefCell<String>>,
    /// Once the user starts editing at an idle prompt, output is intentionally left
    /// inline: shell echo/completion and true background output are ambiguous then.
    idle_input_dirty_rc: Rc<Cell<bool>>,
    /// Live-surface cursor position (col, row) captured at PromptEnd; the start
    /// anchor for the text-range read that produces `EngineState::vte_typed_cmd`.
    prompt_end_pos_rc: Rc<Cell<(i64, i64)>>,
    prompt_anchor_rows_rc: Rc<Cell<i64>>,
    prompt_anchor_ready_rc: Rc<Cell<bool>>,
    remote_session_cbs: StrCallbacks,
    exited_cbs: IntCallbacks,
    activity_cbs: VoidCallbacks,
    alt_screen_cbs: AltScreenCallbacks,
    command_started_cbs: CommandStartedCallbacks,
    command_finished_cbs: CommandFinishedCallbacks,
    mouse_reporting_rc: Rc<Cell<MouseReportingMode>>,
    bracketed_paste_rc: Rc<Cell<bool>>,
    /// Dynamic OSC 10/11/12 overrides for this pane: consulted for OSC color
    /// query replies and overlaid onto the theme for new finished blocks.
    /// Shared with `TermView` so undo-clear rebuilds and theme switches see the
    /// same state.
    dynamic_colors_rc: DynamicColorsRc,
    config_for_cb: Rc<RefCell<Config>>,
    parser: Rc<RefCell<Parser>>,
    /// Raw OSC capability observer. It advances in reset-splitter order so a
    /// RIS can invalidate pre-reset trust before same-chunk suffix bytes.
    capability_observer: RefCell<ShellCapabilityObserver>,
    shell_capability_token: String,
    /// ANSI-aware raw framing in front of pinned jterm_core. Only unresolved
    /// reset candidates are retained across PTY chunks.
    reset_splitter: RefCell<ResetAwareParserSplitter>,
    reserved_history_block_ids: Rc<RefCell<HashSet<u64>>>,
    pty_synced_rc: Rc<Cell<bool>>,
    ftcs_seen_rc: Rc<Cell<bool>>,
    init_cmds_queue_for_cb: Rc<RefCell<std::collections::VecDeque<String>>>,
    pty_for_init: Rc<OwnedPty>,
    block_start_time_for_cb: Rc<Cell<Option<SystemTime>>>,
    /// The shell's execution id for the running command (jsh only): the key its
    /// execution journal keeps the record under. Engine-private, but kept out
    /// of `EngineState` so the `if let Some(id) = ...take()` at the journal
    /// submit can hold its scrutinee borrow across the submit without pinning
    /// the whole engine cell.
    execution_id_rc: Rc<RefCell<Option<String>>>,
    current_cwd_for_cb: Rc<RefCell<String>>,
    event_buf: Rc<RefCell<Vec<ParserEvent>>>,
    cmd_running_rc: Rc<Cell<bool>>,
    running_cmd_rc: Rc<RefCell<String>>,
    armed_agent_execution_rc: Rc<RefCell<Option<ArmedAgentExecution>>>,
    agent_prompt_generation_rc: Rc<Cell<u64>>,
    /// Agent identity consumed at CommandStart and emitted with this command's
    /// eventual trusted BlockFinished event.
    active_agent_execution_rc: Rc<Cell<Option<crate::agent::AgentExecutionRef>>>,
    agent_execution_supported_rc: Rc<Cell<bool>>,
    verified_submission: VerifiedSubmissionCtx,
    /// Fired engine-side after `finalize_block` returns; carries the resolved
    /// Agent correlation, so the trust policy behind it stays out of backends.
    block_finished_cbs: BlockFinishedCallbacks,
    /// Parks incoming PTY chunks while the user drag-selects text on the live
    /// VTE, so streaming repaints can't destroy the selection mid-drag.
    selection_feed_hold: Rc<SelectionFeedHold>,
}

/// Per-event handlers for the PTY reader pipeline: one method per
/// `ParserEvent` arm, dispatched by [`ReaderCtx::handle_event`] in the
/// order the events arrive. Bodies moved verbatim from the former
/// `process_chunk` match closure (arm-level `continue` became `return`).
impl ReaderCtx {
    /// Drop the engine-owned raw-output ring together with the marker that
    /// describes it. The two are only meaningful as a pair: retained bytes
    /// under a stale marker would report the next command's output as
    /// truncated, and fresh bytes under a cleared marker would report a
    /// wrapped stream as complete.
    fn clear_live_raw_output(&self) {
        self.live_raw_output_rc.borrow_mut().clear();
        self.live_raw_output_dropped_rc.set(false);
    }

    fn handle_event(&self, event: &ParserEvent) {
        let state = self.bstate_rc.get();
        match event {
            ParserEvent::RemoteSessionId(id) => self.on_remote_session_id(id),
            ParserEvent::DecsetMode { mode, set } => self.on_decset_mode(*mode, *set),
            ParserEvent::Bytes(bytes) => self.on_bytes(bytes, state),
            ParserEvent::PromptStart => self.on_prompt_start(),
            ParserEvent::PromptEnd => self.on_prompt_end(),
            ParserEvent::CommandStart(meta) => self.on_command_start(meta),
            ParserEvent::CommandEnd { exit, meta } => self.on_command_end(*exit, meta),
            ParserEvent::AltScreenEnter(mode) => self.on_alt_screen_enter(*mode),
            ParserEvent::AltScreenLeave(mode) => self.on_alt_screen_leave(*mode),
            ParserEvent::ClipboardSet(text) => self.on_clipboard_set(text),
            ParserEvent::ClipboardQuery => self.on_clipboard_query(),
            ParserEvent::ColorQuery(kind) => self.on_color_query(*kind),
            ParserEvent::ColorSet { kind, spec } => self.on_color_set(*kind, spec),
            ParserEvent::ColorReset(kind) => self.on_color_reset(*kind),
            ParserEvent::KeyboardProtocolQuery(query) => self.on_keyboard_protocol_query(*query),
            ParserEvent::ApcSequence(payload) => self.on_apc_sequence(payload),
            ParserEvent::Notification { title, body } => self.on_notification(title, body),
            // OSC 7771 readiness: Anvil's trust authority is the raw
            // ShellCapabilityObserver, which advances in reset-splitter order
            // ahead of dispatch. Core's copy of the token needs no handling.
            ParserEvent::AgentIntegrationReady(_) => {}
            // Core barrier events: emitted immediately before the exact
            // ED3/RIS bytes are passed through as Bytes, so the invalidation
            // runs once per sequence, ahead of the VTE feed. The splitter
            // keeps the part boundary (observation ordering) but no longer
            // synthesizes these calls itself.
            ParserEvent::EraseScrollback => self.on_erase_scrollback(),
            ParserEvent::HardReset => self.on_hard_reset(),
        }
    }

    fn on_remote_session_id(&self, id: &str) {
        for cb in self.remote_session_cbs.borrow().iter() {
            cb(id);
        }
    }

    fn on_erase_scrollback(&self) {
        // The parser emits this barrier before the exact ED3 bytes reach VTE.
        // Remember the semantic reset instead of inferring it from an
        // adjustment/cursor pair that settles asynchronously.
        self.live_extent_force_full_rc.set(true);
        self.backend.erase_scrollback();
    }

    fn on_decset_mode(&self, mode: u32, set: bool) {
        if mode == 2004 {
            self.bracketed_paste_rc.set(set);
            self.pty_for_init.set_shell_bracketed_paste(set);
        }
        // VTE handles paste/cursor/etc. natively from its
        // own bytes; block_view only needs mouse-reporting
        // state for wheel suppression in alt-screen apps.
        let new_mode = match (mode, set) {
            (1000, true) => Some(MouseReportingMode::Click),
            (1002, true) => Some(MouseReportingMode::Button),
            (1003, true) => Some(MouseReportingMode::Motion),
            (1006, true) => Some(MouseReportingMode::Sgr),
            (1000 | 1002 | 1003 | 1006, false) => Some(MouseReportingMode::None),
            _ => None,
        };
        if let Some(m) = new_mode {
            self.mouse_reporting_rc.set(m);
        }
    }

    fn on_hard_reset(&self) {
        // RIS invalidates the same row mapping as ED3. Record that before any
        // reset cleanup or bytes can trigger a live-surface layout.
        self.live_extent_force_full_rc.set(true);
        // RIS invalidates saved screens and every row address. Completed
        // metadata remains in the backend document, but an old marker must
        // never become authoritative again after reset.
        let (was_alt_screen, restored_state) = {
            let mut engine = self.engine.borrow_mut();
            engine.pending_zone = None;
            engine.osc133_depth = 0;
            engine.prompt_buf.clear();
            engine.background_output.clear();
            engine.background_output_dropped_front = false;
            engine.vte_typed_cmd.clear();
            engine.prompt_display.clear();
            engine.pending_exit_code = None;
            engine.shell_duration_ms = None;
            engine.execution_id_trusted = false;
            engine.agent_completion_trusted = false;
            engine.command_cwd = None;
            (
                self.bstate_rc.get() == BlockState::AltScreen,
                engine.prev_state,
            )
        };

        // No pending settle/submission may publish trust derived from the
        // pre-RIS surface. The generation bump invalidates an already queued
        // settle callback even though Anvil does not retain its source id.
        self.prompt_anchor_ready_rc.set(false);
        self.agent_prompt_generation_rc
            .set(self.agent_prompt_generation_rc.get().wrapping_add(1));
        self.prompt_end_pos_rc.set((-1, -1));
        self.prompt_anchor_rows_rc.set(0);
        self.typed_cmd_rc.borrow_mut().clear();
        self.idle_input_dirty_rc.set(false);
        self.pty_synced_rc.set(false);
        self.clear_live_raw_output();
        self.block_start_time_for_cb.set(None);
        self.execution_id_rc.borrow_mut().take();
        self.cmd_running_rc.set(false);
        self.running_cmd_rc.borrow_mut().clear();

        if let Some(source) = self.verified_submission.source_id.borrow_mut().take() {
            source.remove();
        }
        let pending_execution = self
            .verified_submission
            .submission
            .borrow_mut()
            .take()
            .and_then(|submission| submission.execution);
        self.armed_agent_execution_rc.borrow_mut().take();
        let active_execution = self
            .active_agent_execution_rc
            .take()
            .filter(|execution| Some(*execution) != pending_execution);
        for execution in [pending_execution, active_execution].into_iter().flatten() {
            emit_agent_execution_lost(
                &self.verified_submission.agent_execution_lost_callbacks,
                execution,
                "a terminal reset invalidated the Agent command correlation",
            );
        }
        self.agent_execution_supported_rc.set(false);
        *self.capability_observer.borrow_mut() = ShellCapabilityObserver::default();

        // This runs before the exact RIS bytes are fed. Unified clears both
        // injector and row authority here, so its normal feed wrapper cannot
        // prepend a retired OSC 8 marker to the reset itself.
        self.backend.hard_reset();

        if was_alt_screen {
            // RIS already returns VTE to the primary screen. Do not emit rmcup
            // after it; only unwind the frontend ownership/chrome state.
            self.backend.exit_fullscreen();
            self.backend.exit_alt_screen_chrome();
            emit_alt_screen_transition(&self.alt_screen_cbs, AltScreenTransition::Left);
            self.backend.layout_active_surface();
            self.bstate_rc.set(restored_state);
        }
        self.engine.borrow_mut().active_alt_screen_mode = None;
        self.backend.reset_kitty_pipeline();
        self.bracketed_paste_rc.set(false);
        self.pty_for_init.set_shell_bracketed_paste(false);
        self.mouse_reporting_rc.set(MouseReportingMode::None);
        self.dynamic_colors_rc.set(DynamicColors::default());
        let config = self.config_for_cb.borrow();
        *self.parser.borrow_mut() = Parser::with_config(ParserConfig {
            mouse_reporting: config.mouse_reporting_enabled,
            focus_reporting: config.focus_reporting_enabled,
        });
    }

    fn parse_and_dispatch(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut events = self.event_buf.borrow_mut();
        events.clear();
        self.parser.borrow_mut().feed(bytes, &mut events);
        coalesce_bytes_events(&mut events);
        // `events` remains borrowed across dispatch exactly as in the former
        // process_chunk body; handlers never touch this staging buffer.
        for event in events.iter() {
            self.handle_event(event);
        }
    }

    fn process_parser_input(&self, bytes: &[u8]) {
        // The local splitter only interposes on ESC-led reset/control strings.
        // Its Ground + no-ESC path would produce exactly one `Bytes` part, so
        // retain the original slice and avoid an otherwise redundant full-chunk
        // allocation/copy. A pending ESC/CSI/control string always takes the
        // stateful path even when this continuation chunk contains no ESC.
        if self.reset_splitter.borrow().can_forward_borrowed(bytes) {
            self.observe_capability_bytes(bytes);
            self.parse_and_dispatch(bytes);
            return;
        }
        let parts = self.reset_splitter.borrow_mut().feed(bytes);
        for part in parts {
            match part {
                ResetAwareParserPart::Bytes(bytes) => {
                    self.observe_capability_bytes(&bytes);
                    self.parse_and_dispatch(&bytes);
                }
                ResetAwareParserPart::OpaqueBytes(bytes) => {
                    // Capability packets nested in an opaque control string
                    // are data, not shell-integration authority.
                    self.on_bytes(&bytes, self.bstate_rc.get());
                }
                ResetAwareParserPart::ApcSequence(payload) => self.on_apc_sequence(&payload),
                ResetAwareParserPart::Reset { bytes, .. } => {
                    // Pure ordering barrier: the splitter carved these exact
                    // bytes into their own part so prefix parts were observed
                    // and parsed before the reset and suffix parts after it.
                    // Core emits the EraseScrollback/HardReset barrier event
                    // when the bytes are fed, and handle_event performs the
                    // invalidation exactly once from there — the splitter no
                    // longer calls the handlers itself.
                    self.observe_capability_bytes(&bytes);
                    self.parse_and_dispatch(&bytes);
                }
            }
        }
    }

    fn observe_capability_bytes(&self, bytes: &[u8]) {
        let expected = self.shell_capability_token.as_str();
        self.capability_observer.borrow_mut().feed(
            bytes,
            expected,
            &self.agent_execution_supported_rc,
        );
    }

    fn on_bytes(&self, bytes: &[u8], state: BlockState) {
        if state == BlockState::AwaitingCommand {
            if let Some(submission) = self.verified_submission.submission.borrow_mut().as_mut() {
                if submission.phase == ReviewedSubmissionPhase::Submitted
                    && !reviewed_pre_command_bytes_are_identity_neutral(bytes)
                {
                    submission.identity_feed_tainted = true;
                }
            }
        }
        // No shell integration seen yet: once real output flows,
        // stream everything into the live VTE (raw fallback).
        if state == BlockState::Idle {
            self.bstate_rc.set(BlockState::RawFallback);
        }

        let feed_active_vte = match self.bstate_rc.get() {
            BlockState::CollectingPrompt => {
                let text = String::from_utf8_lossy(bytes);
                append_bounded_text_tail(
                    &mut self.engine.borrow_mut().prompt_buf,
                    &text,
                    MAX_PROMPT_CAPTURE_BYTES,
                );
                self.backend.mark_scroll_dirty();
                true
            }
            BlockState::AwaitingCommand => {
                // Warp separates asynchronous output only when it
                // arrives before the user begins editing. Once input
                // is dirty, PTY echo/completion is indistinguishable
                // from a background process and remains inline.
                if !self.idle_input_dirty_rc.get() {
                    let mut engine = self.engine.borrow_mut();
                    let dropped = append_bounded_output(
                        &mut engine.background_output,
                        bytes,
                        MAX_RAW_OUTPUT_BYTES,
                    );
                    engine.background_output_dropped_front |= dropped;
                }
                self.backend.mark_scroll_dirty();
                true
            }
            BlockState::CollectingOutput | BlockState::PostCommand => {
                if self.bstate_rc.get() != BlockState::PostCommand
                    || !is_post_command_metadata(bytes)
                {
                    let dropped = append_bounded_output(
                        &mut self.live_raw_output_rc.borrow_mut(),
                        bytes,
                        MAX_RAW_OUTPUT_BYTES,
                    );
                    if dropped {
                        self.live_raw_output_dropped_rc.set(true);
                    }
                }
                for cb in self.activity_cbs.borrow().iter() {
                    cb();
                }
                true
            }
            BlockState::AltScreen => {
                // Alt-screen bytes go to the live VTE only — they
                // are not captured into block output (ephemeral).
                true
            }
            _ => true,
        };

        if feed_active_vte {
            self.backend.feed_live(bytes);
        }
    }

    fn on_prompt_start(&self) {
        self.ftcs_seen_rc.set(true);
        let state = self.bstate_rc.get();
        if state == BlockState::CollectingOutput || state == BlockState::AltScreen {
            return;
        }
        if state == BlockState::PostCommand
            && !agent_prompt_boundary_is_trusted(
                self.active_agent_execution_rc.get(),
                self.pty_for_init.shell_is_foreground(),
            )
        {
            // A foreground child can print a guessed/known
            // C/D/A sequence. Its D may have moved us to
            // PostCommand, but it cannot return foreground
            // ownership to the shell. Resume capture and
            // wait for the shell's real D + prompt instead.
            log::warn!("Ignoring an Agent prompt marker while a child process still owns the PTY");
            self.engine.borrow_mut().pending_exit_code = None;
            self.cmd_running_rc.set(true);
            self.bstate_rc.set(BlockState::CollectingOutput);
            return;
        }
        // Read out of the engine cell first: an `if` condition's temporary
        // `Ref` would otherwise stay alive across the fan-out in the body.
        let agent_completion_trusted = self.engine.borrow().agent_completion_trusted;
        if state == BlockState::PostCommand && !agent_completion_trusted {
            if let Some(execution) = self.active_agent_execution_rc.take() {
                emit_agent_execution_lost(
                    &self.verified_submission.agent_execution_lost_callbacks,
                    execution,
                    "the shell prompt arrived without a trusted matching command end",
                );
            }
        }
        // All rejection/recovery guards above have passed: this PromptStart
        // really ends the prior lifecycle. Clear an ED3/RIS full-card latch
        // before backend finalization resets VTE and can synchronously relayout.
        self.live_extent_force_full_rc.set(false);
        let background_output = if state == BlockState::AwaitingCommand {
            let mut engine = self.engine.borrow_mut();
            // The marker describes exactly the bytes taken (or discarded)
            // here, so it is consumed with them.
            let taken = take_background_output(&mut engine.background_output);
            let dropped_front = std::mem::take(&mut engine.background_output_dropped_front);
            taken.map(|output| (output, dropped_front))
        } else {
            None
        };
        let is_background = background_output.is_some();
        let completes_record = state == BlockState::PostCommand || is_background;
        let zone_plan =
            plan_prompt_zone(self.engine.borrow().pending_zone, completes_record, || {
                next_block_id(&self.reserved_history_block_ids)
            });
        self.engine.borrow_mut().pending_zone = Some(PendingZone::Prompt(zone_plan.prompt_id));
        self.backend.begin_prompt_zone(zone_plan.prompt_id);
        // Finalize the previous command (deferred from CommandEnd),
        // or turn commandless async output into a first-class block.
        if completes_record {
            // The VTE-text capture taken at CommandStart is
            // authoritative — it reflects what was on screen
            // when the user pressed Enter. Fall back to the
            // keystroke shadow only if the VTE read came back
            // empty (which would indicate the prompt-end
            // anchor never captured a valid cursor position).
            let mut cmd = if is_background {
                String::new()
            } else {
                finished_command(
                    &self.engine.borrow().vte_typed_cmd,
                    &self.typed_cmd_rc.borrow(),
                )
            };

            if cmd.is_empty() && !is_background {
                // Never silently discard a command lifecycle merely because
                // the asynchronous VTE range read and input shadow both raced.
                // A synchronized input write or visible output proves that a
                // command ran, and the bounded visibility replay avoids
                // materializing the lazy render payload to make that decision.
                let output_visible = {
                    let mut output = self.live_raw_output_rc.borrow_mut();
                    background_output_has_visible_text(output.make_contiguous())
                };
                if self.pty_synced_rc.get() || output_visible {
                    log::warn!(
                        "finished command text was unavailable; preserving record with placeholder"
                    );
                    cmd = UNAVAILABLE_COMMAND_PLACEHOLDER.to_string();
                } else {
                    // A genuinely empty submission with no output is not useful
                    // history; reset for the prompt without creating a record.
                    let preserve = self.config_for_cb.borrow().preserve_live_scrollback;
                    self.backend.reset_active_surface(preserve);
                    // The surface reset no longer drops the capture
                    // (the ring is engine-owned now); clear it here
                    // for parity with the pre-split `reset_active`.
                    self.clear_live_raw_output();
                    // No block is created here, so half-uploaded
                    // kitty chunks and undisplayed images have
                    // nowhere to land: drop them with the rest of
                    // the active state instead of leaking into
                    // the next command.
                    self.backend.reset_kitty_pipeline();
                    self.bstate_rc.set(BlockState::CollectingPrompt);
                    self.engine.borrow_mut().prompt_buf.clear();
                    self.backend.mark_scroll_dirty();
                    return;
                }
            }

            let prompt = if is_background {
                String::new()
            } else {
                std::mem::take(&mut self.engine.borrow_mut().prompt_display)
            };

            // Keep bytes in their existing capture owner until an actual
            // output consumer asks. Unified finalization therefore allocates
            // neither raw/plain strings nor a BlockData value by default.
            let (captured_output, output_dropped_front) = match background_output {
                Some((output, dropped_front)) => {
                    (CapturedFinalizeOutput::Background(output), dropped_front)
                }
                None => (
                    CapturedFinalizeOutput::Foreground(self.live_raw_output_rc.clone()),
                    self.live_raw_output_dropped_rc.get(),
                ),
            };

            let start_time = if is_background {
                None
            } else {
                self.block_start_time_for_cb.get()
            };
            let now = SystemTime::now();
            let end_time_ms = now
                .duration_since(SystemTime::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_millis() as u64);
            let start_time_ms = start_time.and_then(|st| {
                st.duration_since(SystemTime::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_millis() as u64)
            });
            // Commandless background output has no shell-reported
            // figure of its own, so it keeps the local timer only.
            let shell_duration_ms = if is_background {
                None
            } else {
                self.engine.borrow_mut().shell_duration_ms.take()
            };
            let duration_ms = block_duration_ms(shell_duration_ms, start_time, now);

            let block_cwd = {
                // The cwd the shell said the command ran in wins
                // over the pane's tracked cwd: after `cd`, the
                // OSC 7 that updated the pane already names the
                // directory the *next* command will run in.
                let reported = if is_background {
                    None
                } else {
                    self.engine.borrow_mut().command_cwd.take()
                };
                reported.or_else(|| {
                    let cwd_str = self.current_cwd_for_cb.borrow().clone();
                    if cwd_str.is_empty() {
                        None
                    } else {
                        Some(cwd_str)
                    }
                })
            };

            // `None` means the shell never reported a status.
            // Background output was never a command, so it has no
            // status to report either — both render neutrally
            // rather than as a success.
            let exit_code = if is_background {
                None
            } else {
                self.engine.borrow().pending_exit_code
            };

            // The accepted A that opened this command's prompt preallocated
            // the identity. The following A consumes exactly that id here and
            // opens a fresh prompt id; finalization must never mint another.
            let block_id = zone_plan
                .completed_record_id
                .expect("a completing prompt transition owns a record id");
            let record = CompletedCommandRecord {
                id: block_id,
                cmd: cmd.clone(),
                exit_code,
                start_time_ms,
                end_time_ms,
                duration_ms,
                cwd: block_cwd,
                is_background,
            };
            let payload =
                LazyBlockRenderPayload::new(prompt, captured_output, output_dropped_front);

            // jsh owns the command lifecycle record. A trusted correlation id
            // and an enabled output consumer must both exist before touching
            // the lazy payload; jterm_core::submit repeats the same capability
            // check at its queue boundary.
            let journal_execution_id = (!is_background)
                .then(|| self.execution_id_rc.borrow_mut().take())
                .flatten();
            let truncation_limit = self.config_for_cb.borrow().truncation_threshold_lines as usize;
            if let Some(submitted) = build_journal_completion(
                journal_execution_id,
                execution_journal_output_capture_enabled(),
                &payload,
                truncation_limit,
            ) {
                if let Err(error) = jterm_core::execution_journal::submit(submitted) {
                    log::warn!("jsh execution journal rejected a block's output: {error:?}");
                }
            }

            self.backend.finalize_block(&record, &payload);

            // Command-only history is a metadata consumer shared by both
            // backends. Keeping it engine-side prevents Unified from missing
            // palette/history entries without forcing output materialization.
            let (history_path, history_limit) = {
                let cfg = self.config_for_cb.borrow();
                (
                    cfg.command_history_enabled
                        .then(|| cfg.command_history_path.clone())
                        .flatten(),
                    cfg.command_history_max_entries as usize,
                )
            };
            if let Some(path) = history_path {
                if let Err(err) = crate::command_history::enqueue(
                    std::path::Path::new(&path),
                    history_limit,
                    &record.cmd,
                    record.cwd.as_deref(),
                    exit_code_for_i32_api(record.exit_code),
                    record.end_time_ms,
                ) {
                    log::warn!("command history: {err}");
                }
            }
            // DECISION: the block-finished fan-out — and the jsh journal
            // submission bundled with it — is engine policy and now runs AFTER
            // the whole backend finalize. Pre-split it ran mid-finalize: after
            // the finished-blocks push, before the long-block notification, the
            // context menu and selection installs, the second retention pass,
            // the command-history enqueue, reset_active, and the deferred scroll
            // pin. Audit of every registered connect_block_finished consumer:
            // terminal/block.rs (relm4 `sender.output` of CommandFinished +
            // BlockFinished — queued messages, no widget reads) and
            // organism_ui.rs (re-pins the inline organism card before the live
            // widget, which finalize never replaces, then defers a bottom pin).
            // Neither reads pre-reset_active surface state synchronously, so
            // the later emission point is observationally equivalent; bstate is
            // still PostCommand here, exactly as at the old emission point.
            // The Agent correlation resolved into the fan-out stays engine-side
            // by construction: on_prompt_start's foreground/trusted-completion
            // guards above already dropped an untrusted execution as lost
            // before this point, so finalize never sees the trust decision. The
            // `take` below therefore only moved later, not earlier — the sole
            // reader of the cell outside this engine is `agent_command_active`,
            // which the organism calls from a command-started handler, never
            // from a widget callback finalize installs or runs.
            if !is_background {
                let agent_execution = self.active_agent_execution_rc.take();
                let output_sample = OnceCell::new();
                for cb in self.block_finished_cbs.borrow().iter() {
                    match cb {
                        BlockFinishedCallback::Metadata(callback) => callback(
                            record.cmd.clone(),
                            record.exit_code,
                            agent_execution,
                            record.duration_ms,
                        ),
                        BlockFinishedCallback::ConditionalOutput {
                            needs_output,
                            callback,
                        } => {
                            let sample = needs_output(agent_execution).then(|| {
                                output_sample
                                    .get_or_init(|| {
                                        sample_output_for_event(&payload.materialize().output_plain)
                                    })
                                    .clone()
                            });
                            callback(
                                record.cmd.clone(),
                                record.exit_code,
                                sample,
                                agent_execution,
                                record.duration_ms,
                            );
                        }
                    }
                }
            }
            // Keep the engine-owned ring alive through backend finalization and
            // every conditional output observer; clear it only after fan-out.
            self.clear_live_raw_output();
        }
        self.bstate_rc.set(BlockState::CollectingPrompt);
        self.engine.borrow_mut().prompt_buf.clear();
        // Live VTE collapses back to the compact input cell
        // now that no command is running. Sync the PTY size
        // so the shell sees the new winsize before it reads
        // anything past the prompt.
        self.backend.sync_geometry_to_pty();
        self.backend.mark_scroll_dirty();
    }

    fn on_prompt_end(&self) {
        if self.bstate_rc.get() != BlockState::CollectingPrompt {
            return;
        }
        self.verified_submission.cancel_if_pending(
            "a new prompt arrived before the reviewed command start was verified",
        );
        // Capture the rendered prompt (last non-empty line) for the
        // finished block / export.
        {
            let mut engine = self.engine.borrow_mut();
            let prompt_line = strip_ansi(&engine.prompt_buf)
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string();
            engine.prompt_display = prompt_line;
            engine.prompt_buf.clear();
        }
        self.typed_cmd_rc.borrow_mut().clear();
        self.engine.borrow_mut().vte_typed_cmd.clear();
        {
            let mut engine = self.engine.borrow_mut();
            engine.background_output.clear();
            engine.background_output_dropped_front = false;
        }
        self.idle_input_dirty_rc.set(false);
        let prompt_generation = self.agent_prompt_generation_rc.get().wrapping_add(1);
        self.agent_prompt_generation_rc.set(prompt_generation);
        // An armed write belongs to exactly one prompt. A
        // redraw/new prompt before CommandStart invalidates
        // it instead of letting same text match later.
        self.armed_agent_execution_rc.borrow_mut().take();
        self.active_agent_execution_rc.set(None);
        self.engine.borrow_mut().agent_completion_trusted = false;
        self.engine.borrow_mut().execution_id_trusted = false;
        // Snapshot the live VTE cursor at the moment the
        // prompt finishes drawing — this is where the user's
        // command starts. CommandStart will read text from
        // here to the cursor's then-position to recover the
        // command as it really appeared on screen.
        let ((col, row), surface_rows) = self.backend.cursor_and_rows();
        self.prompt_end_pos_rc.set((col, row));
        self.prompt_anchor_rows_rc.set(surface_rows);
        self.prompt_anchor_ready_rc.set(false);
        self.pty_synced_rc.set(false);
        self.bstate_rc.set(BlockState::AwaitingCommand);
        // VTE applies feed asynchronously. Keep the cursor
        // captured at the authenticated PromptEnd boundary
        // immutable, then expose it after a short fence only
        // if no input or new prompt raced it. Moving this
        // anchor to the later live cursor could absorb text
        // printed after PromptEnd (for example a line-editor
        // prefill) into trusted prompt furniture.
        self.backend.schedule_anchor_settle(AnchorSettleArgs {
            prompt_generation,
            state: self.bstate_rc.clone(),
            dirty: self.idle_input_dirty_rc.clone(),
            synced: self.pty_synced_rc.clone(),
            generation: self.agent_prompt_generation_rc.clone(),
            ready: self.prompt_anchor_ready_rc.clone(),
        });
        self.backend.layout_active_surface();
        self.backend.focus_live_deferred();

        // Feed next initial command if any.
        if let Some(cmd) = self.init_cmds_queue_for_cb.borrow_mut().pop_front() {
            let text = format!("{}\r", cmd);
            self.idle_input_dirty_rc.set(true);
            self.pty_synced_rc.set(true);
            self.pty_for_init.write_bytes(text.as_bytes());
        }

        self.backend.reset_scroll_lock();
        self.backend.mark_scroll_dirty();
    }

    fn on_command_start(&self, meta: &CommandMeta) {
        self.ftcs_seen_rc.set(true);
        let state = self.bstate_rc.get();
        if state == BlockState::CollectingOutput || state == BlockState::AltScreen {
            let mut engine = self.engine.borrow_mut();
            engine.osc133_depth = engine.osc133_depth.saturating_add(1);
            return;
        }
        if state != BlockState::AwaitingCommand {
            return;
        }
        self.engine.borrow_mut().osc133_depth = 0;
        // A command start without an intervening PromptStart is
        // an ambiguous shell-integration edge. Keep those bytes
        // visible in the live VTE but do not merge them into the
        // command's output block.
        {
            let mut engine = self.engine.borrow_mut();
            engine.background_output.clear();
            engine.background_output_dropped_front = false;
        }
        // Engine-owned ring: the previous command's capture is dropped here,
        // exactly where `ActiveBlock::reset_output_buffer` used to do it.
        self.clear_live_raw_output();
        self.live_extent_force_full_rc.set(false);
        self.block_start_time_for_cb.set(Some(SystemTime::now()));
        // The shell may attach its own measurement to either
        // mark; jsh puts it on D. Reset it here so the previous
        // command's figure cannot be reused for this one.
        self.engine.borrow_mut().shell_duration_ms = meta.duration_ms;
        // jsh's execution id: the key its journal is written
        // under, so the output captured below can be attached
        // to the record instead of living only in this window.
        *self.execution_id_rc.borrow_mut() = meta.id.clone();
        let trusted_execution_id = meta.id.as_deref().is_some_and(|id| {
            self.pty_for_init
                .shell_integration_token()
                .is_some_and(|token| command_id_uses_shell_token(id, token))
        });
        self.engine.borrow_mut().execution_id_trusted = trusted_execution_id;
        self.engine.borrow_mut().agent_completion_trusted = false;
        // The cwd the command runs *in*. The pane's tracked cwd
        // comes from an OSC 7 the shell emits with its next
        // prompt, which for `cd`/`pushd` is already the new
        // directory by the time this block is finalized.
        self.engine.borrow_mut().command_cwd = meta.cwd.clone();
        // Scrape the command off the live VTE as the fallback
        // for shells that send bare marks: the range from the
        // cursor captured at PromptEnd to the cursor now (right
        // before the shell echoes a newline) is what the user
        // saw, including history recalls and jsh autosuggestion
        // accepts.
        let ((cmd_end_col, cmd_end_row), current_rows) = self.backend.cursor_and_rows();
        let (start_col, start_row) = self.backend.command_capture_anchor(
            self.prompt_end_pos_rc.get(),
            self.prompt_anchor_rows_rc.get(),
        );
        self.prompt_end_pos_rc.set((start_col, start_row));
        self.prompt_anchor_rows_rc.set(current_rows);
        let captured = if !self.prompt_anchor_ready_rc.get() {
            String::new()
        } else if command_capture_range_is_bounded(
            start_row,
            cmd_end_row,
            self.backend.live_column_count(),
        ) {
            self.backend
                .capture_text_range(start_row, start_col, cmd_end_row, cmd_end_col)
                .unwrap_or_default()
        } else {
            TRUNCATED_COMMAND_PLACEHOLDER.to_string()
        };
        let scraped = normalize_captured_command(&captured, &self.engine.borrow().prompt_display);
        let (command, source) =
            resolve_command_text(meta.command.as_deref(), meta.command_truncated, &scraped);
        // Command capture (and every preceding parser feed in this chunk) has
        // completed while the prompt zone is still open. C now converts that
        // exact zone to Command and closes it before any command output can be
        // admitted. Nested/unaccepted C returned above and cannot reach this.
        let command_zone_id = {
            let mut engine = self.engine.borrow_mut();
            match engine.pending_zone {
                Some(PendingZone::Prompt(id)) => {
                    engine.pending_zone = Some(PendingZone::Command(id));
                    Some(id)
                }
                other => {
                    log::debug!("command start without a prompt zone: {other:?}");
                    None
                }
            }
        };
        self.backend.close_prompt_zone(command_zone_id);
        if source == CommandTextSource::ScreenAfterTruncation {
            log::debug!(
                "Shell dropped an oversized command line; falling back to the screen capture ({} bytes)",
                command.len()
            );
        }
        let matching_execution = self.verified_submission.command_start_observed(
            meta.command.as_deref(),
            &captured,
            trusted_execution_id,
        );
        self.active_agent_execution_rc.set(matching_execution);
        self.engine.borrow_mut().vte_typed_cmd = command.clone();
        // `cwd` is read out of the engine cell before the fan-out: the
        // callbacks must not run under an engine borrow.
        let started_cwd = self.engine.borrow().command_cwd.clone();
        *self.running_cmd_rc.borrow_mut() = command.clone();
        self.cmd_running_rc.set(true);
        self.bstate_rc.set(BlockState::CollectingOutput);
        emit_command_started(
            &self.command_started_cbs,
            CommandStartedEvent {
                command,
                cwd: started_cwd,
            },
        );
        // Do not clear the input shadow here. VTE `feed()` is
        // asynchronous, so the text-range capture above can
        // occasionally be empty even for a real command. It
        // remains the finalize fallback until PromptEnd
        // clears both command buffers for the next prompt.
        // Match anvil's block-mode runtime model: keep the
        // active VTE as the live surface while the command
        // runs, then snapshot it into a finished block on the
        // next prompt. Interactive CLIs such as Codex rely on
        // VTE applying cursor positioning/redraws directly.
        self.backend.sync_geometry_to_pty();
        self.backend.mark_scroll_dirty();
    }

    fn on_command_end(&self, exit: Option<i32>, meta: &CommandMeta) {
        let state = self.bstate_rc.get();
        if state != BlockState::CollectingOutput && state != BlockState::AltScreen {
            return;
        }
        let matches_started_id = command_end_matches_started_id(
            self.execution_id_rc.borrow().as_deref(),
            meta.id.as_deref(),
        );
        let osc133_depth = self.engine.borrow().osc133_depth;
        if osc133_depth > 0 && !matches_started_id {
            self.engine.borrow_mut().osc133_depth = osc133_depth - 1;
            return;
        }
        if matches_started_id {
            // A command can print an unmatched nested C
            // marker. The shell's private outer C/D id still
            // identifies its real completion, so do not let
            // hostile output wedge this pane indefinitely.
            self.engine.borrow_mut().osc133_depth = 0;
        }
        let active_agent_execution = self.active_agent_execution_rc.get();
        let shell_is_foreground = self.pty_for_init.shell_is_foreground();
        let execution_id_trusted = self.engine.borrow().execution_id_trusted;
        let trusted_match = execution_id_trusted
            && command_end_matches_started_id(
                self.execution_id_rc.borrow().as_deref(),
                meta.id.as_deref(),
            );
        match decide_agent_command_end(
            active_agent_execution.is_some(),
            shell_is_foreground,
            trusted_match,
        ) {
            AgentCommandEndDecision::IgnoreUntilShellOwnsForeground => {
                // A foreground job can emit arbitrary OSC.
                // Wait until the interactive shell actually
                // regains ownership before accepting its D.
                log::warn!(
                    "Ignoring an Agent command-end marker while a child process owns the PTY"
                );
                return;
            }
            AgentCommandEndDecision::Accept => {
                if active_agent_execution.is_some() {
                    self.engine.borrow_mut().agent_completion_trusted = true;
                }
            }
            AgentCommandEndDecision::AcceptWithoutAgentCorrelation => {
                let execution =
                    active_agent_execution.expect("decision requires an active Agent execution");
                self.active_agent_execution_rc.set(None);
                self.engine.borrow_mut().agent_completion_trusted = false;
                emit_agent_execution_lost(
                    &self.verified_submission.agent_execution_lost_callbacks,
                    execution,
                    "the shell command end lacked a trusted matching id or foreground owner",
                );
            }
        }
        // Safety net (Warp parity): if the alt-screen app
        // crashed or exited without rmcup, force the UI back
        // to the block list so the next prompt is usable.
        if state == BlockState::AltScreen {
            let mode = self.engine.borrow_mut().active_alt_screen_mode.take();
            let mode = mode.unwrap_or(1049);
            let leave = format!("\x1b[?{mode}l");
            self.backend.feed_live(leave.as_bytes());
            let prompt_zone = {
                let engine = self.engine.borrow();
                prompt_zone_to_reopen_after_alt(engine.prev_state, engine.pending_zone)
            };
            if let Some(zone_id) = prompt_zone {
                // `rmcup` above restores the main screen after its marker
                // prefix was interpreted on the alternate screen.
                self.backend.begin_prompt_zone(zone_id);
            }
            self.backend.exit_fullscreen();
            self.backend.exit_alt_screen_chrome();
            emit_alt_screen_transition(&self.alt_screen_cbs, AltScreenTransition::Left);
            self.backend.layout_active_surface();
        }
        // `None` stays `None`: a shell that reported no status
        // is not a shell that reported success, and this used
        // to collapse to a green `exit 0`.
        self.engine.borrow_mut().pending_exit_code = exit;
        // jsh measures the command itself; only fall back to
        // this process's timer when the shell said nothing.
        if meta.duration_ms.is_some() {
            self.engine.borrow_mut().shell_duration_ms = meta.duration_ms;
        }
        // The D packet repeats the execution id, so a shell
        // that only tags the finish still correlates.
        if meta.id.is_some() {
            *self.execution_id_rc.borrow_mut() = meta.id.clone();
        }
        let shell_duration_ms = self.engine.borrow().shell_duration_ms;
        let duration_ms = shell_duration_ms.or_else(|| {
            self.block_start_time_for_cb.get().and_then(|started| {
                SystemTime::now()
                    .duration_since(started)
                    .ok()
                    .map(|elapsed| elapsed.as_millis().min(u64::MAX as u128) as u64)
            })
        });
        // `cwd` leaves the engine cell before the fan-out below.
        let finished_cwd = self.engine.borrow().command_cwd.clone();
        emit_command_finished(
            &self.command_finished_cbs,
            CommandFinishedEvent {
                command: self.running_cmd_rc.borrow().clone(),
                cwd: finished_cwd,
                exit_code: exit,
                duration_ms,
            },
        );
        self.cmd_running_rc.set(false);
        self.bstate_rc.set(BlockState::PostCommand);
        self.backend.mark_scroll_dirty();
    }

    fn on_alt_screen_enter(&self, mode: u32) {
        let from_state = self.bstate_rc.get();
        if from_state != BlockState::CollectingOutput && from_state != BlockState::AwaitingCommand {
            return;
        }
        self.engine.borrow_mut().prev_state = from_state;
        self.bstate_rc.set(BlockState::AltScreen);
        self.engine.borrow_mut().active_alt_screen_mode = Some(mode);
        self.backend.enter_alt_screen_chrome();
        emit_alt_screen_transition(&self.alt_screen_cbs, AltScreenTransition::Entered);
        // Hand the viewport to the alt-screen app: hide finished
        // blocks so the live VTE fills the scroll area.
        self.backend.enter_fullscreen();
        // Grow the live VTE to the full viewport before the
        // app draws (see sync_active_to_pty doc).
        self.backend.sync_geometry_to_pty();
        let enter = format!("\x1b[?{mode}h");
        self.backend.feed_live(enter.as_bytes());
    }

    fn on_alt_screen_leave(&self, mode: u32) {
        if self.bstate_rc.get() != BlockState::AltScreen {
            return;
        }
        // Warp parity: alt-screen content is ephemeral and is
        // NOT merged into the block. The active block keeps
        // just the command name + exit code.
        self.engine.borrow_mut().active_alt_screen_mode = None;
        self.backend.exit_alt_screen_chrome();
        emit_alt_screen_transition(&self.alt_screen_cbs, AltScreenTransition::Left);
        let leave = format!("\x1b[?{mode}l");
        self.backend.feed_live(leave.as_bytes());
        self.engine.borrow_mut().osc133_depth = 0;
        let prev_state = self.engine.borrow().prev_state;
        self.bstate_rc.set(prev_state);
        let prompt_zone =
            prompt_zone_to_reopen_after_alt(prev_state, self.engine.borrow().pending_zone);
        if let Some(zone_id) = prompt_zone {
            // The feed above executes `rmcup` first; only then reassert the
            // prompt marker on the restored main-screen document.
            self.backend.begin_prompt_zone(zone_id);
        }
        self.backend.exit_fullscreen();
        // Collapse the live VTE back to the compact input cell
        // now that the alt app has released the viewport.
        self.backend.sync_geometry_to_pty();
        self.backend.focus_live_deferred();
    }

    fn on_clipboard_set(&self, text: &str) {
        // The policy read stays engine-side; only the write crosses the seam.
        let allowed = self.config_for_cb.borrow().allow_remote_clipboard_write;
        if allowed {
            self.backend.set_system_clipboard(text);
        }
    }

    fn on_clipboard_query(&self) {
        self.pty_for_init.write_bytes(b"\x1b]52;c;\x1b\\");
    }

    fn on_color_query(&self, kind: ColorKind) {
        let reply = build_color_query_reply(
            &self.config_for_cb.borrow(),
            self.dynamic_colors_rc.get(),
            kind,
        );
        self.pty_for_init.write_bytes(reply.as_bytes());
    }

    // The original OSC bytes already passed through to the
    // live VTE (which recolors natively); only the tracked
    // values change here so queries and new finished blocks
    // see the dynamic color.
    fn on_color_set(&self, kind: ColorKind, spec: &str) {
        let mut colors = self.dynamic_colors_rc.get();
        colors.set(kind, spec);
        self.dynamic_colors_rc.set(colors);
    }

    fn on_color_reset(&self, kind: ColorKind) {
        let mut colors = self.dynamic_colors_rc.get();
        colors.reset(kind);
        self.dynamic_colors_rc.set(colors);
    }

    fn on_keyboard_protocol_query(&self, query: KeyboardProtocolQuery) {
        let (col, row) = self.backend.cursor_position_report();
        let reply = build_keyboard_query_reply(query, col, row);
        self.pty_for_init.write_bytes(reply.as_bytes());
    }

    fn on_apc_sequence(&self, payload: &[u8]) {
        // APC G — Kitty graphics. libvte has no APC graphics
        // handler, so re-wrapping these bytes and feeding
        // them to the live VTE (the previous behaviour)
        // silently dropped every inline image. Decode them
        // here instead, regardless of block state — tools
        // like `kitten icat` emit them at the shell prompt
        // (main screen), not only inside alt-screen apps.
        // Completed textures accumulate against the running
        // command and are mounted on its finished block.
        // Non-G APC payloads keep today's behaviour: consumed
        // silently, since libvte would ignore them anyway.
        if jterm_core::kitty_graphics::is_graphics_payload(payload) {
            let status = self.backend.kitty_feed(payload);
            // Answer before consuming the outcome: clients
            // like `kitten icat` block on the `i=`-keyed
            // OK/error reply. Keeping this ordering engine-side
            // is what makes the seam headlessly recordable.
            if let Some(reply) = kitty_graphics::response_for(payload, &status) {
                self.pty_for_init.write_bytes(&reply);
            }
            if status == kitty_graphics::FeedStatus::Complete {
                self.backend.kitty_admit_pending();
            }
        }
    }

    fn on_notification(&self, title: &Option<String>, body: &str) {
        // Desktop notification requested via OSC 9 / OSC
        // 777. The parser already control-stripped and
        // capped the text; here only launch pacing is
        // enforced, then extras drop silently.
        let now = Instant::now();
        LAST_NOTIFICATION_AT.with(|last| {
            if notification_permitted(last.get(), now) {
                last.set(Some(now));
                self.backend.desktop_notify(title.as_deref(), body);
            }
        });
    }
}

/// Fold every run of consecutive `ParserEvent::Bytes(_)` entries in `events`
/// into a single Bytes event whose payload is the concatenation. Preserves
/// the relative order of all other event kinds. The reader callback dispatches
/// per-event side effects (active_vte.feed, mark_dirty, accumulate_output,
/// activity_cbs), so coalescing replaces N feeds + N mark_dirty calls inside
/// one chunk with one of each per stretch — a win on `top` redraws, `cargo
/// build` spew, and any sustained byte-only output. Safe because boundary
/// events (PromptStart/End, AltScreen*, CommandStart/End) are NOT merged and
/// keep their own synchronous mark_dirty.
fn coalesce_bytes_events(events: &mut Vec<ParserEvent>) {
    if events.len() < 2 {
        return;
    }
    let mut write = 0usize;
    let mut i = 0usize;
    let n = events.len();
    while i < n {
        if matches!(events[i], ParserEvent::Bytes(_)) {
            // Move the first Bytes payload out so we can extend it in place.
            let placeholder = ParserEvent::Bytes(Vec::new());
            let first = std::mem::replace(&mut events[i], placeholder);
            let mut merged = match first {
                ParserEvent::Bytes(b) => b,
                _ => unreachable!(),
            };
            i += 1;
            while i < n {
                if let ParserEvent::Bytes(b) = &events[i] {
                    merged.extend_from_slice(b);
                    i += 1;
                } else {
                    break;
                }
            }
            events[write] = ParserEvent::Bytes(merged);
            write += 1;
        } else {
            if write != i {
                events.swap(write, i);
            }
            write += 1;
            i += 1;
        }
    }
    events.truncate(write);
}

/// Minimum spacing between OSC 9/777 desktop notifications. The request
/// originates inside the PTY (and may be remote over SSH), so spawning
/// `notify-send` is rate-limited app-wide rather than per pane.
const NOTIFICATION_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

thread_local! {
    /// Last desktop notification launch, shared by every block view: all PTY
    /// reader callbacks dispatch on the GTK main thread, so one thread-local
    /// cell is the app-level state.
    static LAST_NOTIFICATION_AT: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// True when enough time has passed since the previous desktop notification.
/// The first permitted notification stamps `LAST_NOTIFICATION_AT`, so later
/// requests in the same event batch fail this check and drop silently — at
/// most one notification per batch, matching frost.
fn notification_permitted(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|prev| now.duration_since(prev) >= NOTIFICATION_MIN_INTERVAL)
}

fn is_post_command_metadata(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x1b]7;")
        || bytes.starts_with(b"\x1b]0;")
        || bytes.starts_with(b"\x1b]1;")
        || bytes.starts_with(b"\x1b]2;")
}

/// Background output is meaningful only when stripping terminal decoration leaves
/// at least one visible character. Prompt redraw control sequences and blank CR/LF
/// bursts should not create empty history cards.
fn background_output_has_visible_text(bytes: &[u8]) -> bool {
    ansi::ansi_has_visible_text(bytes)
}

/// Snapshot the engine-owned raw-output ring as lossy UTF-8. Raw PTY bytes may
/// split a multi-byte sequence at either end of the bounded window, so the
/// conversion is deliberately lossy rather than fallible.
pub(super) fn live_output_text(ring: &RefCell<VecDeque<u8>>) -> String {
    let mut raw = ring.borrow_mut();
    if raw.is_empty() {
        return String::new();
    }
    String::from_utf8_lossy(raw.make_contiguous()).into_owned()
}

fn take_background_output(pending: &mut VecDeque<u8>) -> Option<VecDeque<u8>> {
    if background_output_has_visible_text(pending.make_contiguous()) {
        Some(std::mem::take(pending))
    } else {
        pending.clear();
        None
    }
}

/// Exact mirror of pinned jterm_core's private journal capability predicate.
/// `std::env::var(...).ok()` means a missing or non-Unicode value defaults to
/// enabled; only these five normalized spellings disable capture.
fn execution_journal_output_capture_enabled_for(value: Option<&str>) -> bool {
    value.is_none_or(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

fn execution_journal_output_capture_enabled() -> bool {
    let value = std::env::var("JSH_EXECUTION_JOURNAL").ok();
    execution_journal_output_capture_enabled_for(value.as_deref())
}

fn truncate_output_for_journal(output_plain: &str, line_limit: usize) -> String {
    let trimmed = output_plain.trim();
    let lines: Vec<&str> = trimmed.lines().collect();
    if lines.len() > line_limit {
        let kept = lines[..line_limit].join("\n");
        format!(
            "{}\n\n[... truncated: {} lines total, showing first {}]",
            kept,
            lines.len(),
            line_limit
        )
    } else {
        trimmed.to_string()
    }
}

fn build_journal_completion(
    id: Option<String>,
    enabled: bool,
    payload: &dyn BlockRenderPayloadAccessor,
    line_limit: usize,
) -> Option<jterm_core::execution_journal::CompletedExecution> {
    let id = id.filter(|_| enabled)?;
    let output_plain = &payload.materialize().output_plain;
    let total_bytes = output_plain.trim().len();
    let output = truncate_output_for_journal(output_plain, line_limit);
    Some(jterm_core::execution_journal::CompletedExecution {
        id,
        truncated: output.len() != total_bytes,
        total_bytes,
        output,
        output_available: true,
    })
}

/// Pick the command text recorded for a finished block.  The live VTE is the
/// most faithful source (history recall, cursor editing, suggestions), but its
/// `feed()` updates can still be queued when OSC 133;C is handled.  Keep the
/// input shadow as a fallback for that race instead of dropping the block.
fn finished_command(vte_capture: &str, input_shadow: &str) -> String {
    let captured = vte_capture.trim();
    if captured.is_empty() {
        input_shadow.trim().to_string()
    } else {
        captured.to_string()
    }
}

/// The GTK/VTE rendering seam. Bodies moved verbatim from the former
/// `ReaderCtx` handler statements and `render_finalized_block`; the free
/// helper functions they wrap stay module-level because non-reader paths
/// (restore, resize, submit) share them.
impl RenderBackend for BlockBackend {
    fn feed_live(&self, bytes: &[u8]) {
        self.active_vte.feed(bytes);
    }

    fn reset_active_surface(&self, preserve_scrollback: bool) {
        self.active_rc.borrow().reset_active(preserve_scrollback);
    }

    fn focus_live_deferred(&self) {
        if !self.active_vte.has_focus() {
            return;
        }
        let active_for_focus = self.active_rc.clone();
        glib::idle_add_local_once(move || {
            active_for_focus.borrow().grab_focus();
        });
    }

    fn sync_geometry_to_pty(&self) {
        sync_active_to_pty(
            &self.layout_active_surface,
            &self.active_vte,
            &self.block_scroll_rc,
            &self.pty_for_init,
        );
    }

    fn layout_active_surface(&self) {
        (self.layout_active_surface)();
    }

    fn records(&self) -> BackendRecords<'_> {
        BackendRecords::Blocks(self.block_data_for_cb.borrow())
    }

    fn record_search_target(&self, block_id: u64, is_output: bool) -> Option<RecordSearchTarget> {
        let finished = self.finished_blocks_for_cb.borrow();
        let block = finished.iter().find(|block| block.id == block_id)?;
        let terminal = if is_output {
            block.output_vte.clone()
        } else {
            block.command_vte.clone()
        };
        Some(RecordSearchTarget {
            terminal,
            widget: block.widget().clone().upcast(),
            uses_live_surface: false,
        })
    }

    fn scroll_to_record(&self, block_id: u64) -> bool {
        let widget: gtk::Widget = {
            let finished = self.finished_blocks_for_cb.borrow();
            let Some(block) = finished.iter().find(|block| block.id == block_id) else {
                return false;
            };
            block.widget().clone().upcast()
        };
        widget.grab_focus();
        find::scroll_widget_to_block_scroller_top(&widget, &self.block_scroll_rc);
        true
    }

    fn can_scroll_to_record(&self, block_id: u64) -> bool {
        // The same mounted-widget lookup `scroll_to_record` resolves.
        self.finished_blocks_for_cb
            .borrow()
            .iter()
            .any(|block| block.id == block_id)
    }

    fn jumpable_records(&self, candidates: &HashSet<(u64, bool)>) -> HashSet<(u64, bool)> {
        let finished = self.finished_blocks_for_cb.borrow();
        mounted_jumpable_records(finished.iter().map(|block| block.id), candidates)
    }

    fn completed_search_surfaces(
        &self,
        max_bytes: usize,
        deadline_exhausted: &mut dyn FnMut() -> bool,
    ) -> BackendSearchBatch {
        let finished = self.finished_blocks_for_cb.borrow();
        let mut remaining = max_bytes;
        let mut surfaces = Vec::new();
        for (block_index, block) in finished.iter().enumerate() {
            if remaining == 0 {
                return BackendSearchBatch {
                    surfaces,
                    incomplete: true,
                    native_fallback: None,
                };
            }
            let mut command_incomplete = false;
            if !push_search_surface_before_deadline(&mut surfaces, deadline_exhausted, || {
                let command_prefix = utf8_prefix_bounded(&block.cmd_text, remaining);
                command_incomplete = command_prefix.len() < block.cmd_text.len();
                remaining = remaining.saturating_sub(command_prefix.len());
                BackendSearchSurface {
                    block_id: block.id,
                    block_index,
                    is_output: false,
                    is_live: false,
                    windows: vec![BackendSearchWindow {
                        text: command_prefix.to_string(),
                        incomplete: command_incomplete,
                        initial_wrap: false,
                    }],
                    scanned_bytes: command_prefix.len(),
                    reset_cursor: false,
                    terminal: block.command_vte.clone(),
                }
            }) {
                return BackendSearchBatch {
                    surfaces,
                    incomplete: true,
                    native_fallback: None,
                };
            }
            if command_incomplete {
                return BackendSearchBatch {
                    surfaces,
                    incomplete: true,
                    native_fallback: None,
                };
            }
            if remaining == 0 {
                let has_more =
                    !block.full_output.borrow().is_empty() || block_index + 1 < finished.len();
                surfaces
                    .last_mut()
                    .expect("the command surface was just appended")
                    .windows
                    .last_mut()
                    .expect("a command search window exists")
                    .incomplete = has_more;
                return BackendSearchBatch {
                    surfaces,
                    incomplete: has_more,
                    native_fallback: None,
                };
            }

            // Bound the decorated prefix before ANSI stripping so both the
            // copied input and retained UTF-8 stay under the aggregate budget.
            let mut output_incomplete = false;
            if !push_search_surface_before_deadline(&mut surfaces, deadline_exhausted, || {
                let raw_output = block.full_output.borrow();
                let raw_prefix = utf8_prefix_bounded(&raw_output, remaining);
                output_incomplete = raw_prefix.len() < raw_output.len();
                remaining = remaining.saturating_sub(raw_prefix.len());
                BackendSearchSurface {
                    block_id: block.id,
                    block_index,
                    is_output: true,
                    is_live: false,
                    windows: vec![BackendSearchWindow {
                        text: strip_ansi(raw_prefix),
                        incomplete: output_incomplete,
                        initial_wrap: false,
                    }],
                    scanned_bytes: raw_prefix.len(),
                    reset_cursor: false,
                    terminal: block.output_vte.clone(),
                }
            }) {
                return BackendSearchBatch {
                    surfaces,
                    incomplete: true,
                    native_fallback: None,
                };
            }
            if output_incomplete {
                return BackendSearchBatch {
                    surfaces,
                    incomplete: true,
                    native_fallback: None,
                };
            }
            if remaining == 0 && block_index + 1 < finished.len() {
                surfaces
                    .last_mut()
                    .expect("the output surface was just appended")
                    .windows
                    .last_mut()
                    .expect("an output search window exists")
                    .incomplete = true;
                return BackendSearchBatch {
                    surfaces,
                    incomplete: true,
                    native_fallback: None,
                };
            }
        }
        BackendSearchBatch {
            surfaces,
            incomplete: false,
            native_fallback: None,
        }
    }

    fn debug_name(&self) -> &'static str {
        "block"
    }

    fn mark_scroll_dirty(&self) {
        self.scroll_debouncer.mark_dirty(&self.block_scroll_rc);
    }

    fn reset_scroll_lock(&self) {
        self.scroll_debouncer.reset_scroll_lock();
    }

    fn enter_alt_screen_chrome(&self) {
        let active = self.active_rc.borrow();
        active.set_live_organism_visible(false);
        active.set_live_organism_alt_screen(true);
    }

    fn exit_alt_screen_chrome(&self) {
        self.active_rc.borrow().set_live_organism_alt_screen(false);
    }

    fn enter_fullscreen(&self) {
        enter_fullscreen(
            &self.finished_blocks_for_cb,
            &self.visible_indices_rc,
            &self.fullscreen_rc,
        );
    }

    fn exit_fullscreen(&self) {
        exit_fullscreen(
            &self.finished_blocks_for_cb,
            &self.visible_indices_rc,
            &self.fullscreen_rc,
        );
    }

    fn kitty_feed(&self, payload: &[u8]) -> kitty_graphics::FeedStatus {
        // A pending admission the engine never consumed must not survive into
        // this feed's outcome (trait contract: cleared at the start of feed).
        self.kitty_pending_admission.borrow_mut().take();
        let outcome = self.kitty_assembler.borrow_mut().feed(payload);
        let status = outcome.status();
        // Park a completed texture backend-side; only the texture-free status
        // crosses the trait, and `kitty_admit_pending` consumes the parked
        // texture after the engine has written the protocol reply.
        if let kitty_graphics::Outcome::Complete(texture) = outcome {
            *self.kitty_pending_admission.borrow_mut() = Some(texture);
        }
        status
    }

    fn kitty_admit_pending(&self) {
        let Some(texture) = self.kitty_pending_admission.borrow_mut().take() else {
            return;
        };
        // Rough memory bound: width*height*4 (bytes
        // per RGBA pixel). Once the shared per-block
        // budget is exhausted, further images drop —
        // the transmission was still acknowledged
        // by the engine, only the display is skipped.
        let approx = (texture.width() as usize)
            .saturating_mul(texture.height() as usize)
            .saturating_mul(4);
        let used = self.kitty_pending_bytes.get();
        if used + approx <= kitty_graphics::MAX_PENDING_BYTES_PER_BLOCK {
            self.kitty_pending_bytes.set(used + approx);
            self.kitty_pending_images.borrow_mut().push(texture);
        } else {
            log::warn!(
                "kitty graphics: per-block image budget exhausted ({} + {} > {}), dropping",
                used,
                approx,
                kitty_graphics::MAX_PENDING_BYTES_PER_BLOCK
            );
        }
    }

    fn reset_kitty_pipeline(&self) {
        self.kitty_assembler.borrow_mut().reset();
        self.kitty_pending_images.borrow_mut().clear();
        self.kitty_pending_bytes.set(0);
        self.kitty_pending_admission.borrow_mut().take();
    }

    fn set_system_clipboard(&self, text: &str) {
        if let Some(display) = gtk::gdk::Display::default() {
            let clipboard = display.clipboard();
            clipboard.set_text(text);
        }
    }

    fn desktop_notify(&self, title: Option<&str>, body: &str) {
        crate::notify::app_notification(title, body);
    }

    fn schedule_anchor_settle(&self, args: AnchorSettleArgs) {
        schedule_prompt_anchor_settle(args);
    }

    fn cursor_and_rows(&self) -> ((i64, i64), i64) {
        (
            self.active_vte.cursor_position(),
            self.active_vte.row_count(),
        )
    }

    fn cursor_position_report(&self) -> (i64, i64) {
        // Bug-compatible on purpose: VTE's `cursor_position()` row is a
        // text-buffer (ring) row, and the pre-split code fed exactly this
        // value into the `ESC[{row+1};{col+1}R` reply. Kept so the reply
        // bytes do not change under the trait split; see the CPR note on
        // `RenderBackend::cursor_position_report`.
        self.active_vte.cursor_position()
    }

    fn command_capture_anchor(&self, provisional: (i64, i64), recorded_rows: i64) -> (i64, i64) {
        // Block changes the VTE from compact prompt height to full command
        // height between B and C. Rebase the saved anchor by that row delta;
        // the query-only submission surface carries the exact same policy bit.
        prompt_anchor_for_surface(
            self.rebase_prompt_anchor_on_row_delta,
            provisional,
            recorded_rows,
            self.active_vte.row_count(),
        )
    }

    fn grid_cols(&self) -> i64 {
        self.active_rc.borrow().grid_cols() as i64
    }

    fn live_column_count(&self) -> i64 {
        self.active_vte.column_count()
    }

    fn capture_text_range(
        &self,
        start_row: i64,
        start_col: i64,
        end_row: i64,
        end_col: i64,
    ) -> Option<String> {
        self.active_vte
            .text_range_format(vte4::Format::Text, start_row, start_col, end_row, end_col)
            .0
            .map(|gs| gs.to_string())
    }

    /// Mount a finished command as a history block and reset the live surface.
    ///
    /// Statement order is load-bearing: find state clears before eviction can
    /// drop a block, the `BlockData` record lands before the widget mounts, and
    /// the live-surface reset comes last. The block-finished fan-out and the
    /// jsh journal submission are engine policy and run after this returns (see
    /// `on_prompt_start`); the raw-output ring clear that used to ride on the
    /// tail `reset_active` is likewise the engine's now.
    fn finalize_block(
        &self,
        record: &CompletedCommandRecord,
        payload: &dyn BlockRenderPayloadAccessor,
    ) {
        // Block is the sole production backend that always asks for render
        // data. Every widget/persistence derivative stays on this side.
        let payload = payload.materialize();
        let block_id = record.id;
        let prompt = payload.prompt.as_str();
        let cmd = record.cmd.as_str();
        let output_with_ansi = payload.output_with_ansi.as_str();
        let output_plain = payload.output_plain.as_str();
        let plain_output_bytes = output_plain.len();
        let block_cwd = record.cwd.as_deref();
        let cols = bounded_finished_vte_columns(self.grid_cols());
        let truncation_limit = self.config_for_cb.borrow().truncation_threshold_lines as usize;
        let output_trimmed = truncate_output_for_journal(output_plain, truncation_limit);
        let line_count = output_trimmed.lines().count();
        let estimated_height = estimated_finished_block_height_for_text(
            &self.config_for_cb.borrow(),
            output_plain,
            cols,
        );
        let block_data = BlockData {
            id: block_id,
            prompt: prompt.to_owned(),
            cmd: cmd.to_owned(),
            cmd_markup: None,
            output: output_plain.trim().to_owned(),
            exit_code: record.exit_code,
            estimated_height,
            line_count,
            start_time_ms: record.start_time_ms,
            end_time_ms: record.end_time_ms,
            duration_ms: record.duration_ms,
            cwd: record.cwd.clone(),
            cols: cols as u16,
        };

        let max_blocks = self.config_for_cb.borrow().max_visible_blocks as usize;
        let newest_estimated_bytes = {
            let images = self.kitty_pending_images.borrow();
            estimated_live_finished_block_retained_bytes(
                prompt,
                cmd,
                None,
                output_with_ansi,
                plain_output_bytes,
                block_cwd,
                cols,
                &images,
            )
        };
        let prebuild_retention_plan = {
            let finished = self.finished_blocks_for_cb.borrow();
            plan_completed_block_retention_with_newest(
                &finished,
                block_id,
                newest_estimated_bytes,
                max_blocks,
            )
        };
        log_completed_block_retention("preparing live block", prebuild_retention_plan);
        if prebuild_retention_plan.evict_prefix > 0 {
            // Retention changes block indices and may
            // remove the VTE owning a highlighted hit.
            clear_find_handles(
                &self.finished_blocks_for_cb,
                &self.active_vte,
                &self.find_state_rc,
            );
        }
        evict_finished_block_prefix(
            prebuild_retention_plan.evict_prefix,
            &self.finished_blocks_for_cb,
            &self.block_data_for_cb,
            &self.block_list_rc,
            &self.widget_pool_for_cb,
            BlockRemovalRefs {
                selected_ids: &self.selected_block_ids_rc,
                selected: &self.selected_block_id_rc,
                anchor: &self.selection_anchor_id_rc,
                bookmarks: &self.bookmarks_rc,
                visible_indices: &self.visible_indices_rc,
                failure_marker_redraw: self.failure_marker_redraw.as_ref(),
                unread_count: &self.unread_count_rc,
                jump_fab: &self.jump_fab,
            },
        );

        mutate_block_data_and_redraw(
            &self.block_data_for_cb,
            self.failure_marker_redraw.as_ref(),
            |blocks| blocks.push_back(block_data),
        );

        // Drain the kitty-graphics images decoded during
        // this command so the finished block mounts them
        // below its text output. Images are display-only:
        // BlockData/history stay text-only, so a restored
        // session simply omits them.
        let kitty_images: Vec<gtk::gdk::Texture> =
            self.kitty_pending_images.borrow_mut().drain(..).collect();
        self.kitty_pending_bytes.set(0);

        let recycled = self.widget_pool_for_cb.borrow_mut().acquire();
        // Snapshot VTEs must match what the live view
        // showed: overlay any dynamic OSC 10/11/12
        // colors onto the theme for this block.
        let block_config =
            finished_block_config(&self.dynamic_colors_rc, &self.config_for_cb.borrow());
        let finished = FinishedBlock::new_with_pool(
            block_id,
            prompt,
            cmd,
            None,
            output_with_ansi,
            record.exit_code,
            &block_config,
            record.duration_ms,
            record.end_time_ms,
            block_cwd,
            cols,
            &kitty_images,
            plain_output_bytes,
            recycled,
        );
        finished
            .widget()
            .insert_before(&self.block_list_rc, Some(self.active_rc.borrow().widget()));

        let was_user_scrolled = self.scroll_debouncer.user_scrolled_up.get();

        // If the user is reading history (scrolled up), this
        // freshly-finished block is "unread": bump the FAB badge
        // so they can see work completed below and jump to it.
        if was_user_scrolled {
            self.unread_count_rc
                .set(self.unread_count_rc.get().saturating_add(1));
            set_jump_fab_label(&self.jump_fab, self.unread_count_rc.get());
            self.jump_fab.set_visible(true);
        }

        let finished_clone = finished.clone();
        let finished_widget = finished_clone.widget().clone();

        finished_clone.connect_actions(
            &self.active_vte,
            &self.pty_for_init,
            &self.pty_synced_rc,
            &self.bracketed_paste_rc,
            &self.typed_cmd_rc,
            &self.armed_agent_execution_rc,
            &self.bstate_rc,
            &self.active_rc,
        );
        finished_clone.connect_scroll_forwarding(&self.block_scroll_rc);

        self.finished_blocks_for_cb.borrow_mut().push(finished);

        {
            let cfg = self.config_for_cb.borrow();
            if !record.is_background && cfg.notify_long_blocks {
                if let Some(ms) = record.duration_ms {
                    if ms >= cfg.notify_long_block_threshold_ms {
                        crate::notify::long_block_finished(
                            cmd,
                            exit_code_for_i32_api(record.exit_code),
                            ms,
                        );
                    }
                }
            }
        }

        // Right-click context menu.
        self.install_finished_block_context_menu(
            finished_widget,
            finished_clone.clone(),
            finished_clone.id,
        );

        install_finished_block_selection(
            &finished_clone,
            &self.active_rc,
            &self.finished_blocks_for_cb,
            &self.selected_block_ids_rc,
            &self.selected_block_id_rc,
            &self.selection_anchor_id_rc,
        );

        // Recheck with the widget's actual retained
        // estimate. Normally this matches the pre-build
        // plan; the second pass closes any estimator
        // drift without ever evicting the newest card.
        let retention_plan = {
            let finished = self.finished_blocks_for_cb.borrow();
            plan_completed_block_retention_with_restored(&[], &finished, max_blocks)
        };
        log_completed_block_retention("finalizing live block", retention_plan);
        if retention_plan.evict_prefix > 0 {
            clear_find_handles(
                &self.finished_blocks_for_cb,
                &self.active_vte,
                &self.find_state_rc,
            );
        }
        evict_finished_block_prefix(
            retention_plan.evict_prefix,
            &self.finished_blocks_for_cb,
            &self.block_data_for_cb,
            &self.block_list_rc,
            &self.widget_pool_for_cb,
            BlockRemovalRefs {
                selected_ids: &self.selected_block_ids_rc,
                selected: &self.selected_block_id_rc,
                anchor: &self.selection_anchor_id_rc,
                bookmarks: &self.bookmarks_rc,
                visible_indices: &self.visible_indices_rc,
                failure_marker_redraw: self.failure_marker_redraw.as_ref(),
                unread_count: &self.unread_count_rc,
                jump_fab: &self.jump_fab,
            },
        );

        let preserve = self.config_for_cb.borrow().preserve_live_scrollback;
        self.active_rc.borrow().reset_active(preserve);
        // Drop any half-uploaded kitty chunks so they
        // can't leak into the next command (the drain
        // above already moved every completed image onto
        // the finished block). The parked admission, had
        // the engine skipped its admit, dies with them.
        self.kitty_assembler.borrow_mut().reset();
        self.kitty_pending_images.borrow_mut().clear();
        self.kitty_pending_bytes.set(0);
        self.kitty_pending_admission.borrow_mut().take();
        if !was_user_scrolled {
            self.scroll_debouncer.reset_scroll_lock();
            self.scroll_debouncer
                .pin_to_bottom_deferred(&self.block_scroll_rc);
        }
    }
}

fn utf8_prefix_bounded(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn capture_vte_text_range(
    vte: &Terminal,
    start_row: i64,
    start_col: i64,
    end_row: i64,
    end_col: i64,
) -> Option<String> {
    vte.text_range_format(vte4::Format::Text, start_row, start_col, end_row, end_col)
        .0
        .map(|text| text.to_string())
}

const VTE_SEARCH_CAPTURE_TIME_LIMIT: std::time::Duration = std::time::Duration::from_millis(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VteCaptureSpan {
    start_row: i64,
    end_row: i64,
    end_col: i64,
    work_cells: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct BoundedVteCapture {
    text: String,
    incomplete: bool,
    /// Grid-cell work charged against the caller's shared ceiling. Blank rows
    /// still cost native extraction work even when they return no text.
    work_cells: usize,
}

/// Extract terminal rows oldest-first without ever asking VTE to copy its
/// whole scrollback. Both requested grid work and retained UTF-8 are bounded
/// by `max_bytes`; each native call spans exactly one row. Despite calling it
/// the "last column", VTE treats `end_col` as the exclusive edge, so a request
/// from column zero through `end_col = N` costs N cells. A multi-row range
/// cannot be honestly charged from only its final `end_col`: every
/// intermediate row is copied at full width. The time predicate lets the UI
/// stop between calls, so a large blank ring is bounded too.
fn capture_vte_rows_bounded(
    start_row: i64,
    end_row: i64,
    columns: i64,
    max_bytes: usize,
    mut time_exhausted: impl FnMut() -> bool,
    mut capture: impl FnMut(VteCaptureSpan) -> Option<String>,
) -> BoundedVteCapture {
    if start_row > end_row {
        return BoundedVteCapture {
            text: String::new(),
            incomplete: false,
            work_cells: 0,
        };
    }
    if max_bytes == 0 {
        return BoundedVteCapture {
            text: String::new(),
            incomplete: true,
            work_cells: 0,
        };
    }

    let mut text = String::with_capacity(max_bytes.min(64 * 1024));
    let mut work_remaining = max_bytes;
    let mut row = start_row;
    let columns = usize::try_from(columns.max(1)).unwrap_or(usize::MAX);
    while row <= end_row && work_remaining > 0 && text.len() < max_bytes {
        if time_exhausted() {
            return BoundedVteCapture {
                text,
                incomplete: true,
                work_cells: max_bytes.saturating_sub(work_remaining),
            };
        }
        let work_cells = columns.min(work_remaining);
        // `work_cells` is non-zero by the loop condition and cannot exceed
        // the original positive i64 column count, so the exclusive edge is
        // representable by VTE's signed coordinate type.
        let end_col = i64::try_from(work_cells).unwrap_or(i64::MAX);
        let span = VteCaptureSpan {
            start_row: row,
            end_row: row,
            end_col,
            work_cells,
        };
        let raw = capture(span).unwrap_or_default();
        let remaining_bytes = max_bytes.saturating_sub(text.len());
        let prefix = utf8_prefix_bounded(&raw, remaining_bytes);
        text.push_str(prefix);
        work_remaining = work_remaining.saturating_sub(work_cells);

        let width_incomplete = work_cells < columns;
        let bytes_incomplete = prefix.len() < raw.len();
        row = row.saturating_add(1);
        if width_incomplete || bytes_incomplete {
            return BoundedVteCapture {
                text,
                incomplete: true,
                work_cells: max_bytes.saturating_sub(work_remaining),
            };
        }
    }

    BoundedVteCapture {
        incomplete: row <= end_row,
        text,
        work_cells: max_bytes.saturating_sub(work_remaining),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct BoundedVteSearchWindows {
    /// Native forward-search order after clearing selection begins at VTE's
    /// current viewport top, not at the oldest retained row.
    viewport_to_tail: BoundedVteCapture,
    /// Present only when the first window completed. Entering it corresponds
    /// to enabling one-shot native wrap-around.
    oldest_history: Option<BoundedVteCapture>,
}

/// Extract one persistent VTE domain in native forward-cursor order. The
/// second window is never requested after a partial first window, because an
/// unseen tail hit would precede every counted history hit.
fn capture_vte_search_windows_bounded(
    retained_start: i64,
    retained_end_exclusive: i64,
    viewport_top: i64,
    columns: i64,
    max_bytes: usize,
    mut time_exhausted: impl FnMut() -> bool,
    mut capture: impl FnMut(VteCaptureSpan) -> Option<String>,
) -> BoundedVteSearchWindows {
    let viewport_top = viewport_top.clamp(retained_start, retained_end_exclusive);
    let viewport_to_tail = capture_vte_rows_bounded(
        viewport_top,
        retained_end_exclusive.saturating_sub(1),
        columns,
        max_bytes,
        &mut time_exhausted,
        &mut capture,
    );
    let remaining = max_bytes
        .saturating_sub(viewport_to_tail.work_cells)
        .min(max_bytes.saturating_sub(viewport_to_tail.text.len()));
    let oldest_history = if !viewport_to_tail.incomplete
        && retained_start < viewport_top
        && remaining > 0
        && !time_exhausted()
    {
        Some(capture_vte_rows_bounded(
            retained_start,
            viewport_top.saturating_sub(1),
            columns,
            remaining,
            &mut time_exhausted,
            &mut capture,
        ))
    } else {
        None
    };
    BoundedVteSearchWindows {
        viewport_to_tail,
        oldest_history,
    }
}

impl RenderBackend for UnifiedBackend {
    fn feed_live(&self, bytes: &[u8]) {
        feed_vte_with_zone_marker(&self.vte, &self.zone_marker, bytes);
    }

    fn begin_prompt_zone(&self, zone_id: u64) {
        self.chrome.begin_zone(zone_id, &self.vte);
        self.chrome
            .enforce_limit(self.config_for_cb.borrow().max_visible_blocks as usize);
        let open = {
            let mut marker = self.zone_marker.borrow_mut();
            marker.begin_zone(zone_id);
            marker.open_bytes()
        };
        if let Some(open) = open {
            self.vte.feed(&open);
        }
    }

    fn close_prompt_zone(&self, zone_id: Option<u64>) {
        close_zone_marker(&self.zone_marker, zone_id, |bytes| self.vte.feed(bytes));
    }

    fn erase_scrollback(&self) {
        self.chrome.erase_scrollback(&self.vte);
    }

    fn hard_reset(&self) {
        self.zone_marker.borrow_mut().close_zone(None);
        self.chrome.clear_authority();
        find::clear_find_state(self.find_state_for_cb.as_ref(), &self.vte);
        self.kitty_assembler.borrow_mut().reset();
    }

    /// Preserve the continuous surface between prompts. A bare SGR reset keeps
    /// an interrupted command's attributes from tinting the next prompt.
    fn reset_active_surface(&self, _preserve_scrollback: bool) {
        self.feed_live(b"\x1b[0m");
    }

    fn focus_live_deferred(&self) {
        // Preserve Anvil's pane-focus invariant: a background pane receiving a
        // prompt must never steal focus merely because its backend is Unified.
        if !self.vte.has_focus() {
            return;
        }
        let vte = self.vte.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(vte) = vte.upgrade() {
                vte.grab_focus();
            }
        });
    }

    fn sync_geometry_to_pty(&self) {
        sync_active_to_pty(
            &self.layout_active_surface,
            &self.vte,
            &self.block_scroll_rc,
            &self.pty_for_init,
        );
    }

    fn layout_active_surface(&self) {
        (self.layout_active_surface)();
    }

    fn records(&self) -> BackendRecords<'_> {
        BackendRecords::Metadata(self.zones.borrow())
    }

    fn record_search_target(&self, _block_id: u64, _is_output: bool) -> Option<RecordSearchTarget> {
        // A shared VTE is not a per-record search surface: returning it here
        // would scope one record's match highlighting onto every zone.
        // Whole-surface find uses its native `block_id == 0` domain, and
        // record navigation goes through [`Self::scroll_to_record`] instead.
        None
    }

    fn scroll_to_record(&self, block_id: u64) -> bool {
        // Chrome only answers with a row it proved at the current epoch; any
        // doubt is `None` and this jump honestly fails.
        let Some(value) = self.chrome.proven_zone_scroll_value(&self.vte, block_id) else {
            return false;
        };
        let Some(adjustment) = gtk::prelude::ScrollableExt::vadjustment(&self.vte) else {
            return false;
        };
        adjustment.set_value(value);
        self.vte.grab_focus();
        true
    }

    fn can_scroll_to_record(&self, block_id: u64) -> bool {
        // The same proof, read only: a hit is labelled reachable exactly when
        // chrome can already name the row it would scroll to.
        self.chrome
            .proven_zone_scroll_value(&self.vte, block_id)
            .is_some()
    }

    fn completed_search_surfaces(
        &self,
        max_bytes: usize,
        deadline_exhausted: &mut dyn FnMut() -> bool,
    ) -> BackendSearchBatch {
        let native_fallback = || BackendNativeSearchFallback {
            block_id: 0,
            block_index: 0,
            is_output: true,
            is_live: true,
            terminal: self.vte.clone(),
        };
        if max_bytes == 0 || deadline_exhausted() {
            return BackendSearchBatch {
                surfaces: Vec::new(),
                incomplete: true,
                native_fallback: Some(native_fallback()),
            };
        }
        let Some(bounds) = self.chrome.trusted_ring_bounds(&self.vte) else {
            return BackendSearchBatch {
                surfaces: Vec::new(),
                incomplete: true,
                native_fallback: Some(native_fallback()),
            };
        };
        let (cursor_col, _cursor_row) = self.vte.cursor_position();
        let columns = self.vte.column_count().max(cursor_col).max(1);
        let started = std::time::Instant::now();
        let captured_windows = {
            let capture_deadline_exhausted =
                || deadline_exhausted() || started.elapsed() >= VTE_SEARCH_CAPTURE_TIME_LIMIT;
            capture_vte_search_windows_bounded(
                bounds.retained_start,
                bounds.retained_end_exclusive,
                bounds.viewport_top,
                columns,
                max_bytes,
                capture_deadline_exhausted,
                |span| {
                    capture_vte_text_range(&self.vte, span.start_row, 0, span.end_row, span.end_col)
                },
            )
        };
        let deadline_limited =
            deadline_exhausted() || started.elapsed() >= VTE_SEARCH_CAPTURE_TIME_LIMIT;
        let has_older_history = bounds.retained_start < bounds.viewport_top;
        let history_complete = !has_older_history
            || captured_windows
                .oldest_history
                .as_ref()
                .is_some_and(|capture| !capture.incomplete);
        let incomplete =
            deadline_limited || captured_windows.viewport_to_tail.incomplete || !history_complete;
        let scanned_work = captured_windows.viewport_to_tail.work_cells
            + captured_windows
                .oldest_history
                .as_ref()
                .map_or(0, |capture| capture.work_cells);
        let retained_bytes = captured_windows.viewport_to_tail.text.len()
            + captured_windows
                .oldest_history
                .as_ref()
                .map_or(0, |capture| capture.text.len());
        let scanned_bytes = scanned_work.max(retained_bytes);
        let mut windows = vec![BackendSearchWindow {
            text: captured_windows.viewport_to_tail.text,
            incomplete: incomplete
                && (deadline_limited
                    || captured_windows.viewport_to_tail.incomplete
                    || has_older_history),
            initial_wrap: false,
        }];
        if let Some(history) = captured_windows.oldest_history {
            windows.push(BackendSearchWindow {
                text: history.text,
                incomplete: incomplete && (deadline_limited || history.incomplete),
                initial_wrap: true,
            });
        }
        BackendSearchBatch {
            surfaces: vec![BackendSearchSurface {
                block_id: 0,
                block_index: 0,
                is_output: true,
                is_live: true,
                windows,
                scanned_bytes,
                reset_cursor: true,
                terminal: self.vte.clone(),
            }],
            incomplete,
            native_fallback: incomplete.then(native_fallback),
        }
    }

    fn persists_block_history(&self) -> bool {
        false
    }

    fn zone_replay_snapshot(
        &self,
        max_zones: usize,
        max_bytes: usize,
    ) -> Option<Vec<zone_history::PersistedZone>> {
        let zones = self.zones.borrow();
        let persisted = zones
            .records
            .iter()
            .map(|record| zone_history::PersistedZone::from_live(record, zones.snapshot(record.id)))
            .collect();
        Some(zone_history::bound_persisted_zones(
            persisted, max_zones, max_bytes,
        ))
    }

    fn replay_zone_snapshot(&self, zones: Vec<zone_history::PersistedZone>) -> usize {
        if zones.is_empty() {
            return 0;
        }
        // Replay is display-only: the bytes go straight to VTE, never through
        // the parser, so a restored zone cannot be mistaken for a command this
        // session ran. Marker frames come from this pane's own injector under
        // freshly issued ids, so chrome addresses restored rows exactly as it
        // addresses live ones.
        self.vte.feed(&zone_history::replay_banner(zones.len()));
        let max_zones = self.config_for_cb.borrow().max_visible_blocks as usize;
        let mut restored = 0;
        for zone in zones {
            let id = next_block_id(&self.reserved_history_block_ids);
            let (record, snapshot) = zone.into_live(id);
            self.chrome.begin_zone(id, &self.vte);
            self.zone_marker.borrow_mut().begin_zone(id);
            let bytes = zone_history::replay_bytes(
                &record,
                snapshot.as_ref().map(|snapshot| snapshot.plain.as_str()),
                snapshot.as_ref().is_some_and(|snapshot| snapshot.truncated),
            );
            feed_vte_with_zone_marker(&self.vte, &self.zone_marker, &bytes);
            close_zone_marker(&self.zone_marker, Some(id), |part| self.vte.feed(part));
            self.chrome
                .record_completed(unified_chrome::ZoneChromeRecord {
                    id: record.id,
                    exit_code: record.exit_code,
                    duration_ms: record.duration_ms,
                    is_background: record.is_background,
                });
            let retired = {
                let mut store = self.zones.borrow_mut();
                let retired = record_unified_zone(&mut store, record, max_zones);
                if let Some(snapshot) = snapshot {
                    store.insert_snapshot(id, snapshot);
                }
                store.enforce_snapshot_budget(MAX_TOTAL_SNAPSHOT_BYTES);
                retired
            };
            self.chrome.retire_ids(&retired);
            restored += 1;
        }
        self.chrome.enforce_limit(max_zones);
        restored
    }

    /// Cards are mounted, but in the dock: this surface owns the viewport, so
    /// a card in the scrolling document would sit where nothing can scroll.
    fn supports_inline_notices(&self) -> bool {
        true
    }

    fn docks_inline_notices(&self) -> bool {
        true
    }

    fn supports_block_mutation(&self) -> bool {
        false
    }

    fn scroll_surface_lines(&self, lines: i32) -> bool {
        let Some(adj) = gtk::prelude::ScrollableExt::vadjustment(&self.vte) else {
            return true;
        };
        let step = adj.step_increment().max(1.0);
        let max_value = (adj.upper() - adj.page_size()).max(adj.lower());
        adj.set_value((adj.value() + step * f64::from(lines)).clamp(adj.lower(), max_value));
        true
    }

    fn debug_name(&self) -> &'static str {
        "unified"
    }

    // The VTE's own adjustment is authoritative. The outer scroller contains
    // only this viewport-sized surface and has no history to follow.
    fn mark_scroll_dirty(&self) {}

    fn reset_scroll_lock(&self) {}

    fn finalize_block(
        &self,
        record: &CompletedCommandRecord,
        payload: &dyn BlockRenderPayloadAccessor,
    ) {
        find::clear_find_state(self.find_state_for_cb.as_ref(), &self.vte);
        let max_zones = self.config_for_cb.borrow().max_visible_blocks as usize;
        // The engine keeps the raw ring alive through the whole finalize
        // fan-out, so this bounded read still sees the command's bytes. No
        // full payload is materialized.
        let snapshot = payload.output_snapshot(MAX_ZONE_SNAPSHOT_BYTES);
        let retired_record_ids = {
            let mut zones = self.zones.borrow_mut();
            let retired = record_unified_zone(&mut zones, record.clone(), max_zones);
            if let Some(snapshot) = snapshot {
                zones.insert_snapshot(record.id, snapshot);
            }
            zones.enforce_snapshot_budget(MAX_TOTAL_SNAPSHOT_BYTES);
            retired
        };
        self.chrome.retire_ids(&retired_record_ids);
        self.chrome.enforce_limit(max_zones);
        self.chrome
            .record_completed(unified_chrome::ZoneChromeRecord {
                id: record.id,
                exit_code: record.exit_code,
                duration_ms: record.duration_ms,
                is_background: record.is_background,
            });
        log::debug!(
            "unified zone {} recorded: exit={:?} duration_ms={:?} background={} zones={}",
            record.id,
            record.exit_code,
            record.duration_ms,
            record.is_background,
            self.zones.borrow().records.len(),
        );
        self.kitty_assembler.borrow_mut().reset();
    }

    // The block chrome is permanently absent. The organism overlay still has
    // to be suppressed while an alternate-screen application owns the VTE.
    fn enter_alt_screen_chrome(&self) {
        self.chrome.set_alt_screen(true);
        let active = self.active_rc.borrow();
        active.set_live_organism_visible(false);
        active.set_live_organism_alt_screen(true);
    }

    fn exit_alt_screen_chrome(&self) {
        self.chrome.set_alt_screen(false);
        self.active_rc.borrow().set_live_organism_alt_screen(false);
    }

    fn enter_fullscreen(&self) {}

    fn exit_fullscreen(&self) {}

    /// Consume complete Kitty transfers but refuse every display-capable
    /// result: this backend has nowhere to mount the decoded texture yet.
    fn kitty_feed(&self, payload: &[u8]) -> kitty_graphics::FeedStatus {
        let outcome = self.kitty_assembler.borrow_mut().feed(payload);
        let status = outcome.status();
        drop(outcome);
        match status {
            kitty_graphics::FeedStatus::Pending => kitty_graphics::FeedStatus::Pending,
            kitty_graphics::FeedStatus::Invalid => kitty_graphics::FeedStatus::Invalid,
            _ => {
                log::debug!("unified: refusing Kitty graphics because no image surface exists");
                kitty_graphics::FeedStatus::Skipped
            }
        }
    }

    fn kitty_admit_pending(&self) {}

    fn reset_kitty_pipeline(&self) {
        self.kitty_assembler.borrow_mut().reset();
    }

    fn set_system_clipboard(&self, text: &str) {
        if let Some(display) = gtk::gdk::Display::default() {
            display.clipboard().set_text(text);
        }
    }

    fn desktop_notify(&self, title: Option<&str>, body: &str) {
        crate::notify::app_notification(title, body);
    }

    fn schedule_anchor_settle(&self, args: AnchorSettleArgs) {
        schedule_prompt_anchor_settle(args);
    }

    fn cursor_and_rows(&self) -> ((i64, i64), i64) {
        (self.vte.cursor_position(), self.vte.row_count())
    }

    fn cursor_position_report(&self) -> (i64, i64) {
        let (col, row) = self.vte.cursor_position();
        let top_row = gtk::prelude::ScrollableExt::vadjustment(&self.vte)
            .map(|adjustment| adjustment.value() as i64)
            .unwrap_or(0);
        (
            col,
            screen_relative_cpr_row(row, top_row, self.vte.row_count()),
        )
    }

    /// Unified does no compact/full grid churn, so ring-coordinate anchors are
    /// stable even when the viewport gains or loses rows.
    fn command_capture_anchor(&self, provisional: (i64, i64), recorded_rows: i64) -> (i64, i64) {
        prompt_anchor_for_surface(
            self.rebase_prompt_anchor_on_row_delta,
            provisional,
            recorded_rows,
            self.vte.row_count(),
        )
    }

    fn grid_cols(&self) -> i64 {
        self.vte.column_count().max(20)
    }

    fn live_column_count(&self) -> i64 {
        self.vte.column_count()
    }

    fn capture_text_range(
        &self,
        start_row: i64,
        start_col: i64,
        end_row: i64,
        end_col: i64,
    ) -> Option<String> {
        self.vte
            .text_range_format(vte4::Format::Text, start_row, start_col, end_row, end_col)
            .0
            .map(|text| text.to_string())
    }
}

impl ReaderCtx {
    /// `live_vte` is passed in rather than read back off the backend:
    /// `ReaderCtx` itself no longer names widget types, and the selection
    /// hold's VTE hooks are wiring, not lifecycle dispatch.
    fn install(self, pty: &Rc<OwnedPty>, live_vte: &Terminal) -> std::io::Result<()> {
        self.selection_feed_hold.install_vte_hooks(live_vte);

        let ctx = Rc::new(self);

        // Keep the complete security-observer → parser → Block state machine
        // behind one replayable closure. A selection hold intercepts raw chunks
        // before this boundary and flushes them back through this exact path.
        let process_chunk: Rc<RefCell<dyn FnMut(Vec<u8>)>> = Rc::new(RefCell::new({
            let ctx = ctx.clone();
            move |data: Vec<u8>| {
                ctx.process_parser_input(&data);
            }
        }));

        ctx.selection_feed_hold.set_flush({
            let process_chunk = Rc::downgrade(&process_chunk);
            move |bytes| {
                if let Some(process_chunk) = process_chunk.upgrade() {
                    (process_chunk.borrow_mut())(bytes);
                }
            }
        });

        let hold_for_reader = ctx.selection_feed_hold.clone();
        let hold_for_exit = ctx.selection_feed_hold.clone();
        let exited_cbs = ctx.exited_cbs.clone();
        pty.start_reader(
            move |data: Vec<u8>| {
                if hold_for_reader.try_buffer(&data) {
                    return;
                }
                (process_chunk.borrow_mut())(data);
            },
            move |exit_code| {
                hold_for_exit.flush_then(|| {
                    log::debug!("Shell exited with code {}", exit_code);
                    for cb in exited_cbs.borrow().iter() {
                        cb(exit_code);
                    }
                });
            },
        )
    }
}

/// Lay out the live surface and then push the full pane grid to the PTY
/// synchronously. Used at state
/// transitions where the child needs to see a correct winsize on its very first
/// read — `top` queries TIOCGWINSZ before painting, less/vim do the same.
/// Without the synchronous push the per-frame resize tick would catch up only
/// on the next frame, racing with the child.
fn sync_active_to_pty(
    layout_active_surface: &Rc<dyn Fn()>,
    vte: &Terminal,
    scroll: &ScrolledWindow,
    pty: &OwnedPty,
) {
    layout_active_surface();
    let (cols, rows) = pty_grid_size(vte, scroll);
    pty.resize(cols, rows);
}

fn pty_grid_size(vte: &Terminal, scroll: &ScrolledWindow) -> (u16, u16) {
    let cols = vte.column_count().max(1) as u16;
    let rows = viewport_rows_for(vte, scroll)
        .unwrap_or_else(|| vte.row_count().max(1))
        .clamp(1, u16::MAX as i64) as u16;
    (cols, rows)
}

fn viewport_rows_for(vte: &Terminal, scroll: &ScrolledWindow) -> Option<i64> {
    let cell_h = (vte.char_height() as i32).max(1);
    let page = scroll.vadjustment().page_size() as i32;
    if page <= 1 {
        return None;
    }
    // .block-active wraps the VTE with margin+border+padding; subtract the
    // chrome for the active density so the holder total fits exactly.
    let compact = vte
        .parent()
        .and_then(|parent| parent.downcast::<gtk::Box>().ok())
        .is_some_and(|holder| holder.has_css_class("block-compact"));
    let chrome = if compact {
        css::BLOCK_ACTIVE_COMPACT_VCHROME_PX
    } else {
        css::BLOCK_ACTIVE_VCHROME_PX
    };
    let usable = (page - chrome).max(cell_h);
    Some(((usable / cell_h).max(1)) as i64)
}

/// Rows the live card shows while a command is running.
///
/// The grid underneath stays a full viewport — that is the winsize the child
/// was given — but the card is only as tall as the output produced so far. Same
/// rule ember and frost apply to their live block: the greater of the idle
/// prompt's height and the content extent, capped by the viewport. Feeding it a
/// high-water extent keeps a repaint that parks the cursor back at the top from
/// shrinking the card mid-command.
fn live_visible_rows(extent: i64, viewport_rows: i64) -> i64 {
    let floor = (MIN_INPUT_ROWS as i64).min(viewport_rows);
    extent.clamp(floor, viewport_rows.max(floor))
}

/// Resolve one live-card measurement without trusting VTE's asynchronous
/// command-start transition before this command has emitted output.
///
/// `CommandStart` switches the block state and synchronously lays out the live
/// surface before VTE has necessarily applied the prompt/command bytes already
/// queued through `feed()`. During that short window [`live_content_extent`]
/// can expose either an unmeasurable extent or a coherent-looking stale extent
/// from the previous grid generation. Growing from either sample pushes
/// finished history off screen before the command has drawn anything. Until
/// the engine-owned output capture proves that this command has produced bytes,
/// keep the compact input height recorded in `high_water` and ignore the VTE
/// sample entirely. Once output exists, a coherent measurement may grow the
/// card, while an ordinary `None` remains provisional because the raw bytes are
/// captured before VTE finishes applying them. Only an explicit ED3/RIS parser
/// barrier sets `force_full` and exposes the whole viewport.
///
/// Returns `(visible_rows, next_high_water)`; the latter is monotone for the
/// lifetime of one command.
fn live_visible_rows_for_measurement(
    measured: Option<i64>,
    output_started: bool,
    force_full: bool,
    high_water: i64,
    viewport_rows: i64,
) -> (i64, i64) {
    if force_full {
        let next_high_water = high_water.max(viewport_rows);
        return (viewport_rows, next_high_water);
    }
    if !output_started {
        return (live_visible_rows(high_water, viewport_rows), high_water);
    }
    match measured {
        Some(measured) => {
            let next_high_water = high_water.max(measured);
            (
                live_visible_rows(next_high_water, viewport_rows),
                next_high_water,
            )
        }
        // The raw bytes enter the capture before VTE applies feed(). An
        // ordinary first output chunk can therefore still leave the extent
        // temporarily unreadable; retain the last proven height until a
        // contents-changed pass publishes a coherent measurement.
        None => (live_visible_rows(high_water, viewport_rows), high_water),
    }
}

/// How many rows of the live grid this command has reached: the top of the
/// screen down to the cursor.
///
/// Screen-relative on purpose. VTE row numbers are absolute ring coordinates
/// that climb for the whole session (and `[H[2J` does not reset them), so
/// `cursor_position` alone measures the session, not the block. `upper -
/// page_size` is the first row of the screen — the row the in-stream clear at
/// the previous `reset_active` homed the cursor to — and unlike the
/// adjustment's `value` it does not move when the user scrolls the live
/// terminal back.
fn live_content_extent(vte: &Terminal) -> Option<i64> {
    let adjustment = gtk::prelude::ScrollableExt::vadjustment(vte)?;
    live_content_extent_for(
        adjustment.lower(),
        adjustment.upper(),
        adjustment.page_size(),
        vte.cursor_position().1,
    )
}

/// The arithmetic behind [`live_content_extent`], separated so it can be tested
/// without a display.
///
/// `None` means the extent is not measurable right now. Every such sample is
/// provisional because cursor and adjustment may describe different grid
/// generations (see [`live_visible_rows_for_measurement`]). A running command
/// that emits `ESC[3J` (what `clear` sends to drop the scrollback) is handled by
/// the parser's explicit ED3 barrier instead of being inferred from `None`:
/// while VTE applies the bytes, it can renumber the adjustment down to the
/// screen but leave `cursor_position` in the old ring coordinates, so the two
/// stop describing the same buffer and any difference between them is
/// meaningless.
/// `cursor_row > upper` is the tell — the cursor is always inside the buffer the
/// adjustment describes, except that a cursor resting one row past the last
/// written row is normal, so the test has to be strict. The next prompt's
/// `reset_active` re-bases both.
fn live_content_extent_for(lower: f64, upper: f64, page_size: f64, cursor_row: i64) -> Option<i64> {
    if !lower.is_finite() || !upper.is_finite() || !page_size.is_finite() {
        return None;
    }
    if cursor_row > upper as i64 {
        return None;
    }
    let screen_top = (upper - page_size).max(lower) as i64;
    Some(
        cursor_row
            .saturating_sub(screen_top)
            .saturating_add(1)
            .max(1),
    )
}

type FinishedLayoutKey = (i32, i32, i32);

/// Change detector for the finished-block re-fit.
///
/// Deliberately *pure pane geometry*: the history's own page size and the cell
/// height (font zoom). It must not depend on the live input cell's height, nor
/// on how many blocks exist — both change on every command, and re-fitting a
/// block re-feeds its VTE, so folding either in made the entire history clear
/// and repaint on each Enter. A newly appended block fits itself from its own
/// `connect_map` handler instead.
fn finished_layout_key(page_width: i32, page_height: i32, cell_height: i32) -> FinishedLayoutKey {
    (page_width, page_height, cell_height.max(1))
}

fn compute_viewport_state(
    block_data: &VecDeque<BlockData>,
    visible_top: i32,
    visible_bottom: i32,
) -> ViewportState {
    let mut y = 0_i32;
    let mut first = None;
    let mut last = 0;
    for (index, block) in block_data.iter().enumerate() {
        let block_top = y;
        let block_bottom = y.saturating_add(block.estimated_height.max(1));
        if first.is_none() && block_bottom > visible_top {
            first = Some(index);
        }
        if block_top < visible_bottom {
            last = index;
        }
        y = block_bottom;

        if first.is_some() && y >= visible_bottom {
            break;
        }
    }

    ViewportState {
        first_visible: first.unwrap_or(0),
        last_visible: last,
    }
}

fn viewport_bounds_for_scroll(
    scroll_top: f64,
    viewport_height: f64,
    margin_pages: u32,
) -> Option<(i32, i32)> {
    if !scroll_top.is_finite() || !viewport_height.is_finite() || viewport_height < 1.0 {
        return None;
    }
    let scroll_top = scroll_top.max(0.0) as i32;
    let viewport_height = viewport_height as i32;
    if viewport_height <= 0 {
        return None;
    }
    let margin = viewport_height.saturating_mul(i32::try_from(margin_pages).unwrap_or(i32::MAX));
    let visible_top = scroll_top.saturating_sub(margin).max(0);
    let visible_bottom = scroll_top
        .saturating_add(viewport_height)
        .saturating_add(margin);
    (visible_bottom > visible_top).then_some((visible_top, visible_bottom))
}

/// Convert mapped GTK scroll geometry into a block range. Notebook/tab
/// transitions temporarily expose zero-sized adjustments; retaining the last
/// valid set avoids virtualizing every card during that transient frame.
fn viewport_state_for_scroll(
    block_data: &VecDeque<BlockData>,
    scroll_top: f64,
    viewport_height: f64,
    margin_pages: u32,
) -> Option<ViewportState> {
    let (visible_top, visible_bottom) =
        viewport_bounds_for_scroll(scroll_top, viewport_height, margin_pages)?;
    Some(compute_viewport_state(
        block_data,
        visible_top,
        visible_bottom,
    ))
}

/// Compute strict and one-page-looser virtualization ranges in one history
/// walk. Scroll signals used to call `viewport_state_for_scroll` twice; near
/// the bottom of a long session that scanned the same old block prefix twice
/// on the GTK main thread.
fn viewport_states_for_scroll(
    block_data: &VecDeque<BlockData>,
    scroll_top: f64,
    viewport_height: f64,
    margin_pages: u32,
) -> Option<(ViewportState, ViewportState)> {
    let strict_bounds = viewport_bounds_for_scroll(scroll_top, viewport_height, margin_pages)?;
    let loose_bounds =
        viewport_bounds_for_scroll(scroll_top, viewport_height, margin_pages.saturating_add(1))?;

    let mut y = 0_i32;
    let mut strict_first = None;
    let mut strict_last = 0;
    let mut loose_first = None;
    let mut loose_last = 0;
    for (index, block) in block_data.iter().enumerate() {
        let block_top = y;
        let block_bottom = y.saturating_add(block.estimated_height.max(1));

        if strict_first.is_none() && block_bottom > strict_bounds.0 {
            strict_first = Some(index);
        }
        if block_top < strict_bounds.1 {
            strict_last = index;
        }
        if loose_first.is_none() && block_bottom > loose_bounds.0 {
            loose_first = Some(index);
        }
        if block_top < loose_bounds.1 {
            loose_last = index;
        }
        y = block_bottom;

        if strict_first.is_some()
            && loose_first.is_some()
            && y >= strict_bounds.1.max(loose_bounds.1)
        {
            break;
        }
    }

    Some((
        ViewportState {
            first_visible: strict_first.unwrap_or(0),
            last_visible: strict_last,
        },
        ViewportState {
            first_visible: loose_first.unwrap_or(0),
            last_visible: loose_last,
        },
    ))
}

/// `changed` also fires when hiding a card changes only `upper`. Recompute on
/// real page-size changes, not on the virtualization side effect itself.
fn viewport_page_size_changed(last_page_size: &Cell<Option<f64>>, page_size: f64) -> bool {
    if !page_size.is_finite() {
        return false;
    }
    let changed = last_page_size
        .get()
        .is_none_or(|last| (last - page_size).abs() > 0.5);
    if changed {
        last_page_size.set(Some(page_size));
    }
    changed
}

fn visible_indices_for_viewport(vp: &ViewportState) -> HashSet<usize> {
    (vp.first_visible..=vp.last_visible.min(vp.first_visible.saturating_add(1000))).collect()
}

/// Strict visibility plus one extra margin page for cards already rendered.
/// This hysteresis prevents boundary cards from alternating visibility when a
/// geometry shrink clamps the bottom-pinned scroll value on the next frame.
fn stable_visible_indices(
    strict: &ViewportState,
    loose: Option<&ViewportState>,
    current: &HashSet<usize>,
) -> HashSet<usize> {
    let mut next = visible_indices_for_viewport(strict);
    if let Some(loose) = loose {
        let keep = visible_indices_for_viewport(loose);
        next.extend(current.iter().copied().filter(|index| keep.contains(index)));
    }
    next
}

fn apply_visible_indices(
    finished: &[FinishedBlock],
    block_data: &mut VecDeque<BlockData>,
    visible: &mut HashSet<usize>,
    new_visible: HashSet<usize>,
) {
    for (index, block) in finished.iter().enumerate() {
        let should_render = new_visible.contains(&index);
        // Off-screen cards become fixed-height placeholders rather than
        // disappearing, so the document's height — and with it the scroll
        // position the user is reading at — does not move as blocks cross the
        // viewport edge.
        let placeholder_height = block.set_virtualized(!should_render);
        let height = if should_render {
            // Keep the metadata document converged on real allocations for
            // rendered cards: the font-metric estimate drifts from what GTK
            // actually allocates, and `compute_viewport_state` accumulates that
            // drift until the boundary lands on a card that is still on screen.
            let allocated = block.widget().height();
            if allocated > 1 {
                allocated
            } else {
                continue;
            }
        } else {
            placeholder_height
        };
        if let Some(data) = block_data.get_mut(index) {
            data.estimated_height = height;
        }
    }
    *visible = new_visible;
}

/// Hand the viewport to an alt-screen app: hide every finished block so the live
/// VTE fills the scroll area like a normal full-screen terminal.
fn enter_fullscreen(
    finished: &Rc<RefCell<Vec<FinishedBlock>>>,
    visible_indices: &Rc<RefCell<std::collections::HashSet<usize>>>,
    fullscreen: &Rc<Cell<bool>>,
) {
    if fullscreen.replace(true) {
        return;
    }
    let finished = finished.borrow();
    // Virtual-scroll state is untouched: each card remembers whether it was
    // parked, so exiting restores exactly the pre-TUI document.
    let _visible = visible_indices.borrow();
    for block in finished.iter() {
        block.widget().set_visible(false);
    }
}

/// Restore the block list when the alt-screen app exits, re-applying virtual-scroll
/// visibility so only the previously-visible blocks reappear.
fn exit_fullscreen(
    finished: &Rc<RefCell<Vec<FinishedBlock>>>,
    visible_indices: &Rc<RefCell<std::collections::HashSet<usize>>>,
    fullscreen: &Rc<Cell<bool>>,
) {
    if !fullscreen.replace(false) {
        return;
    }
    let _visible = visible_indices.borrow();
    for block in finished.borrow().iter() {
        // The placeholder is part of the history document either way; each
        // card's contents remember whether virtual scrolling had parked them.
        block.widget().set_visible(true);
    }
}

/// Captures the handles the live-VTE key handler needs. With the VTE owning line
/// editing + IME natively (anvil model), this is reduced to a Capture-phase
/// navigation / copy-paste / block-selection handler; printable keys and editing
/// fall through to the VTE.
struct KeyCtx {
    pty_for_key: Rc<OwnedPty>,
    pty_synced_for_key: Rc<Cell<bool>>,
    bracketed_paste_for_key: Rc<Cell<bool>>,
    typed_cmd_for_key: Rc<RefCell<String>>,
    armed_agent_execution_for_key: Rc<RefCell<Option<ArmedAgentExecution>>>,
    finished_blocks_for_key: Rc<RefCell<Vec<FinishedBlock>>>,
    selected_block_ids_for_key: SelectedBlockIds,
    selected_block_id_for_key: Rc<Cell<Option<u64>>>,
    selection_anchor_id_for_key: Rc<Cell<Option<u64>>>,
    block_scroll_for_key: ScrolledWindow,
    bookmarks_for_key: Rc<RefCell<std::collections::HashSet<u64>>>,
    bstate_for_key: Rc<Cell<BlockState>>,
}

impl KeyCtx {
    fn connect(self, key_ctrl: &gtk::EventControllerKey) {
        let KeyCtx {
            pty_for_key,
            pty_synced_for_key,
            bracketed_paste_for_key,
            typed_cmd_for_key,
            armed_agent_execution_for_key,
            finished_blocks_for_key,
            selected_block_ids_for_key,
            selected_block_id_for_key,
            selection_anchor_id_for_key,
            block_scroll_for_key,
            bookmarks_for_key,
            bstate_for_key,
        } = self;
        key_ctrl.connect_key_pressed(move |_controller, keyval, _keycode, modifiers| {
            use gtk::gdk::Key;
            let ctrl = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            let shift = modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);
            let alt = modifiers.contains(gtk::gdk::ModifierType::ALT_MASK);

            // Warp pages and jumps through block history locally. While a command
            // or fullscreen/raw terminal owns the viewport, forward these keys to it.
            let history_navigation = !matches!(
                bstate_for_key.get(),
                BlockState::CollectingOutput | BlockState::AltScreen | BlockState::RawFallback
            );
            if !ctrl
                && !shift
                && !alt
                && history_navigation
                && matches!(keyval, Key::Home | Key::End)
            {
                scroll_history_to_edge(&block_scroll_for_key, keyval == Key::End);
                return glib::Propagation::Stop;
            }
            if !ctrl
                && !shift
                && !alt
                && history_navigation
                && matches!(keyval, Key::Page_Up | Key::Page_Down)
            {
                let adj = block_scroll_for_key.vadjustment();
                let step = (adj.page_size() * 0.9).max(1.0);
                let delta = if keyval == Key::Page_Up { -step } else { step };
                let max_val = (adj.upper() - adj.page_size()).max(adj.lower());
                adj.set_value((adj.value() + delta).clamp(adj.lower(), max_val));
                return glib::Propagation::Stop;
            }

            // Shift+Up/Down expands or contracts the active range. The fixed anchor
            // stays where selection began, while the stronger active edge moves.
            if !ctrl
                && shift
                && !alt
                && selected_block_id_for_key.get().is_some()
                && matches!(keyval, Key::Up | Key::Down)
            {
                let finished = finished_blocks_for_key.borrow();
                let direction = if keyval == Key::Up { -1 } else { 1 };
                if extend_finished_block_selection(
                    &finished,
                    &selected_block_ids_for_key,
                    &selected_block_id_for_key,
                    &selection_anchor_id_for_key,
                    &block_scroll_for_key,
                    direction,
                ) {
                    return glib::Propagation::Stop;
                }
            }

            // Once a block is selected, plain Up/Down walks blocks instead of
            // editing shell history. Without a selection the keys still reach VTE.
            if !ctrl
                && !shift
                && !alt
                && selected_block_id_for_key.get().is_some()
                && matches!(keyval, Key::Up | Key::Down)
            {
                let finished = finished_blocks_for_key.borrow();
                let direction = if keyval == Key::Up { -1 } else { 1 };
                move_finished_block_selection(
                    &finished,
                    &selected_block_ids_for_key,
                    &selected_block_id_for_key,
                    &selection_anchor_id_for_key,
                    &block_scroll_for_key,
                    direction,
                );
                return glib::Propagation::Stop;
            }

            // Warp Linux: Ctrl+Shift+Up/Down jumps to the top/bottom edge
            // of the currently selected block.
            if ctrl && shift && !alt && matches!(keyval, Key::Up | Key::Down) {
                let finished = finished_blocks_for_key.borrow();
                if scroll_selected_finished_block_edge(
                    &finished,
                    &selected_block_id_for_key,
                    &block_scroll_for_key,
                    keyval == Key::Down,
                ) {
                    return glib::Propagation::Stop;
                }
            }

            // Keep the existing bracket aliases for window-manager/keybinding
            // conflicts, but route them through the same selection semantics.
            if ctrl && shift && !alt && matches!(keyval, Key::bracketleft | Key::bracketright) {
                let finished = finished_blocks_for_key.borrow();
                let direction = if keyval == Key::bracketleft { -1 } else { 1 };
                move_finished_block_selection(
                    &finished,
                    &selected_block_ids_for_key,
                    &selected_block_id_for_key,
                    &selection_anchor_id_for_key,
                    &block_scroll_for_key,
                    direction,
                );
                return glib::Propagation::Stop;
            }

            // Enter while blocks are selected: recall every selected command in
            // terminal order as one editable multiline buffer. If the PTY is not
            // at a prompt (or the selection contains only background output), do
            // not swallow Enter from the running program/live editor.
            if matches!(keyval, Key::Return | Key::KP_Enter) {
                if selected_block_id_for_key.get().is_some() {
                    let finished = finished_blocks_for_key.borrow();
                    let recalled = if armed_agent_execution_for_key.borrow().is_some() {
                        false
                    } else {
                        let selected = selected_block_ids_for_key.borrow();
                        recall_selected_commands_at_prompt(
                            &pty_for_key,
                            &pty_synced_for_key,
                            &typed_cmd_for_key,
                            bstate_for_key.get(),
                            &finished,
                            &selected,
                            bracketed_paste_for_key.get(),
                        )
                    };
                    if recalled {
                        clear_finished_block_selection(
                            &finished,
                            &selected_block_ids_for_key,
                            &selected_block_id_for_key,
                            &selection_anchor_id_for_key,
                        );
                        return glib::Propagation::Stop;
                    }
                }
                return glib::Propagation::Proceed;
            }

            // Escape clears the block selection (when one is active).
            if keyval == Key::Escape {
                if selected_block_id_for_key.get().is_some() {
                    let finished = finished_blocks_for_key.borrow();
                    clear_finished_block_selection(
                        &finished,
                        &selected_block_ids_for_key,
                        &selected_block_id_for_key,
                        &selection_anchor_id_for_key,
                    );
                    return glib::Propagation::Stop;
                }
                return glib::Propagation::Proceed;
            }

            // Linux Warp toggles the output filter editor with Alt+Shift+F. Target
            // the selected block, or the latest block when selection is empty.
            if alt
                && shift
                && !ctrl
                && matches!(keyval, Key::f | Key::F)
                && bstate_for_key.get() != BlockState::AltScreen
            {
                let finished = finished_blocks_for_key.borrow();
                let target = selected_block_id_for_key
                    .get()
                    .and_then(|id| finished.iter().find(|block| block.id == id))
                    .or_else(|| finished.last());
                if let Some(block) = target {
                    (block.toggle_filter)();
                    return glib::Propagation::Stop;
                }
            }

            // Ctrl+Shift+B: toggle a bookmark on the selected block (Warp's
            // Linux binding). Shows the gutter star + accent stripe.
            // Only consume the key when bookmark logic actually fires.
            if ctrl
                && shift
                && !alt
                && matches!(keyval, Key::b | Key::B)
                && bstate_for_key.get() != BlockState::AltScreen
            {
                if let Some(sel_id) = selected_block_id_for_key.get() {
                    let finished = finished_blocks_for_key.borrow();
                    if let Some(block) = finished.iter().find(|b| b.id == sel_id) {
                        let mut marks = bookmarks_for_key.borrow_mut();
                        let now_marked = if marks.remove(&sel_id) {
                            false
                        } else {
                            marks.insert(sel_id);
                            true
                        };
                        block.bookmark_star.set_visible(now_marked);
                        if now_marked {
                            block.widget().add_css_class("block-bookmarked");
                        } else {
                            block.widget().remove_css_class("block-bookmarked");
                        }
                        return glib::Propagation::Stop;
                    }
                }
            }

            // Ctrl+,/Ctrl+. : jump to the previous/next bookmarked block (Warp's
            // SelectBookmarkUp/Down). The global pane-cycle defaults deliberately
            // leave these two context-sensitive chords available to block mode.
            if ctrl && !alt && !shift && matches!(keyval, Key::comma | Key::period) {
                let finished = finished_blocks_for_key.borrow();
                let marks = bookmarks_for_key.borrow();
                if marks.is_empty() {
                    return glib::Propagation::Stop;
                }
                let marked_idx: Vec<usize> = finished
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| marks.contains(&b.id))
                    .map(|(i, _)| i)
                    .collect();
                if marked_idx.is_empty() {
                    return glib::Propagation::Stop;
                }
                let cur = selected_block_id_for_key
                    .get()
                    .and_then(|id| finished.iter().position(|b| b.id == id));
                let target = if keyval == Key::comma {
                    marked_idx
                        .iter()
                        .rev()
                        .find(|&&i| cur.map(|c| i < c).unwrap_or(true))
                        .copied()
                        .or_else(|| marked_idx.last().copied())
                } else {
                    marked_idx
                        .iter()
                        .find(|&&i| cur.map(|c| i > c).unwrap_or(true))
                        .copied()
                        .or_else(|| marked_idx.first().copied())
                };
                if let Some(idx) = target {
                    let new_id = finished.get(idx).map(|b| b.id);
                    replace_finished_block_selection(
                        &finished,
                        &selected_block_ids_for_key,
                        &selected_block_id_for_key,
                        &selection_anchor_id_for_key,
                        new_id,
                    );
                    if let Some(block) = finished.get(idx) {
                        scroll_finished_block_into_view(block, &block_scroll_for_key);
                    }
                }
                return glib::Propagation::Stop;
            }

            // Ctrl+Shift+C / Ctrl+Shift+V are handled at the window-level
            // capture handler in main.rs (via TermView::copy_to_clipboard /
            // paste_from_clipboard) so they work regardless of which child
            // widget currently has focus — in particular after the user
            // mouse-selects text inside a finished block's TextView, focus
            // sits there and this per-VTE controller never fires.

            // Everything else: let the VTE translate it (printable keys, editing,
            // control sequences, IME) and emit `commit`.
            glib::Propagation::Proceed
        });
    }
}

#[allow(dead_code)]
impl TermView {
    /// Replace the runtime configuration shared by the reader/finalize
    /// callbacks. Visual setters are dispatched separately; this updates
    /// behavioral options (scrollback preservation, output truncation, block
    /// retention, long-block notifications, command-history capture, the OSC
    /// color-reply palette, remote clipboard policy) without requiring Block
    /// panes to be recreated. Parser flags (mouse/focus reporting) are
    /// snapshotted into `ParserConfig` at pane construction and are NOT
    /// affected.
    pub(crate) fn reload_config(&self, config: &Config) {
        *self.config.borrow_mut() = config.clone();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &Config,
        mode: &crate::config::TerminalMode,
        shell_argv: &[String],
        cwd: Option<&str>,
        cwd_external: bool,
        session_id: Option<&str>,
        cwd_token: &str,
        initial_commands: &[String],
    ) -> std::io::Result<Self> {
        // ── Build widget tree ──────────────────────────────────────────────
        let root = gtk::Box::new(Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.set_focusable(true);
        root.add_css_class("term-view-root");

        // Block list inside a scrolled window
        let block_list = gtk::Box::new(Orientation::Vertical, 0);
        block_list.set_vexpand(true); // anvil: expand so the active card fills
                                      // the space left below finished blocks.
        block_list.add_css_class("block-list");

        let block_scroll = ScrolledWindow::new();
        block_scroll.set_hexpand(true);
        block_scroll.set_vexpand(true);
        block_scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        block_scroll.set_child(Some(&block_list));
        block_scroll.add_css_class("block-scroll");
        // A focusable ScrolledWindow steals keyboard focus from the live VTE
        // child (cursor goes hollow, keystrokes never reach the terminal). Make
        // it not a focus target so focus delegates to the VTE. NOTE: use
        // `focusable(false)`, NOT `can_focus(false)` — in GTK4 `can-focus=false`
        // blocks the whole subtree (including the VTE) from ever taking focus.
        block_scroll.set_focusable(false);

        // Active block: a single persistent live VTE pinned at the bottom of the
        // block list. Prompt + typing + output all render here natively (anvil
        // model); finished commands snapshot into styled blocks above it.
        // Bounded raw-output ring for the running command. Engine-owned: the
        // reader appends/clears/snapshots it, and `ActiveBlock` holds this same
        // cell only so live-find can read the running capture.
        let live_raw_output: Rc<RefCell<VecDeque<u8>>> = Rc::new(RefCell::new(VecDeque::new()));
        let live_raw_output_dropped: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let live_extent_force_full: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let active = Rc::new(RefCell::new(ActiveBlock::new(
            config,
            live_raw_output.clone(),
        )));
        let active_vte = active.borrow().active_vte.clone();
        // The caller owns the mode decision. Managed remote panes deliberately
        // pass Block even when the shared config defaults to Unified.
        let unified = mode.is_unified();
        let unified_zones: Rc<RefCell<UnifiedZoneStore>> =
            Rc::new(RefCell::new(UnifiedZoneStore::new()));
        if unified {
            let holder = active.borrow().widget().clone();
            holder.remove_css_class("block-compact");
            holder.add_css_class("block-fullscreen");
        }
        let focus_requested = Rc::new(Cell::new(false));
        {
            let focus_requested = focus_requested.clone();
            active_vte.connect_map(move |vte| {
                if focus_requested.replace(false) {
                    vte.grab_focus();
                }
            });
        }

        block_list.append(active.borrow().widget());

        // The live VTE is visually compact at a prompt and expands to the full
        // viewport while a command or terminal app owns the surface. Its PTY
        // geometry remains viewport-sized in both cases.

        // ── Jump-to-bottom floating action button ─────────────────────────
        // Shown when the user scrolls up into history; an optional unread badge
        // counts finished blocks that completed while scrolled away. Clicking it
        // returns the view to the live prompt. Overlaid on the scroll area so it
        // floats over the block list without taking layout space.
        let jump_fab = gtk::Button::new();
        jump_fab.add_css_class("jump-bottom-fab");
        jump_fab.add_css_class("flat");
        jump_fab.set_tooltip_text(Some("Jump to latest"));
        set_jump_fab_label(&jump_fab, 0);
        jump_fab.set_halign(gtk::Align::End);
        jump_fab.set_valign(gtk::Align::End);
        jump_fab.set_margin_end(18);
        jump_fab.set_margin_bottom(18);
        jump_fab.set_visible(false);
        jump_fab.set_can_focus(false);

        // ── Sticky running-command header ─────────────────────────────────
        // When a command is running and the user has scrolled up into history,
        // a thin bar pins to the top of the scroll area showing the live command
        // and its elapsed time, so they don't lose track of what's executing.
        let sticky_label = gtk::Label::new(None);
        sticky_label.set_halign(gtk::Align::Start);
        sticky_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        sticky_label.set_hexpand(true);
        sticky_label.add_css_class("sticky-running-label");

        let sticky_jump_bottom_btn =
            icon_button("go-bottom-symbolic", "Jump to bottom of this block");
        sticky_jump_bottom_btn.add_css_class("sticky-header-control");
        sticky_jump_bottom_btn.add_css_class("flat");
        sticky_jump_bottom_btn.set_focusable(false);
        sticky_jump_bottom_btn.set_visible(false);

        let sticky_minimize_btn = icon_button("go-up-symbolic", "Minimize sticky command header");
        sticky_minimize_btn.add_css_class("sticky-header-control");
        sticky_minimize_btn.add_css_class("flat");
        sticky_minimize_btn.set_focusable(false);

        let sticky_stop_btn = icon_button(
            "media-playback-stop-symbolic",
            "Interrupt the running command (Ctrl+C)",
        );
        sticky_stop_btn.add_css_class("sticky-header-control");
        sticky_stop_btn.add_css_class("flat");
        sticky_stop_btn.set_focusable(false);
        sticky_stop_btn.set_visible(false);

        let sticky_organism_slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        sticky_organism_slot.set_can_target(false);
        sticky_organism_slot.set_focusable(false);
        sticky_organism_slot.set_visible(false);
        let sticky_bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        sticky_bar.add_css_class("sticky-running-header");
        sticky_bar.append(&sticky_organism_slot);
        sticky_bar.append(&sticky_label);
        sticky_bar.append(&sticky_stop_btn);
        sticky_bar.append(&sticky_jump_bottom_btn);
        sticky_bar.append(&sticky_minimize_btn);
        sticky_bar.set_halign(gtk::Align::Fill);
        sticky_bar.set_valign(gtk::Align::Start);
        sticky_bar.set_visible(false);
        sticky_bar.set_focusable(false);

        // Some sticky headers represent a finished, oversized block. Store that
        // block id so clicking the label can jump back to its command start.
        let sticky_target_id: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));
        let sticky_minimized: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        {
            let minimized = sticky_minimized.clone();
            let label = sticky_label.clone();
            let jump = sticky_jump_bottom_btn.clone();
            let stop = sticky_stop_btn.clone();
            let bar = sticky_bar.clone();
            sticky_minimize_btn.connect_clicked(move |button| {
                let now_minimized = !minimized.get();
                minimized.set(now_minimized);
                label.set_visible(!now_minimized);
                jump.set_visible(false);
                stop.set_visible(false);
                if now_minimized {
                    bar.add_css_class("sticky-minimized");
                    set_icon_button(button, "go-down-symbolic", "Expand sticky command header");
                } else {
                    bar.remove_css_class("sticky-minimized");
                    set_icon_button(button, "go-up-symbolic", "Minimize sticky command header");
                }
            });
        }

        let scroll_overlay = gtk::Overlay::new();
        scroll_overlay.set_child(Some(&block_scroll));
        scroll_overlay.add_overlay(&sticky_bar);
        scroll_overlay.add_overlay(&jump_fab);

        // Streaming output would normally repaint away a live-VTE selection.
        // Park the raw feed during that drag and explain the frozen surface only
        // after the first chunk is actually deferred.
        let selection_feed_hold = SelectionFeedHold::new();
        {
            let hold_badge = gtk::Label::new(Some("Output paused — selection"));
            hold_badge.add_css_class("feed-hold-badge");
            hold_badge.set_accessible_role(gtk::AccessibleRole::Status);
            hold_badge.update_property(&[gtk::accessible::Property::Label(
                "Output paused while text is selected",
            )]);
            hold_badge.set_tooltip_text(Some(
                "Streaming output is held so your selection survives. Copy it, click elsewhere, or wait a few seconds to resume.",
            ));
            hold_badge.set_halign(gtk::Align::Start);
            hold_badge.set_valign(gtk::Align::End);
            hold_badge.set_margin_start(14);
            hold_badge.set_margin_bottom(14);
            hold_badge.set_visible(false);
            hold_badge.set_can_focus(false);
            scroll_overlay.add_overlay(&hold_badge);
            let badge = hold_badge.downgrade();
            selection_feed_hold.set_state_listener(move |parked| {
                if let Some(badge) = badge.upgrade() {
                    badge.set_visible(parked);
                }
            });
        }
        root.append(&scroll_overlay);

        // Cards dock BELOW the surface as a sibling that takes space, not as
        // an overlay: the row an overlay would cover is the prompt the user is
        // typing at. Occupying space costs one grid resize per toggle, which
        // the layout closure turns into a single SIGWINCH.
        let notice_dock = gtk::Box::new(Orientation::Vertical, 0);
        notice_dock.add_css_class("notice-dock");
        notice_dock.set_visible(false);
        root.append(&notice_dock);

        let unread_count: Rc<Cell<u32>> = Rc::new(Cell::new(0));

        // ── PTY ───────────────────────────────────────────────────────────
        let (argv_vec, session_applied) =
            crate::config::shell_argv_with_session(shell_argv, session_id);
        let argv: Vec<&str> = argv_vec.iter().map(|s| s.as_str()).collect();
        let request_shell_token = shell_argv_supports_agent_ids(&argv_vec);

        // Git defaults LESS to "FRX" when the user has not set it. "F" quits
        // the pager when output fits on one screen, and "X" disables less'
        // alternate-screen setup. anvil defaults it to raw-control-char
        // rendering only: keep colored git output, keep the interactive pager
        // even for a short `git log`, and let less use alt-screen so transient
        // pager content stays ephemeral. An explicit user LESS still wins.
        //
        // That choice now lives with the spawn itself, as
        // `child_env::ChildEnv::less_default` in `crate::pty` — one place that
        // also gets it right for the Flatpak bridge, where "the parent has no
        // LESS" is the wrong question to ask about the host session.
        let mut env_extra = Vec::from(crate::terminal::cwd_token_environment(cwd_token));
        let session_id_owned = session_id.map(|s| s.to_string());
        if let Some(ref sid) = session_id_owned {
            if session_applied {
                env_extra.push(("JSH_SESSION_ID", sid.as_str()));
            }
        }

        let pty = Rc::new(OwnedPty::spawn_with_shell_token(
            &argv,
            cwd,
            &env_extra,
            request_shell_token,
        )?);

        // Store child PID on the live VTE so kill_all_terminal_children can find it
        unsafe {
            active_vte.set_data::<i32>("child-pid", pty.pid_i32());
        }

        // ── Register CSS ──────────────────────────────────────────────────
        install_block_css(config);

        // ── Shared state ──────────────────────────────────────────────────
        let bstate = Rc::new(Cell::new(BlockState::Idle));

        // Keystroke-shadow command line; kept only to drive the idle input-cell
        // height (newline count). The authoritative command text is read off the
        // VTE at CommandStart, so this shadow is no longer load-bearing — it
        // does not need to match the rendered line in edge cases.
        let typed_cmd: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let idle_input_dirty: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // VTE cursor position (col, row) right after the prompt finished
        // drawing — anchor for the text-range read at CommandStart.
        let prompt_end_pos: Rc<Cell<(i64, i64)>> = Rc::new(Cell::new((0, 0)));
        let prompt_anchor_rows: Rc<Cell<i64>> = Rc::new(Cell::new(0));
        let prompt_anchor_ready: Rc<Cell<bool>> = Rc::new(Cell::new(false));

        // Derived from the same switch that selects the render backend and
        // copied into its SubmissionSurface: Block's compact/full transition
        // rebases; Unified's one stable full-size grid does not.
        let rebase_prompt_anchor_on_row_delta = prompt_anchor_rebases_on_row_delta(unified);

        // Scroll-lock flags shared across the contents_changed pin, value_changed
        // detector, FAB, and ScrollDebouncer. `user_scrolled_up` suppresses the
        // follow-bottom pin while the user is reading history; `programmatic_scroll`
        // marks our own adjustment writes so the value_changed detector doesn't
        // mistake them for a user drag.
        let user_scrolled_up: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let programmatic_scroll: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // The one coalesced, frame-spaced follow-bottom controller. Shared by
        // the PTY reader and the stranded-focus key recovery below so their
        // settling timers coalesce instead of racing each other.
        let scroll_debouncer = ScrollDebouncer::with_scroll_lock(
            user_scrolled_up.clone(),
            programmatic_scroll.clone(),
        );
        let block_data_rc: Rc<RefCell<VecDeque<BlockData>>> =
            Rc::new(RefCell::new(VecDeque::new()));
        let reserved_history_block_ids: Rc<RefCell<HashSet<u64>>> =
            Rc::new(RefCell::new(HashSet::new()));
        let finished_blocks_rc: Rc<RefCell<Vec<FinishedBlock>>> = Rc::new(RefCell::new(Vec::new()));
        let find_state: Rc<RefCell<FindState>> = Rc::new(RefCell::new(FindState::default()));

        // Paint failures against the full-history track, independently of the
        // currently visible viewport. This is a visual hint only: the narrow
        // overlay deliberately cannot intercept pointer events meant for GTK's
        // scrollbar beneath it.
        let failure_markers = gtk::DrawingArea::new();
        failure_markers.add_css_class("block-failure-markers");
        failure_markers.set_content_width(10);
        failure_markers.set_hexpand(false);
        failure_markers.set_vexpand(true);
        failure_markers.set_halign(gtk::Align::End);
        failure_markers.set_valign(gtk::Align::Fill);
        failure_markers.set_can_target(false);
        {
            let block_data = block_data_rc.clone();
            let scroll = block_scroll.downgrade();
            failure_markers.set_draw_func(move |area, cr, width, height| {
                let Some(scroll) = scroll.upgrade() else {
                    return;
                };
                let adjustment = scroll.vadjustment();
                if width <= 0 || height <= 0 || adjustment.upper() <= adjustment.page_size() + 0.5 {
                    return;
                }

                let color = area.color();
                cr.set_source_rgba(
                    color.red() as f64,
                    color.green() as f64,
                    color.blue() as f64,
                    color.alpha() as f64,
                );
                const MARKER_HEIGHT: f64 = 3.0;
                let span = (f64::from(height) - MARKER_HEIGHT).max(0.0);
                let marker_width = f64::from(width.min(8));
                let marker_x = f64::from(width) - marker_width;
                for fraction in failed_block_marker_fractions(&block_data.borrow()) {
                    cr.rectangle(marker_x, fraction * span, marker_width, MARKER_HEIGHT);
                }
                let _ = cr.fill();
            });
        }
        scroll_overlay.add_overlay(&failure_markers);
        let failure_marker_redraw: FailureMarkerRedraw = {
            let failure_markers = failure_markers.downgrade();
            Rc::new(move || {
                if let Some(failure_markers) = failure_markers.upgrade() {
                    failure_markers.queue_draw();
                }
            })
        };
        {
            let redraw = failure_marker_redraw.clone();
            block_scroll.vadjustment().connect_changed(move |_| {
                redraw();
            });
        }

        // ── Warp-style live-card sizing ───────────────────────────────────
        // The live card hugs its content — prompt plus typed command while idle,
        // and the output produced so far while a command runs — with a
        // guaranteed minimum height, so finished blocks stay on screen and the
        // history pans up a row at a time instead of being shoved off by a
        // page-tall reservation. The card is the full viewport only for
        // alt-screen apps (vim/less/TUIs) and the no-integration fallback.
        //
        // Two heights, not one. `vte.set_size` still gives the terminal the
        // whole viewport for the entire command — that is the winsize the child
        // was told about, and absolute cursor addressing needs those rows to
        // exist — while the card's own height comes from the clip built in
        // `ActiveBlock::new`. The PTY never sees either number: `pty_grid_size`
        // reports the viewport in every state.
        let block_layout_active_surface: Rc<dyn Fn()> = {
            // This callback is retained by adjustment/VTE signals as well as
            // TermView.  Keep every widget edge weak so closing a pane cannot
            // form signal -> callback -> ancestor/terminal reference cycles.
            let holder = active.borrow().widget().downgrade();
            let vte = active_vte.downgrade();
            let scroll = block_scroll.downgrade();
            let bstate = bstate.clone();
            let typed_cmd = typed_cmd.clone();
            let finished_for_layout = finished_blocks_rc.clone();
            let block_data_for_layout = block_data_rc.clone();
            let failure_marker_redraw_for_layout = failure_marker_redraw.clone();
            let last_size_target: Rc<Cell<(i64, i64)>> = Rc::new(Cell::new((0, 0)));
            let last_output_layout: Rc<Cell<FinishedLayoutKey>> = Rc::new(Cell::new((-1, -1, -1)));
            // High-water content extent of the command running now. Owned by
            // ActiveBlock so that `reset_active` — the one funnel every reset
            // path goes through — clears it for the next command.
            let live_rows_high_water = active.borrow().live_extent_rows();
            // The engine-owned capture gates coherent growth after
            // CommandStart. Empty means every VTE sample is still provisional;
            // only the separate parser-barrier flag authorizes a full card.
            let live_output_for_layout = live_raw_output.clone();
            let live_force_full_for_layout = live_extent_force_full.clone();
            // Weak: ActiveBlock owns the VTE that retains this callback.
            let active_for_layout = Rc::downgrade(&active);
            Rc::new(move || {
                let (Some(holder), Some(vte), Some(scroll)) =
                    (holder.upgrade(), vte.upgrade(), scroll.upgrade())
                else {
                    return;
                };
                let Some(active_block) = active_for_layout.upgrade() else {
                    return;
                };
                let cell_h = (vte.char_height() as i32).max(1);
                let Some(viewport_rows) = viewport_rows_for(&vte, &scroll) else {
                    return;
                };
                // Re-fit blocks that are already on screen to a resized pane.
                // Cards that scrolled off and back re-fit themselves on map.
                let fit_finished_outputs = || {
                    let page_height = scroll.vadjustment().page_size() as i32;
                    let page_width = scroll.hadjustment().page_size() as i32;
                    let layout_key = finished_layout_key(page_width, page_height, cell_h);
                    if last_output_layout.replace(layout_key) == layout_key {
                        return;
                    }

                    // Collect first, write after: re-fitting touches GTK
                    // widgets, and holding the metadata borrow across that would
                    // turn a re-entrant layout pass into a RefCell panic.
                    let resized: Vec<(u64, i32)> = {
                        let finished = finished_for_layout.borrow();
                        finished
                            .iter()
                            .filter_map(|block| {
                                block.refit_output_to_viewport().map(|h| (block.id, h))
                            })
                            .collect()
                    };
                    if resized.is_empty() {
                        return;
                    }
                    mutate_block_data_and_redraw(
                        &block_data_for_layout,
                        failure_marker_redraw_for_layout.as_ref(),
                        |block_data| {
                            for (id, height) in resized {
                                if let Some(data) = block_data.iter_mut().find(|data| data.id == id)
                                {
                                    data.estimated_height = height;
                                }
                            }
                        },
                    );
                };
                let cols = vte.column_count().max(1);
                let state = bstate.get();
                holder.set_visible(true);
                if matches!(
                    state,
                    BlockState::CollectingOutput | BlockState::PostCommand
                ) {
                    // The grid stays a full viewport: `pty_grid_size` told the
                    // child that is its winsize, and an application that repaints
                    // by absolute row without taking the alternate screen (`top`,
                    // `watch`, a plain `clear`) needs every one of those rows to
                    // draw into.
                    let target = (cols, viewport_rows);
                    if last_size_target.get() != target {
                        vte.set_size(cols, viewport_rows);
                        last_size_target.set(target);
                    }
                    // The card, though, is only as tall as the output so far, so
                    // the blocks above stay on screen and the history pans up a
                    // row at a time instead of being shoved off by a page-tall
                    // reservation the command may never fill.
                    let output_started = !live_output_for_layout.borrow().is_empty();
                    let (visible_rows, next_high_water) = live_visible_rows_for_measurement(
                        live_content_extent(&vte),
                        output_started,
                        live_force_full_for_layout.get(),
                        live_rows_high_water.get(),
                        viewport_rows,
                    );
                    live_rows_high_water.set(next_high_water);
                    holder.set_height_request((visible_rows as i32) * cell_h);
                    if let Ok(active_block) = active_block.try_borrow() {
                        active_block.set_live_geometry(cell_h, viewport_rows, visible_rows);
                    }
                    fit_finished_outputs();
                    return;
                }
                // Not running. The next command starts from the prompt's height
                // again — except on the way out of a screen application, where
                // the restored primary screen is already full of content the
                // card has to keep showing.
                let compact_rows = || {
                    let input_lines =
                        1 + typed_cmd.borrow().bytes().filter(|&b| b == b'\n').count() as i64;
                    let floor = (MIN_INPUT_ROWS as i64).min(viewport_rows);
                    let cap = viewport_rows.max(floor);
                    input_lines.clamp(floor, cap)
                };
                let target_rows = match state {
                    // A real terminal's grid is the viewport, always. While a
                    // alt-screen app owns the screen, or while we fall back to
                    // raw VTE (no OSC-133), keep the live VTE pinned to the full
                    // viewport. Normal command output is rendered into the
                    // running block above, so the live input cell can stay
                    // compact instead of stealing the page.
                    BlockState::AltScreen | BlockState::RawFallback => viewport_rows,
                    // Between prompts the live cell collapses to fit the
                    // typed command (warp-style compact input). Must NOT use
                    // cursor_position().1 here: it's the absolute scrollback
                    // row and climbs without bound across the session.
                    BlockState::Idle
                    | BlockState::CollectingPrompt
                    | BlockState::AwaitingCommand => compact_rows(),
                    BlockState::CollectingOutput | BlockState::PostCommand => viewport_rows,
                };
                // Preserve the compact input height across CommandStart. VTE's
                // cursor/adjustment pair may still describe different grid
                // generations until the first real output arrives, so the
                // running branch needs a trustworthy baseline of its own.
                live_rows_high_water.set(target_rows);
                // Drive the VTE grid directly. `set_height_request` only sets a
                // *minimum*, so it cannot shrink a VTE whose natural height
                // (row_count * char_height) is larger — the cell would stay
                // full-height. `set_size` sets the preferred grid, shrinking the
                // VTE's natural height so the (non-expanding) holder collapses to
                // it. The PTY-resize tick then follows row_count up/down.
                let target = (cols, target_rows);
                if last_size_target.get() != target {
                    vte.set_size(cols, target_rows);
                    last_size_target.set(target);
                }
                holder.set_height_request((target_rows as i32) * cell_h);
                // Nothing to clip outside a running command: the card and the
                // grid are the same height at the prompt, on the alternate
                // screen and in the no-integration fallback.
                if let Ok(live) = active_block.try_borrow() {
                    live.set_live_geometry(cell_h, target_rows, target_rows);
                }
                fit_finished_outputs();
            })
        };
        // Unified has one layout state: a viewport-sized VTE for prompts,
        // commands and alternate-screen applications alike.
        let unified_layout_active_surface: Rc<dyn Fn()> = {
            let holder = active.borrow().widget().downgrade();
            let vte = active_vte.downgrade();
            let scroll = block_scroll.downgrade();
            let last_size_target: Rc<Cell<(i64, i64)>> = Rc::new(Cell::new((0, 0)));
            let active_for_layout = Rc::downgrade(&active);
            Rc::new(move || {
                let (Some(holder), Some(vte), Some(scroll)) =
                    (holder.upgrade(), vte.upgrade(), scroll.upgrade())
                else {
                    return;
                };
                let Some(active_block) = active_for_layout.upgrade() else {
                    return;
                };
                let cell_h = (vte.char_height() as i32).max(1);
                let Some(viewport_rows) = viewport_rows_for(&vte, &scroll) else {
                    return;
                };
                let target = (vte.column_count().max(1), viewport_rows);
                holder.set_visible(true);
                if last_size_target.get() != target {
                    vte.set_size(target.0, target.1);
                    last_size_target.set(target);
                }
                holder.set_height_request((viewport_rows as i32) * cell_h);
                // Unified has one surface and one height; the clip is a no-op
                // here, but the terminal still needs its size request or the
                // Fixed would allocate it its bare minimum.
                if let Ok(live) = active_block.try_borrow() {
                    live.set_live_geometry(cell_h, viewport_rows, viewport_rows);
                };
            })
        };
        let layout_active_surface: Rc<dyn Fn()> = if unified {
            unified_layout_active_surface
        } else {
            block_layout_active_surface
        };

        // Coalesces follow-bottom pins so a burst of contents-changed signals
        // schedules at most one deferred scroll.
        let pin_pending: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let contents_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        {
            // Drive sizing from the data path (contents changed: prompt printed,
            // user typing, output streaming, alt-screen toggle), and follow the
            // bottom from here too — NOT from the vadjustment `changed` signal.
            //
            // Why a deferred idle and not `changed`: pinning inside `changed`
            // reacts to virtualization's own `upper` changes (off-screen blocks
            // collapse to 0 height when hidden), so pin → hide top block → upper
            // shrinks → `changed` → pin → block reappears → upper grows → `changed`
            // → … an infinite two-state oscillation. A low-priority idle runs once
            // per content burst, AFTER layout settles (so `upper` is final), and is
            // never re-triggered by the visibility side-effects of its own scroll.
            let f = layout_active_surface.clone();
            let scroll = block_scroll.downgrade();
            let user_scrolled = user_scrolled_up.clone();
            let programmatic = programmatic_scroll.clone();
            let pin_pending = pin_pending.clone();
            let contents_generation_for_signal = contents_generation.clone();
            active_vte.connect_contents_changed(move |_| {
                contents_generation_for_signal
                    .set(contents_generation_for_signal.get().wrapping_add(1));
                f();
                if user_scrolled.get() || pin_pending.get() {
                    return;
                }
                pin_pending.set(true);
                let scroll = scroll.clone();
                let user_scrolled = user_scrolled.clone();
                let programmatic = programmatic.clone();
                let pin_pending = pin_pending.clone();
                glib::idle_add_local_once(move || {
                    pin_pending.set(false);
                    if user_scrolled.get() {
                        return;
                    }
                    let Some(scroll) = scroll.upgrade() else {
                        return;
                    };
                    let adj = scroll.vadjustment();
                    let target = (adj.upper() - adj.page_size()).max(adj.lower());
                    if (adj.value() - target).abs() > 1.0 {
                        programmatic.set(true);
                        adj.set_value(target);
                        programmatic.set(false);
                    }
                });
            });
        }

        // True while an alt-screen app owns the viewport (finished blocks hidden).
        let fullscreen: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let cwd_callbacks: CwdCallbacks = Rc::new(RefCell::new(vec![]));
        let remote_session_callbacks: StrCallbacks = Rc::new(RefCell::new(vec![]));
        let exited_callbacks: IntCallbacks = Rc::new(RefCell::new(vec![]));
        let bell_callbacks: VoidCallbacks = Rc::new(RefCell::new(vec![]));
        // Bell signal is delivered natively by VTE — no need to scan the byte
        // stream for BEL ourselves (and disambiguate it from OSC string
        // terminators). VTE already does that disambiguation inside its parser.
        {
            let bell_cbs = bell_callbacks.clone();
            active_vte.connect_bell(move |_| {
                for cb in bell_cbs.borrow().iter() {
                    cb();
                }
            });
        }
        let title_callbacks: StrCallbacks = Rc::new(RefCell::new(vec![]));
        let activity_callbacks: VoidCallbacks = Rc::new(RefCell::new(vec![]));
        let human_input_callbacks: HumanInputCallbacks = Rc::new(RefCell::new(vec![]));
        let alt_screen_callbacks: AltScreenCallbacks = Rc::new(RefCell::new(vec![]));
        let command_started_callbacks: CommandStartedCallbacks = Rc::new(RefCell::new(vec![]));
        let command_finished_callbacks: CommandFinishedCallbacks = Rc::new(RefCell::new(vec![]));
        let block_finished_callbacks: BlockFinishedCallbacks = Rc::new(RefCell::new(vec![]));
        let ask_ai_about_block_callbacks: BlockContextCallbacks = Rc::new(RefCell::new(vec![]));
        let mouse_reporting_mode: Rc<Cell<MouseReportingMode>> =
            Rc::new(Cell::new(MouseReportingMode::None));
        // Unlike a regular VTE terminal, block mode owns the shell PTY. Keep
        // DECSET 2004 state here so clipboard pastes can be forwarded as one
        // ordered byte stream instead of relying on VTE's unrelated PTY.
        let bracketed_paste: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // Dynamic OSC 10/11/12 overrides are recorded by the reader loop but
        // also read by TermView (block rebuilds) and cleared by it (theme
        // switch), so the cell is created here and shared with ReaderCtx.
        let dynamic_colors: DynamicColorsRc = Rc::new(Cell::new(DynamicColors::default()));
        // A finished-block sticky header behaves like Warp's: click it to return
        // to the command at the top of the oversized block.
        {
            let target = sticky_target_id.clone();
            let finished = finished_blocks_rc.clone();
            let scroll = block_scroll.clone();
            let click = gtk::GestureClick::new();
            click.set_button(1);
            click.connect_released(move |_, n_press, _, _| {
                if n_press != 1 {
                    return;
                }
                let Some(id) = target.get() else {
                    return;
                };
                let finished = finished.borrow();
                let Some(block) = finished.iter().find(|block| block.id == id) else {
                    return;
                };
                block.scroll_to_edge(&scroll, false);
            });
            sticky_label.add_controller(click);
        }
        {
            let target = sticky_target_id.clone();
            let finished = finished_blocks_rc.clone();
            let scroll = block_scroll.clone();
            sticky_jump_bottom_btn.connect_clicked(move |_| {
                let Some(id) = target.get() else {
                    return;
                };
                let finished = finished.borrow();
                let Some(block) = finished.iter().find(|block| block.id == id) else {
                    return;
                };
                block.scroll_to_edge(&scroll, true);
            });
        }

        let execution_id: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

        let widget_pool: Rc<RefCell<WidgetPool>> = Rc::new(RefCell::new(WidgetPool::new()));
        let pty_synced: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let selected_block_ids: SelectedBlockIds =
            Rc::new(RefCell::new(std::collections::HashSet::new()));
        let selected_block_id: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));
        let selection_anchor_id: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));
        // Bookmarked block ids (in-memory for the session). Toggled with Ctrl+B;
        // navigated with Ctrl+,/Ctrl+.. Not persisted (avoids an rkyv schema bump).
        let block_bookmarks: Rc<RefCell<std::collections::HashSet<u64>>> =
            Rc::new(RefCell::new(std::collections::HashSet::new()));
        // Sticky running-command header state: true while a command is executing,
        // plus the command text captured at CommandStart.
        let cmd_running: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let running_cmd: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let armed_agent_execution: Rc<RefCell<Option<ArmedAgentExecution>>> =
            Rc::new(RefCell::new(None));
        let agent_prompt_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        let active_agent_execution: Rc<Cell<Option<crate::agent::AgentExecutionRef>>> =
            Rc::new(Cell::new(None));
        let agent_execution_supported: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let agent_execution_lost_callbacks: AgentExecutionLostCallbacks =
            Rc::new(RefCell::new(Vec::new()));
        let reviewed_submission: Rc<RefCell<Option<ReviewedSubmission>>> =
            Rc::new(RefCell::new(None));
        let verified_submission_source_id: Rc<RefCell<Option<glib::SourceId>>> =
            Rc::new(RefCell::new(None));
        let verified_submission = VerifiedSubmissionCtx {
            surface: Rc::new(VteSubmissionSurface {
                vte: active_vte.clone(),
                rebase_on_row_delta: rebase_prompt_anchor_on_row_delta,
            }),
            bstate: bstate.clone(),
            pty: pty.clone(),
            typed_cmd: typed_cmd.clone(),
            idle_input_dirty: idle_input_dirty.clone(),
            pty_synced: pty_synced.clone(),
            prompt_end_pos: prompt_end_pos.clone(),
            prompt_anchor_rows: prompt_anchor_rows.clone(),
            prompt_anchor_ready: prompt_anchor_ready.clone(),
            prompt_generation: agent_prompt_generation.clone(),
            contents_generation: contents_generation.clone(),
            submission: reviewed_submission,
            source_id: verified_submission_source_id,
            armed_agent_execution: armed_agent_execution.clone(),
            agent_execution_supported: agent_execution_supported.clone(),
            agent_execution_lost_callbacks,
        };
        let block_start_time: Rc<Cell<Option<SystemTime>>> = Rc::new(Cell::new(None));
        let visible_indices: Rc<RefCell<std::collections::HashSet<usize>>> =
            Rc::new(RefCell::new(std::collections::HashSet::new()));
        // Set once any OSC-133 (FTCS) event is seen, so the view knows shell
        // integration is live.
        let ftcs_seen: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let current_cwd: Rc<RefCell<String>> = Rc::new(RefCell::new(cwd.unwrap_or("").to_string()));
        let current_cwd_external = Rc::new(Cell::new(cwd_external));

        // CWD updates come from VTE's native OSC 7 signal (the parser passes
        // OSC 7 through unchanged, see parser.rs). Title updates likewise come
        // from VTE's window-title-changed (OSC 0/2).
        {
            let cwd_cbs = cwd_callbacks.clone();
            let current_cwd_for_signal = current_cwd.clone();
            let current_cwd_external_for_signal = current_cwd_external.clone();
            let vte_for_cwd = active_vte.clone();
            let pty_for_cwd = pty.clone();
            let cwd_token_for_signal = cwd_token.to_string();
            active_vte.connect_current_directory_uri_notify(move |_| {
                if let Some(uri) = vte_for_cwd.current_directory_uri() {
                    if let Ok((path, host)) = glib::filename_from_uri(uri.as_str()) {
                        let path = path.to_string_lossy().to_string();
                        if path.is_empty() {
                            return;
                        }
                        let authority = crate::terminal::classify_cwd_authority(
                            host.as_deref(),
                            &cwd_token_for_signal,
                        );
                        let foreground_external = crate::process::foreground_uses_external_cwd(
                            pty_for_cwd.master_fd_raw(),
                            pty_for_cwd.pid_i32(),
                        );
                        let external =
                            crate::terminal::resolve_cwd_external(authority, foreground_external);
                        current_cwd_external_for_signal.set(external);
                        *current_cwd_for_signal.borrow_mut() = path.clone();
                        for cb in cwd_cbs.borrow().iter() {
                            cb(&path, external);
                        }
                    }
                }
            });
        }

        {
            let title_cbs = title_callbacks.clone();
            let vte_for_title = active_vte.clone();
            active_vte.connect_window_title_changed(move |_| {
                if let Some(title) = vte_for_title.window_title() {
                    let title_str = title.to_string();
                    if !title_str.is_empty() {
                        for cb in title_cbs.borrow().iter() {
                            cb(&title_str);
                        }
                    }
                }
            });
        }

        // One shared cell for the runtime Config: TermView.config and the
        // reader/finalize path must alias, or reload_config / set_font /
        // set_font_scale would update a copy the reader callbacks never see.
        // The three borrow_mut sites (reload_config, set_font, set_font_scale)
        // are statement-scoped and run from UI actions outside reader dispatch.
        // Reader-path borrows may span calls only into code that cannot reach a
        // borrow_mut of this cell (height estimation, finished_block_config,
        // FinishedBlock::new_with_pool, the OSC color-reply builder,
        // notify::long_block_finished and command_history::enqueue are all
        // non-reentrant). Parser flags (mouse/focus reporting) are NOT covered:
        // ParserConfig below snapshots them at construction.
        let config_shared: Rc<RefCell<Config>> = Rc::new(RefCell::new(config.clone()));

        // ── Wire PTY → parser → block events ─────────────────────────────
        let render_backend_slot: Rc<RefCell<Option<Rc<dyn RenderBackend>>>> =
            Rc::new(RefCell::new(None));
        {
            let active_rc = active.clone();
            let active_vte_rc = active_vte.clone();
            let bstate_rc = bstate.clone();
            let typed_cmd_rc = typed_cmd.clone();
            let prompt_end_pos_rc = prompt_end_pos.clone();
            let prompt_anchor_rows_rc = prompt_anchor_rows.clone();
            let block_list_rc = block_list.clone();
            let block_scroll_rc = block_scroll.clone();
            let exited_cbs = exited_callbacks.clone();
            let activity_cbs = activity_callbacks.clone();
            let alt_screen_cbs = alt_screen_callbacks.clone();
            let mouse_reporting_rc = mouse_reporting_mode.clone();
            let bracketed_paste_rc = bracketed_paste.clone();
            let dynamic_colors_rc = dynamic_colors.clone();
            let config_for_cb = config_shared.clone();
            let parser = Rc::new(RefCell::new(Parser::with_config(ParserConfig {
                mouse_reporting: config.mouse_reporting_enabled,
                focus_reporting: config.focus_reporting_enabled,
            })));
            let block_data_for_cb = block_data_rc.clone();
            let finished_blocks_for_cb = finished_blocks_rc.clone();
            let scroll_debouncer = scroll_debouncer.clone();
            let widget_pool_for_cb = widget_pool.clone();
            let pty_synced_rc = pty_synced.clone();
            let visible_indices_rc = visible_indices.clone();
            let fullscreen_rc = fullscreen.clone();
            let ftcs_seen_rc = ftcs_seen.clone();

            // Command queue for replaying initial_commands on PromptEnd events
            let init_cmds_queue: Rc<RefCell<std::collections::VecDeque<String>>> =
                Rc::new(RefCell::new(initial_commands.iter().cloned().collect()));
            let init_cmds_queue_for_cb = Rc::clone(&init_cmds_queue);
            let pty_for_init = Rc::clone(&pty);
            let block_start_time_for_cb = block_start_time.clone();
            let current_cwd_for_cb = current_cwd.clone();

            let event_buf: Rc<RefCell<Vec<ParserEvent>>> =
                Rc::new(RefCell::new(Vec::with_capacity(32)));
            // This is the only reader-path mode switch. The PTY, parser,
            // lifecycle engine and all state above and below it are shared.
            let backend: Rc<dyn RenderBackend> = if unified {
                let zone_marker = Rc::new(RefCell::new(ZoneMarkerInjector::from_system_entropy()));
                let chrome_authority = Rc::new(RefCell::new(
                    unified_chrome::ZoneChromeAuthority::new(zone_marker.borrow().nonce()),
                ));
                let chrome = unified_chrome::UnifiedChrome::new(
                    &active_vte_rc,
                    &active_rc.borrow().unified_chrome_surface,
                    chrome_authority,
                    config_for_cb.clone(),
                );
                active_rc.borrow().unified_chrome_surface.set_visible(true);
                Rc::new(UnifiedBackend {
                    vte: active_vte_rc,
                    rebase_prompt_anchor_on_row_delta,
                    active_rc,
                    block_scroll_rc,
                    layout_active_surface: layout_active_surface.clone(),
                    config_for_cb: config_for_cb.clone(),
                    pty_for_init: pty_for_init.clone(),
                    zones: unified_zones.clone(),
                    find_state_for_cb: find_state.clone(),
                    zone_marker,
                    chrome,
                    reserved_history_block_ids: reserved_history_block_ids.clone(),
                    kitty_assembler: RefCell::new(kitty_graphics::Assembler::new()),
                })
            } else {
                Rc::new(BlockBackend {
                    active_rc,
                    active_vte: active_vte_rc,
                    // Same construction-time policy passed to the submission
                    // surface above, so every prompt-anchor reader agrees.
                    rebase_prompt_anchor_on_row_delta,
                    block_list_rc,
                    block_scroll_rc,
                    jump_fab: jump_fab.clone(),
                    scroll_debouncer,
                    failure_marker_redraw: failure_marker_redraw.clone(),
                    finished_blocks_for_cb,
                    widget_pool_for_cb,
                    find_state_rc: find_state.clone(),
                    visible_indices_rc,
                    fullscreen_rc,
                    selected_block_ids_rc: selected_block_ids.clone(),
                    selected_block_id_rc: selected_block_id.clone(),
                    selection_anchor_id_rc: selection_anchor_id.clone(),
                    bookmarks_rc: block_bookmarks.clone(),
                    block_data_for_cb,
                    unread_count_rc: unread_count.clone(),
                    layout_active_surface: layout_active_surface.clone(),
                    config_for_cb: config_for_cb.clone(),
                    dynamic_colors_rc: dynamic_colors_rc.clone(),
                    pty_for_init: pty_for_init.clone(),
                    ask_ai_about_block_cbs: ask_ai_about_block_callbacks.clone(),
                    bstate_rc: bstate_rc.clone(),
                    typed_cmd_rc: typed_cmd_rc.clone(),
                    armed_agent_execution_rc: armed_agent_execution.clone(),
                    bracketed_paste_rc: bracketed_paste_rc.clone(),
                    pty_synced_rc: pty_synced_rc.clone(),
                    kitty_assembler: RefCell::new(kitty_graphics::Assembler::new()),
                    kitty_pending_images: RefCell::new(Vec::new()),
                    kitty_pending_bytes: Cell::new(0),
                    kitty_pending_admission: RefCell::new(None),
                })
            };
            *render_backend_slot.borrow_mut() = Some(backend.clone());
            ReaderCtx {
                backend,
                bstate_rc,
                engine: RefCell::new(EngineState {
                    prev_state: BlockState::Idle,
                    osc133_depth: 0,
                    prompt_buf: String::new(),
                    background_output: VecDeque::new(),
                    background_output_dropped_front: false,
                    vte_typed_cmd: String::new(),
                    prompt_display: String::new(),
                    // `None` — the initial value and the value for a shell that
                    // omits the status — means "not reported", which the block
                    // header renders as neutral rather than as `exit 0`.
                    pending_exit_code: None,
                    // Metadata jsh attaches to the same marks (see
                    // ParserEvent::CommandStart).
                    shell_duration_ms: None,
                    execution_id_trusted: false,
                    agent_completion_trusted: false,
                    command_cwd: None,
                    pending_zone: None,
                    active_alt_screen_mode: None,
                }),
                live_raw_output_rc: live_raw_output.clone(),
                live_raw_output_dropped_rc: live_raw_output_dropped.clone(),
                live_extent_force_full_rc: live_extent_force_full.clone(),
                typed_cmd_rc,
                idle_input_dirty_rc: idle_input_dirty.clone(),
                prompt_end_pos_rc,
                prompt_anchor_rows_rc,
                prompt_anchor_ready_rc: prompt_anchor_ready.clone(),
                remote_session_cbs: remote_session_callbacks.clone(),
                exited_cbs,
                activity_cbs,
                alt_screen_cbs,
                command_started_cbs: command_started_callbacks.clone(),
                command_finished_cbs: command_finished_callbacks.clone(),
                mouse_reporting_rc,
                bracketed_paste_rc,
                dynamic_colors_rc,
                config_for_cb,
                parser,
                capability_observer: RefCell::new(ShellCapabilityObserver::default()),
                shell_capability_token: pty_for_init
                    .shell_integration_token()
                    .unwrap_or_default()
                    .to_string(),
                reset_splitter: RefCell::new(ResetAwareParserSplitter::default()),
                reserved_history_block_ids: reserved_history_block_ids.clone(),
                pty_synced_rc,
                ftcs_seen_rc,
                init_cmds_queue_for_cb,
                pty_for_init,
                block_start_time_for_cb,
                execution_id_rc: execution_id.clone(),
                current_cwd_for_cb,
                event_buf,
                cmd_running_rc: cmd_running.clone(),
                running_cmd_rc: running_cmd.clone(),
                armed_agent_execution_rc: armed_agent_execution.clone(),
                agent_prompt_generation_rc: agent_prompt_generation.clone(),
                active_agent_execution_rc: active_agent_execution.clone(),
                agent_execution_supported_rc: agent_execution_supported.clone(),
                verified_submission: verified_submission.clone(),
                block_finished_cbs: block_finished_callbacks.clone(),
                selection_feed_hold: selection_feed_hold.clone(),
            }
            .install(&pty, &active_vte)?;

            // Shells without OSC 133 never emit PromptEnd, so the normal
            // integration-driven replay path above would leave startup/session
            // commands queued forever. After the same grace period used by the
            // VTE backend, drain the shared queue only if no integration marker
            // has appeared. PromptEnd and this fallback therefore cannot send
            // the same command twice.
            if !init_cmds_queue.borrow().is_empty() {
                let init_cmds_fallback = Rc::clone(&init_cmds_queue);
                let ftcs_seen_fallback = ftcs_seen.clone();
                let pty_fallback = Rc::downgrade(&pty);
                glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                    if ftcs_seen_fallback.get() {
                        return;
                    }
                    let Some(pty) = pty_fallback.upgrade() else {
                        return;
                    };
                    for command in init_cmds_fallback.borrow_mut().drain(..) {
                        pty.write_bytes(format!("{command}\r").as_bytes());
                    }
                });
            }
        }

        // ── Scroll lock + jump-to-bottom FAB ──────────────────────────────
        // The block list virtualizes (off-screen finished blocks are hidden →
        // 0 height), so `adjustment.upper()` shrinks as you scroll and the usual
        // value-vs-max "at bottom" math can never be trusted. Instead detect the
        // live bottom geometrically off the never-virtualized live VTE holder.
        //
        // Key subtlety (see scroll.rs): in the normal follow state the holder is
        // one full page tall and parked at its *top*, so its top edge sits a little
        // below y=0 (≈ the just-finished block's height) and its bottom edge falls
        // *below* the viewport. So neither "top≈0" nor "bottom inside viewport"
        // alone is right. What actually distinguishes "following" from "scrolled
        // up into history" is whether the live prompt is still on screen: while
        // following, the holder's top is somewhere inside the viewport; scroll up
        // far enough and the holder (prompt) slides off the bottom. So: at-bottom
        // ⟺ holder top is above the viewport's bottom edge. Sampled on idle so it
        // reflects the settled post-scroll layout.
        {
            let user_scrolled = user_scrolled_up.clone();
            let fab = jump_fab.clone();
            let unread = unread_count.clone();
            let scroll = block_scroll.downgrade();
            let holder = active.borrow().widget().downgrade();
            let programmatic_scroll = programmatic_scroll.clone();
            let check_pending = Rc::new(Cell::new(false));
            let pending_programmatic_only = Rc::new(Cell::new(true));
            block_scroll
                .vadjustment()
                .connect_value_changed(move |_adj| {
                    // `set_value()` emits this synchronously, while the geometry
                    // check below deliberately runs on idle. Preserve the source
                    // now: otherwise the programmatic flag has been cleared by
                    // the time the idle runs and a follow-bottom pin is mistaken
                    // for the user scrolling into history.
                    let caused_by_programmatic_scroll = programmatic_scroll.get();
                    if check_pending.get() {
                        if !caused_by_programmatic_scroll {
                            pending_programmatic_only.set(false);
                        }
                        return;
                    }
                    check_pending.set(true);
                    pending_programmatic_only.set(caused_by_programmatic_scroll);
                    let user_scrolled = user_scrolled.clone();
                    let fab = fab.clone();
                    let unread = unread.clone();
                    let scroll = scroll.clone();
                    let holder = holder.clone();
                    let check_pending = check_pending.clone();
                    let pending_programmatic_only = pending_programmatic_only.clone();
                    glib::idle_add_local_once(move || {
                        check_pending.set(false);
                        if pending_programmatic_only.replace(true) {
                            return;
                        }
                        let (Some(scroll), Some(holder)) = (scroll.upgrade(), holder.upgrade())
                        else {
                            return;
                        };
                        let vp_h = scroll.height() as f64;
                        let at_bottom = holder
                            .compute_bounds(&scroll)
                            .map(|b| (b.y() as f64) < vp_h - 4.0)
                            .unwrap_or(true);
                        user_scrolled.set(!at_bottom);
                        if at_bottom {
                            unread.set(0);
                            fab.set_visible(false);
                        } else {
                            set_jump_fab_label(&fab, unread.get());
                            fab.set_visible(true);
                        }
                    });
                });
        }

        // ── Unified scroll-position detector ──────────────────────────────
        // The outer scroller holds one full-viewport surface, so its
        // adjustment never moves and the detector above never fires. Reading
        // history here means scrolling the VTE's own adjustment, which is what
        // must drive the same flag: the sticky running header and the jump FAB
        // both key off it, and both were unreachable in this mode without it.
        if unified {
            let user_scrolled = user_scrolled_up.clone();
            let fab = jump_fab.downgrade();
            let unread = unread_count.clone();
            let fullscreen = fullscreen.clone();
            if let Some(adjustment) = gtk::prelude::ScrollableExt::vadjustment(&active_vte) {
                adjustment.connect_value_changed(move |adj| {
                    let Some(fab) = fab.upgrade() else {
                        return;
                    };
                    if fullscreen.get() {
                        user_scrolled.set(false);
                        unread.set(0);
                        fab.set_visible(false);
                        return;
                    }
                    // One row of slack: a partially scrolled last row still
                    // counts as following the bottom.
                    let at_bottom = adj.value() >= adj.upper() - adj.page_size() - 1.0;
                    user_scrolled.set(!at_bottom);
                    if at_bottom {
                        unread.set(0);
                        fab.set_visible(false);
                    } else {
                        set_jump_fab_label(&fab, unread.get());
                        fab.set_visible(true);
                    }
                });
            }
        }

        // ── Re-clamp input height on viewport resize ──────────────────────
        // `changed` fires during the viewport's size-allocate, after layout. We
        // re-clamp the input height here ONLY when the viewport itself resized
        // (page_size moved) — content-driven sizing comes from the data path
        // (contents_changed) above. We deliberately do NOT pin the scroll here:
        // pinning from `changed` reacts to virtualization's own `upper` changes
        // (hidden off-screen blocks collapse to 0 height) and oscillates forever.
        // The follow-bottom pin is the deferred idle scheduled on contents_changed.
        {
            let f = layout_active_surface.clone();
            let last_page = Rc::new(Cell::new(0.0f64));
            block_scroll.vadjustment().connect_changed(move |adj| {
                let page = adj.page_size();
                if (page - last_page.get()).abs() > 0.5 {
                    last_page.set(page);
                    f();
                }
            });
        }
        // Width-only split changes leave the vertical page untouched but alter
        // finished VTE wrapping, so they need the same geometry refit sweep.
        {
            let f = layout_active_surface.clone();
            let last_page = Rc::new(Cell::new(0.0f64));
            block_scroll
                .hadjustment()
                .connect_changed(move |adjustment| {
                    let page = adjustment.page_size();
                    if (page - last_page.get()).abs() > 0.5 {
                        last_page.set(page);
                        f();
                    }
                });
        }

        // ── Jump-to-bottom FAB click: return to the live prompt ───────────
        {
            let scroll = block_scroll.clone();
            let programmatic = programmatic_scroll.clone();
            let user_scrolled = user_scrolled_up.clone();
            let unread = unread_count.clone();
            let fab = jump_fab.clone();
            let live_vte = active_vte.downgrade();
            jump_fab.connect_clicked(move |_| {
                // Returning to the live prompt is not a single set_value: blocks
                // below the viewport are virtualized to 0 height, so `upper` only
                // grows as they scroll into view. One jump lands partway; we have
                // to re-apply `upper - page` across idle passes until `upper` stops
                // growing (true bottom reached) or we hit a small iteration cap.
                user_scrolled.set(false);
                unread.set(0);
                fab.set_visible(false);
                if let Some(adjustment) = live_vte
                    .upgrade()
                    .and_then(|terminal| terminal.vadjustment())
                {
                    adjustment.set_value(
                        (adjustment.upper() - adjustment.page_size()).max(adjustment.lower()),
                    );
                }
                let adj = scroll.vadjustment();
                programmatic.set(true);
                adj.set_value((adj.upper() - adj.page_size()).max(adj.lower()));
                programmatic.set(false);

                let scroll = scroll.clone();
                let programmatic = programmatic.clone();
                let tries = Rc::new(Cell::new(0u8));
                glib::idle_add_local(move || {
                    // Runs for a handful of frames (cap below), too fast for the
                    // user to interrupt — so we don't watch user_scrolled here; the
                    // value_changed geometry check settles the FAB state afterward.
                    if tries.get() >= 12 {
                        return glib::ControlFlow::Break;
                    }
                    tries.set(tries.get() + 1);
                    let adj = scroll.vadjustment();
                    let before = adj.value();
                    let target = (adj.upper() - adj.page_size()).max(adj.lower());
                    programmatic.set(true);
                    adj.set_value(target);
                    programmatic.set(false);
                    // Stable once another pass no longer advances the position.
                    if (adj.value() - before).abs() < 1.0 {
                        glib::ControlFlow::Break
                    } else {
                        glib::ControlFlow::Continue
                    }
                });
            });
        }

        // ── Sticky command header ────────────────────────────────────────
        // Running commands keep their existing status header while the user reads
        // history. Finished oversized blocks pin their command when the original
        // header has scrolled above the viewport but the block still spans it.
        {
            let pty = pty.clone();
            let human_input = human_input_callbacks.clone();
            let selection_feed_hold = selection_feed_hold.clone();
            sticky_stop_btn.connect_clicked(move |_| {
                selection_feed_hold.flush_now();
                pty.write_bytes(b"\x03");
                emit_human_input(&human_input, HumanInputKind::StickyStop);
            });
        }
        let sticky_timer_id = {
            let sticky = sticky_bar.clone();
            let sticky_label = sticky_label.clone();
            let sticky_jump_bottom = sticky_jump_bottom_btn.clone();
            let sticky_stop = sticky_stop_btn.clone();
            let sticky_organism = sticky_organism_slot.clone();
            let sticky_target = sticky_target_id.clone();
            let sticky_minimized = sticky_minimized.clone();
            let cmd_running = cmd_running.clone();
            let running_cmd = running_cmd.clone();
            let block_start_time = block_start_time.clone();
            let user_scrolled = user_scrolled_up.clone();
            let finished = finished_blocks_rc.clone();
            let scroll = block_scroll.clone();
            let fullscreen = fullscreen.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
                if sticky.parent().is_none() {
                    return glib::ControlFlow::Break;
                }

                let minimized = sticky_minimized.get();
                if fullscreen.get() {
                    sticky_target.set(None);
                    sticky_jump_bottom.set_visible(false);
                    sticky_stop.set_visible(false);
                    sticky_organism.set_visible(false);
                    sticky.set_visible(false);
                    return glib::ControlFlow::Continue;
                }
                // At the live prompt there is no sticky header to compute. Avoid
                // walking every finished block and querying GTK geometry on a
                // permanent timer while the terminal is idle.
                if !user_scrolled.get() {
                    sticky_target.set(None);
                    sticky_jump_bottom.set_visible(false);
                    sticky_stop.set_visible(false);
                    sticky_organism.set_visible(false);
                    sticky.set_visible(false);
                    return glib::ControlFlow::Continue;
                }

                if cmd_running.get() {
                    sticky_target.set(None);
                    sticky_jump_bottom.set_visible(false);
                    sticky_stop.set_visible(!minimized);
                    sticky_organism.set_visible(
                        sticky_organism
                            .first_child()
                            .is_some_and(|child| child.is_visible()),
                    );
                    let cmd = running_cmd.borrow();
                    let cmd_disp = crate::review_input::safe_inline_display(cmd.trim(), 1024);
                    let elapsed = block_start_time
                        .get()
                        .and_then(|st| SystemTime::now().duration_since(st).ok())
                        .map(|duration| duration.as_secs())
                        .unwrap_or(0);
                    let elapsed_str = if elapsed >= 3600 {
                        format!("{}h{:02}m", elapsed / 3600, (elapsed % 3600) / 60)
                    } else if elapsed >= 60 {
                        format!("{}m{:02}s", elapsed / 60, elapsed % 60)
                    } else {
                        format!("{}s", elapsed)
                    };
                    let label = if cmd_disp.is_empty() {
                        format!("\u{25b6}  (running)    {}", elapsed_str)
                    } else {
                        format!("\u{25b6}  {}    {}", cmd_disp, elapsed_str)
                    };
                    sticky_label.set_text(&label);
                    sticky_label.set_visible(!minimized);
                    sticky.set_visible(true);
                    return glib::ControlFlow::Continue;
                }

                let sticky_height = sticky.height().max(1) as f32;
                let candidate = finished.borrow().iter().find_map(|block| {
                    let header = block.header_row.compute_bounds(&scroll)?;
                    let card = block.widget().compute_bounds(&scroll)?;
                    let header_bottom = header.y() + header.height();
                    let card_bottom = card.y() + card.height();
                    if header_bottom <= 0.0 && card_bottom > sticky_height + 4.0 {
                        let command = block.cmd_text.lines().next().unwrap_or("").trim();
                        let command = crate::review_input::safe_inline_display(command, 1024);
                        Some((block.id, command, block.long_output))
                    } else {
                        None
                    }
                });

                if let Some((id, command, long_output)) = candidate {
                    sticky_target.set(Some(id));
                    sticky_stop.set_visible(false);
                    sticky_organism.set_visible(false);
                    let command = if command.is_empty() {
                        "Background output".to_string()
                    } else {
                        command
                    };
                    sticky_label.set_text(&format!("\u{276f}  {}", command));
                    sticky_label.set_visible(!minimized);
                    sticky_jump_bottom.set_visible(!minimized && long_output);
                    sticky.set_visible(true);
                } else {
                    sticky_target.set(None);
                    sticky_jump_bottom.set_visible(false);
                    sticky_stop.set_visible(false);
                    sticky_organism.set_visible(false);
                    sticky.set_visible(false);
                }
                glib::ControlFlow::Continue
            })
        };

        // ── VTE is used as a display-only widget (fed via feed() in alt-screen mode)
        //    so we do NOT attach it to the PTY. Our reader thread handles all I/O.

        // ── Live VTE input → PTY (anvil model) ───────────────────────────
        // The active VTE has input_enabled(true), so it translates keystrokes and
        // owns IME natively; its `commit` signal carries the bytes to send. We
        // forward them to the PTY and, while awaiting a command, reconstruct the
        // typed command line so the finalize path can style it into the block.
        {
            let pty_for_commit = pty.clone();
            let bstate_for_commit = bstate.clone();
            let typed_cmd_for_commit = typed_cmd.clone();
            let armed_agent_execution_for_commit = armed_agent_execution.clone();
            let verified_submission_for_commit = verified_submission.clone();
            let idle_input_dirty_for_commit = idle_input_dirty.clone();
            let pty_synced_for_commit = pty_synced.clone();
            let finished_blocks_for_commit = finished_blocks_rc.clone();
            let selected_block_ids_for_commit = selected_block_ids.clone();
            let selected_block_id_for_commit = selected_block_id.clone();
            let selection_anchor_id_for_commit = selection_anchor_id.clone();
            let human_input_for_commit = human_input_callbacks.clone();
            active_vte.connect_commit(move |_, text, _size| {
                if armed_agent_execution_for_commit.borrow().is_some()
                    || verified_submission_for_commit.submission.borrow().is_some()
                {
                    log::warn!("Ignoring VTE commit while an Agent command submission is pending");
                    return;
                }
                // Real terminal input exits historical block selection. Without
                // this, Enter can recall an old selection after the user has
                // already begun editing a new command at the live prompt.
                if selected_block_id_for_commit.get().is_some() {
                    let finished = finished_blocks_for_commit.borrow();
                    clear_finished_block_selection(
                        &finished,
                        &selected_block_ids_for_commit,
                        &selected_block_id_for_commit,
                        &selection_anchor_id_for_commit,
                    );
                }

                pty_for_commit.write_bytes(text.as_bytes());
                emit_human_input(&human_input_for_commit, HumanInputKind::Keyboard);
                // The finished-block command text comes from a live-VTE
                // text_range read at CommandStart (see PromptEnd / CommandStart
                // handlers), so this shadow buffer is only used to size the
                // input cell while idle (line count). We do not need to track
                // escape sequences or replay deletes — newline count is what
                // drives `update_input_height`.
                if bstate_for_commit.get() == BlockState::AwaitingCommand {
                    idle_input_dirty_for_commit.set(true);
                    if text
                        .as_bytes()
                        .iter()
                        .any(|&byte| byte != b'\r' && byte != b'\n')
                    {
                        // A later history recall must replace this readline
                        // buffer, not append to it.
                        pty_synced_for_commit.set(true);
                    }
                    let mut cmd = typed_cmd_for_commit.borrow_mut();
                    for ch in text.chars() {
                        if ch == '\r' || ch == '\n' {
                            // Submitted — leave whatever is in the buffer; it
                            // is cleared at PromptEnd for the next prompt.
                        } else if ch == '\x7f' || ch == '\x08' {
                            pop_typed_command_shadow(&mut cmd);
                        } else if (ch as u32) < 0x20 {
                            // Control bytes: ignore.
                        } else {
                            let mut encoded = [0; 4];
                            append_typed_command_shadow(&mut cmd, ch.encode_utf8(&mut encoded));
                        }
                    }
                }
            });
        }

        // Keep explicit terminal interrupts available while a normal command is
        // running. Printable keys, editing keys, and Enter must fall through to
        // VTE: its GTK input-method context turns a composed CJK candidate into
        // one UTF-8 `commit` signal. Sending raw keyvals here bypasses that
        // context, so fcitx/ibus can show a candidate window but cannot commit it.
        {
            let pty_for_root_key = pty.clone();
            let bstate_for_root_key = bstate.clone();
            let hold_for_root_key = selection_feed_hold.clone();
            let human_input_for_root_key = human_input_callbacks.clone();
            let root_key = gtk::EventControllerKey::new();
            root_key.set_propagation_phase(gtk::PropagationPhase::Capture);
            root_key.connect_key_pressed(move |_controller, keyval, _keycode, modifiers| {
                use gtk::gdk::Key;
                if !matches!(
                    bstate_for_root_key.get(),
                    BlockState::CollectingOutput | BlockState::PostCommand
                ) {
                    return glib::Propagation::Proceed;
                }

                let ctrl = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
                let alt = modifiers.contains(gtk::gdk::ModifierType::ALT_MASK);
                if ctrl && !alt && matches!(keyval, Key::c | Key::C) {
                    hold_for_root_key.flush_then(|| pty_for_root_key.write_bytes(b"\x03"));
                    emit_human_input(&human_input_for_root_key, HumanInputKind::ProcessControl);
                    return glib::Propagation::Stop;
                }
                if ctrl && !alt && matches!(keyval, Key::d | Key::D) {
                    hold_for_root_key.flush_then(|| pty_for_root_key.write_bytes(b"\x04"));
                    emit_human_input(&human_input_for_root_key, HumanInputKind::ProcessControl);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            root.add_controller(root_key);
        }

        // Read-only snapshot VTEs and header buttons inside finished blocks
        // are click-focusable, so a click into history strands keyboard focus
        // where typing goes nowhere. A typing-shaped key press hands focus
        // back to the live prompt and re-pins the view to the bottom. The
        // triggering keystroke is consumed rather than forwarded: replaying it
        // into the PTY would bypass the live VTE's input-method context and
        // corrupt CJK composition. Bound chords never get here — the
        // window-level dispatcher captures first and swallows them.
        {
            let active_vte_for_refocus = active_vte.clone();
            let root_for_refocus = root.clone();
            let scroll_for_refocus = block_scroll.clone();
            let debouncer_for_refocus = scroll_debouncer.clone();
            let unread_for_refocus = unread_count.clone();
            let fab_for_refocus = jump_fab.clone();
            let refocus_key = gtk::EventControllerKey::new();
            refocus_key.set_propagation_phase(gtk::PropagationPhase::Capture);
            refocus_key.connect_key_pressed(move |_controller, keyval, _keycode, modifiers| {
                if active_vte_for_refocus.has_focus()
                    || !stranded_focus_key_recovers(keyval, modifiers)
                {
                    return glib::Propagation::Proceed;
                }
                let Some(focused) = root_for_refocus.root().and_then(|window| window.focus())
                else {
                    return glib::Propagation::Proceed;
                };
                if focused_widget_keeps_key(&focused, keyval) {
                    return glib::Propagation::Proceed;
                }
                active_vte_for_refocus.grab_focus();
                unread_for_refocus.set(0);
                set_jump_fab_label(&fab_for_refocus, 0);
                fab_for_refocus.set_visible(false);
                debouncer_for_refocus.reset_scroll_lock();
                // Focusing the live VTE makes the ScrolledWindow scroll to
                // reveal the holder's *top* (see the palette-dismissal note in
                // palette.rs) — pin the bottom right after, and keep pinning
                // across frames while virtualized blocks settle.
                debouncer_for_refocus.mark_dirty(&scroll_for_refocus);
                debouncer_for_refocus.pin_to_bottom_deferred(&scroll_for_refocus);
                glib::Propagation::Stop
            });
            root.add_controller(refocus_key);
        }

        // ── Keyboard navigation / copy-paste (Capture phase) ──────────────
        {
            let pty_for_key = pty.clone();
            let typed_cmd_for_key = typed_cmd.clone();
            let finished_blocks_for_key = finished_blocks_rc.clone();
            let selected_block_ids_for_key = selected_block_ids.clone();
            let selected_block_id_for_key = selected_block_id.clone();
            let selection_anchor_id_for_key = selection_anchor_id.clone();
            let block_scroll_for_key = block_scroll.clone();
            let key_ctrl = gtk::EventControllerKey::new();
            key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);

            KeyCtx {
                pty_for_key,
                pty_synced_for_key: pty_synced.clone(),
                bracketed_paste_for_key: bracketed_paste.clone(),
                typed_cmd_for_key,
                armed_agent_execution_for_key: armed_agent_execution.clone(),
                finished_blocks_for_key,
                selected_block_ids_for_key,
                selected_block_id_for_key,
                selection_anchor_id_for_key,
                block_scroll_for_key,
                bookmarks_for_key: block_bookmarks.clone(),
                bstate_for_key: bstate.clone(),
            }
            .connect(&key_ctrl);

            active_vte.add_controller(key_ctrl);
        }

        // Clicking the live prompt is also an explicit exit from historical
        // selection. Programmatic focus from a block header does not trigger
        // this gesture, so keyboard navigation remains intact.
        {
            let finished_for_click = finished_blocks_rc.clone();
            let selected_ids_for_click = selected_block_ids.clone();
            let selected_for_click = selected_block_id.clone();
            let anchor_for_click = selection_anchor_id.clone();
            let active_click = gtk::GestureClick::new();
            active_click.set_button(1);
            active_click.set_propagation_phase(gtk::PropagationPhase::Capture);
            active_click.connect_pressed(move |_, _, _, _| {
                if selected_for_click.get().is_some() {
                    let finished = finished_for_click.borrow();
                    clear_finished_block_selection(
                        &finished,
                        &selected_ids_for_click,
                        &selected_for_click,
                        &anchor_for_click,
                    );
                }
            });
            active_vte.add_controller(active_click);
        }

        // A plain click in the live prompt places the shell's edit cursor
        // there, the way an editor would, instead of making the user walk an
        // arrow key across a long command.
        crate::terminal::click_cursor::install(
            &active_vte,
            crate::terminal::click_cursor::ClickCursorCtx {
                enabled: config.click_moves_cursor,
                pty: Rc::clone(&pty),
                prompt_anchor: {
                    let surface = verified_submission.surface.clone();
                    let prompt_end_pos = prompt_end_pos.clone();
                    let prompt_anchor_rows = prompt_anchor_rows.clone();
                    Rc::new(move || {
                        surface.prompt_anchor(prompt_end_pos.get(), prompt_anchor_rows.get())
                    })
                },
                bstate: bstate.clone(),
                mouse_mode: mouse_reporting_mode.clone(),
                fullscreen: fullscreen.clone(),
                suggestion_rgb: crate::terminal::click_cursor::suggestion_rgb(&config.palette),
            },
        );

        // Wheel handling inside an alt-screen + mouse-reporting app (less / vim /
        // htop). VTE only synthesizes mouse-wheel CSI sequences when it owns the
        // PTY; ours is fed by our reader, so we synthesize and write the bytes
        // ourselves. The pointer cell under the cursor is tracked via a motion
        // controller so the column/row in the report matches what the user sees.
        //
        // - alt-screen + mouse mode + scroll_reporting_enabled → encode wheel,
        //   write to PTY, stop propagation (so block_scroll doesn't also scroll).
        // - alt-screen + mouse mode + !scroll_reporting_enabled → swallow wheel
        //   (user has opted out of mouse-driven paging).
        // - otherwise → let the event bubble to block_scroll for normal scroll.
        {
            // Track pointer position over the live VTE in cell coordinates so
            // wheel events emitted below can include accurate col/row.
            let pointer_cell: Rc<Cell<(i64, i64)>> = Rc::new(Cell::new((1, 1)));
            {
                let pointer_for_motion = pointer_cell.clone();
                let vte_for_motion = active_vte.clone();
                let motion = gtk::EventControllerMotion::new();
                motion.set_propagation_phase(gtk::PropagationPhase::Capture);
                motion.connect_motion(move |_, x, y| {
                    let cw = (vte_for_motion.char_width() as f64).max(1.0);
                    let ch = (vte_for_motion.char_height() as f64).max(1.0);
                    let col = (x / cw).floor() as i64 + 1;
                    let row = (y / ch).floor() as i64 + 1;
                    pointer_for_motion.set((col.max(1), row.max(1)));
                });
                active_vte.add_controller(motion);
            }

            let fullscreen_for_scroll = fullscreen.clone();
            let mouse_mode_for_scroll = mouse_reporting_mode.clone();
            let scroll_enabled = config.scroll_reporting_enabled;
            let pty_for_scroll = pty.clone();
            let pointer_for_scroll = pointer_cell.clone();
            let bstate_for_scroll = bstate.clone();
            let vte_for_scroll = active_vte.downgrade();
            let outer_for_scroll = block_scroll.downgrade();
            let unified_for_scroll = unified;
            let scroll_ctrl = gtk::EventControllerScroll::new(
                gtk::EventControllerScrollFlags::VERTICAL
                    | gtk::EventControllerScrollFlags::HORIZONTAL,
            );
            scroll_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
            scroll_ctrl.connect_scroll(move |_, _dx, dy| {
                let in_mouse_app = (fullscreen_for_scroll.get()
                    || bstate_for_scroll.get() == BlockState::AltScreen)
                    && mouse_mode_for_scroll.get() != MouseReportingMode::None;
                if in_mouse_app {
                    if !scroll_enabled {
                        return glib::Propagation::Stop;
                    }
                    let (col, row) = pointer_for_scroll.get();
                    if let Some(bytes) =
                        encode_mouse_wheel(mouse_mode_for_scroll.get(), dy, col, row)
                    {
                        pty_for_scroll.write_bytes(&bytes);
                    }
                    return glib::Propagation::Stop;
                }
                // Unified's VTE owns all scrollback, including at the idle
                // prompt. Let its native wheel handler move that adjustment.
                if unified_for_scroll {
                    return glib::Propagation::Proceed;
                }
                // Alt-screen programs without mouse reporting use VTE's native
                // wheel-to-arrow fallback.
                if bstate_for_scroll.get() == BlockState::AltScreen {
                    return glib::Propagation::Proceed;
                }
                // The still-running VTE is a first-class scroll surface. Hand
                // wheel motion to history only once its own buffer reaches an
                // edge, and never let idle VTE fallback swallow the event.
                if matches!(
                    bstate_for_scroll.get(),
                    BlockState::CollectingOutput
                        | BlockState::PostCommand
                        | BlockState::RawFallback
                ) {
                    if let Some(adjustment) = vte_for_scroll
                        .upgrade()
                        .and_then(|terminal| terminal.vadjustment())
                    {
                        if scroll_adjustment_by_wheel(&adjustment, dy) {
                            return glib::Propagation::Stop;
                        }
                    }
                }
                let Some(outer) = outer_for_scroll.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                forward_outer_scroll(&outer, dy);
                glib::Propagation::Stop
            });
            active_vte.add_controller(scroll_ctrl);

            // The overlay scrollbar follows the same live-buffer → history
            // handoff instead of becoming a dead wheel target at its edges.
            let live_scrollbar = active.borrow().live_scrollbar.clone();
            let scrollbar_scroll =
                gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
            scrollbar_scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
            let vte_for_scrollbar = active_vte.downgrade();
            let outer_for_scrollbar = block_scroll.downgrade();
            scrollbar_scroll.connect_scroll(move |_, _dx, dy| {
                if let Some(adjustment) = vte_for_scrollbar
                    .upgrade()
                    .and_then(|terminal| terminal.vadjustment())
                {
                    if scroll_adjustment_by_wheel(&adjustment, dy) {
                        return glib::Propagation::Stop;
                    }
                }
                let Some(outer) = outer_for_scrollbar.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                forward_outer_scroll(&outer, dy);
                glib::Propagation::Stop
            });
            live_scrollbar.add_controller(scrollbar_scroll);
        }

        let cross_selection = CrossSelection::install(
            &block_scroll,
            finished_blocks_rc.clone(),
            active_vte.clone(),
            selected_block_ids.clone(),
            selected_block_id.clone(),
            selection_anchor_id.clone(),
            selection_feed_hold.clone(),
            bstate.clone(),
            mouse_reporting_mode.clone(),
        );

        let term_view = TermView {
            root,
            block_scroll,
            block_list,
            notice_dock,
            active_vte,
            active,
            bstate,
            typed_cmd,
            prompt_end_pos,
            prompt_anchor_rows,
            prompt_anchor_ready,
            armed_agent_execution,
            agent_execution_supported,
            verified_submission,
            active_agent_execution,
            idle_input_dirty: idle_input_dirty.clone(),
            fullscreen,
            user_scrolled_up: user_scrolled_up.clone(),
            programmatic_scroll: programmatic_scroll.clone(),
            focus_requested,
            pty,
            pty_synced: pty_synced.clone(),
            cwd_callbacks,
            remote_session_callbacks,
            exited_callbacks,
            bell_callbacks,
            title_callbacks,
            activity_callbacks,
            human_input_callbacks,
            alt_screen_callbacks,
            command_started_callbacks,
            command_finished_callbacks,
            block_finished_callbacks,
            ask_ai_about_block_callbacks,
            mouse_reporting_mode,
            bracketed_paste,
            dynamic_colors,
            config: config_shared,
            block_data: block_data_rc,
            reserved_history_block_ids,
            failure_marker_redraw,
            finished_blocks: finished_blocks_rc,
            viewport: Rc::new(RefCell::new(ViewportState {
                first_visible: 0,
                last_visible: 0,
            })),
            widget_pool,
            visible_indices,
            selected_block_ids,
            selected_block_id,
            navigated_record_id: Cell::new(None),
            selection_anchor_id,
            bookmarks: block_bookmarks,
            cleared_stash: RefCell::new(Vec::new()),
            history_baselines: RefCell::new(HashMap::new()),
            history_explicit_replace_pending: RefCell::new(VecDeque::new()),
            unread_count,
            jump_fab,
            sticky_organism_slot,
            find_state,
            current_cwd: current_cwd.clone(),
            resize_tick_id: RefCell::new(None),
            sticky_timer_id: RefCell::new(Some(sticky_timer_id)),
            cross_selection,
            selection_feed_hold,
            layout_active_surface,
            render_backend: render_backend_slot
                .borrow_mut()
                .take()
                .expect("reader backend installed before TermView construction"),
        };

        // Load history if configured
        let _ = term_view.load_history();

        // Create widgets for loaded blocks. Each block's `cols` is what the live
        // VTE was wrapping at when the command ran; restoring at the same cols
        // reproduces the exact line breaks (so `ls` columns don't get split
        // mid-word). For old saves without a cols field (cols == 0), fall back
        // to the live VTE's current column count.
        {
            let config = term_view.config.borrow();
            let fallback_cols =
                bounded_finished_vte_columns(term_view.active.borrow().grid_cols() as i64);
            mutate_block_data_and_redraw(
                &term_view.block_data,
                term_view.failure_marker_redraw.as_ref(),
                |block_data_ref| {
                    for block in block_data_ref.iter_mut() {
                        let cols = bounded_finished_vte_columns(if block.cols > 0 {
                            block.cols as i64
                        } else {
                            fallback_cols
                        });
                        // Older history entries stored an estimate based only on `\n`
                        // count. Recompute it so a restored long wrapped line is not
                        // virtualized away while it is still visible.
                        block.estimated_height =
                            estimated_finished_block_height_for_text(&config, &block.output, cols);
                        let finished = FinishedBlock::new(
                            block.id,
                            &block.prompt,
                            &block.cmd,
                            block.cmd_markup.as_deref(),
                            &block.output,
                            block.exit_code,
                            &config,
                            block.duration_ms,
                            block.end_time_ms,
                            block.cwd.as_deref(),
                            cols,
                        );
                        finished.widget().insert_before(
                            &term_view.block_list,
                            Some(term_view.active.borrow().widget()),
                        );
                        finished.connect_actions(
                            &term_view.active_vte,
                            &term_view.pty,
                            &pty_synced,
                            &term_view.bracketed_paste,
                            &term_view.typed_cmd,
                            &term_view.armed_agent_execution,
                            &term_view.bstate,
                            &term_view.active,
                        );
                        finished.connect_scroll_forwarding(&term_view.block_scroll);
                        install_finished_block_selection(
                            &finished,
                            &term_view.active,
                            &term_view.finished_blocks,
                            &term_view.selected_block_ids,
                            &term_view.selected_block_id,
                            &term_view.selection_anchor_id,
                        );
                        term_view.finished_blocks.borrow_mut().push(finished);
                    }
                },
            );
        }

        // Initialize viewport and visibility
        term_view.update_viewport();
        term_view.update_block_visibility();

        // Wire virtual scrolling: connect scroll signals
        {
            let viewport = term_view.viewport.clone();
            let block_scroll = term_view.block_scroll.downgrade();
            let block_data = term_view.block_data.clone();
            let config = term_view.config.clone();
            let finished_blocks = Rc::downgrade(&term_view.finished_blocks);
            let visible_indices = term_view.visible_indices.clone();
            let fullscreen = term_view.fullscreen.clone();
            let failure_marker_redraw = term_view.failure_marker_redraw.clone();
            let visibility_update_pending = Rc::new(Cell::new(false));
            let last_page_size = Rc::new(Cell::new(None::<f64>));

            let schedule_visibility_update: Rc<dyn Fn()> = Rc::new(move || {
                let Some(scroll) = block_scroll.upgrade() else {
                    return;
                };
                if fullscreen.get()
                    || !scroll.is_mapped()
                    || visibility_update_pending.replace(true)
                {
                    return;
                }
                let Some(finished) = finished_blocks.upgrade() else {
                    visibility_update_pending.set(false);
                    return;
                };

                let viewport = viewport.clone();
                let block_data = block_data.clone();
                let config = config.clone();
                let visible = visible_indices.clone();
                let fullscreen = fullscreen.clone();
                let pending = visibility_update_pending.clone();
                let failure_marker_redraw = failure_marker_redraw.clone();
                glib::idle_add_local_once(move || {
                    pending.set(false);
                    if fullscreen.get() || !scroll.is_mapped() {
                        return;
                    }
                    let adjustment = scroll.vadjustment();
                    let margin = config.borrow().virtual_scroll_margin;
                    let block_data_ref = block_data.borrow();
                    let Some((strict, loose)) = viewport_states_for_scroll(
                        &block_data_ref,
                        adjustment.value(),
                        adjustment.page_size(),
                        margin,
                    ) else {
                        return;
                    };
                    drop(block_data_ref);

                    let new_visible =
                        stable_visible_indices(&strict, Some(&loose), &visible.borrow());
                    *viewport.borrow_mut() = strict;

                    let finished_ref = finished.borrow();
                    let mut block_data_ref = block_data.borrow_mut();
                    let mut visible_ref = visible.borrow_mut();
                    apply_visible_indices(
                        &finished_ref,
                        &mut block_data_ref,
                        &mut visible_ref,
                        new_visible,
                    );
                    failure_marker_redraw();
                });
            });

            let adjustment = term_view.block_scroll.vadjustment();
            {
                let schedule = schedule_visibility_update.clone();
                let last_page_size = last_page_size.clone();
                adjustment.connect_changed(move |adjustment| {
                    if viewport_page_size_changed(&last_page_size, adjustment.page_size()) {
                        schedule();
                    }
                });
            }
            {
                let schedule = schedule_visibility_update.clone();
                adjustment.connect_value_changed(move |_| schedule());
            }
            term_view
                .block_scroll
                .connect_map(move |_| schedule_visibility_update());
        }

        // ── Resize handler: sync PTY cols/rows when widget allocation changes ──
        term_view.install_resize_tick();

        Ok(term_view)
    }

    /// Keep PTY geometry synchronized with the real pane viewport, independent
    /// of the compact/full visual state of the live VTE.
    fn install_resize_tick(&self) {
        let pty_for_resize = self.pty.clone();
        let scroll_for_resize = self.block_scroll.clone();
        let layout_for_resize = self.layout_active_surface.clone();
        let clip_for_resize = self.active.borrow().live_clip().downgrade();
        let last: Rc<Cell<(u16, u16)>> = Rc::new(Cell::new((0, 0)));
        let last_pane: Rc<Cell<(i32, i32)>> = Rc::new(Cell::new((0, 0)));
        let tick_id = self.active_vte.add_tick_callback(move |vte, _clock| {
            // The live terminal is sized by an explicit pixel request (it lives
            // in a `gtk::Fixed` and cannot expand into the pane on its own), and
            // a resized pane at an idle prompt produces no `contents-changed`
            // to re-run the layout. Watch the pane from the frame clock so the
            // grid — and therefore the winsize read below — follows the window.
            if let Some(clip) = clip_for_resize.upgrade() {
                let pane = (
                    clip.width(),
                    scroll_for_resize.vadjustment().page_size() as i32,
                );
                if pane != last_pane.get() && pane.0 > 0 {
                    last_pane.set(pane);
                    layout_for_resize();
                }
            }
            let (cols, rows) = pty_grid_size(vte, &scroll_for_resize);
            if cols > 0 && rows > 0 && (cols, rows) != last.get() {
                last.set((cols, rows));
                pty_for_resize.resize(cols, rows);
            }
            glib::ControlFlow::Continue
        });
        *self.resize_tick_id.borrow_mut() = Some(tick_id);
    }

    /// Root GTK widget to embed in the notebook page.
    pub fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    /// Attach a pass-through body to the live VTE overlay. Repeating this for
    /// the same body acts as a move; widgets owned elsewhere are rejected.
    pub(crate) fn put_live_organism_body(&self, body: &gtk::Widget, x: f64, y: f64) -> bool {
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        let surface = self.active.borrow().live_organism_surface.clone();
        let surface_widget: gtk::Widget = surface.clone().upcast();
        match body.parent() {
            None => surface.put(body, x, y),
            Some(parent) if parent == surface_widget => surface.move_(body, x, y),
            Some(_) => return false,
        }
        true
    }

    pub(crate) fn move_live_organism_body(&self, body: &gtk::Widget, x: f64, y: f64) -> bool {
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        let surface = self.active.borrow().live_organism_surface.clone();
        let surface_widget: gtk::Widget = surface.clone().upcast();
        if body.parent().as_ref() != Some(&surface_widget) {
            return false;
        }
        surface.move_(body, x, y);
        true
    }

    pub(crate) fn set_live_organism_visible(&self, visible: bool) {
        self.active.borrow().set_live_organism_visible(visible);
    }

    pub(crate) fn live_organism_surface_metrics(&self) -> LiveOrganismSurfaceMetrics {
        let active = self.active.borrow();
        let visible_height = active
            .live_visible_height_px()
            .min(active.active_vte.height().max(0));
        LiveOrganismSurfaceMetrics {
            // The hidden surface may not be allocated yet. The always-mapped
            // VTE is its measured child and shares the same clipped space.
            width: active.active_vte.width().max(0),
            // The VTE keeps the whole viewport grid while a command runs; only
            // the card's own height is on screen. Placing the body by the grid
            // would drop it into the clipped rows.
            height: visible_height,
            cell_width: (active.active_vte.char_width() as i32).max(1),
            cell_height: (active.active_vte.char_height() as i32).max(1),
            right_gutter: LIVE_ORGANISM_RIGHT_GUTTER,
            alt_screen: active.live_organism_alt_screen(),
            cursor_row: {
                let (_, cursor_row) = active.active_vte.cursor_position();
                let top_row = gtk::prelude::ScrollableExt::vadjustment(&active.active_vte)
                    .map(|adjustment| adjustment.value().floor() as i64)
                    .unwrap_or(0);
                let cell_height = active.active_vte.char_height().max(1);
                let visible_rows = ((visible_height as i64) / cell_height)
                    .clamp(1, active.active_vte.row_count().max(1));
                cursor_row
                    .saturating_sub(top_row)
                    .clamp(0, visible_rows.saturating_sub(1)) as i32
            },
        }
    }

    pub(crate) fn put_sticky_organism_avatar(&self, avatar: &gtk::Widget) -> bool {
        let slot_widget: gtk::Widget = self.sticky_organism_slot.clone().upcast();
        match avatar.parent() {
            None => self.sticky_organism_slot.append(avatar),
            Some(parent) if parent == slot_widget => {}
            Some(_) => return false,
        }
        true
    }

    /// Whether this pane uses the continuous Unified backend rather than the
    /// finished-block renderer.
    pub(crate) fn is_unified(&self) -> bool {
        matches!(self.render_backend.records(), BackendRecords::Metadata(_))
    }

    /// Whether a card can be mounted anywhere the user can reach. A backend
    /// whose document cannot scroll to one answers through the bottom dock
    /// instead of refusing.
    pub(crate) fn supports_inline_notices(&self) -> bool {
        self.render_backend.supports_inline_notices()
    }

    /// Insert a transient card directly above the live prompt. Agent UI is
    /// deliberately not a finished block, so it stays out of history,
    /// selection, virtualization, and persistence metadata.
    ///
    /// Calling this for an already-inserted widget re-pins it below any newly
    /// completed command block.
    pub fn insert_inline_notice(&self, widget: &gtk::Widget) -> bool {
        if self.render_backend.docks_inline_notices() {
            return self.dock_inline_notice(widget);
        }
        if !self.supports_inline_notices() {
            log::debug!("refusing an inline notice card no document can reach");
            return false;
        }
        let active_widget = self.active.borrow().widget().clone();
        let already_inserted = widget
            .parent()
            .is_some_and(|parent| parent == *self.block_list.upcast_ref::<gtk::Widget>());
        if already_inserted {
            let anchor = active_widget.prev_sibling();
            if anchor.as_ref() != Some(widget) {
                self.block_list.reorder_child_after(widget, anchor.as_ref());
            }
        } else {
            widget.insert_before(&self.block_list, Some(&active_widget));
        }
        self.block_list.queue_allocate();
        ScrollDebouncer::with_scroll_lock(
            self.user_scrolled_up.clone(),
            self.programmatic_scroll.clone(),
        )
        .pin_to_bottom_deferred(&self.block_scroll);
        true
    }

    /// Mount a card in the bottom dock, newest last, and give the surface its
    /// new height. Re-mounting a card already docked leaves it where it is:
    /// re-pinning exists to keep a card next to the prompt in a scrolling
    /// document, and the dock is always next to the prompt.
    fn dock_inline_notice(&self, widget: &gtk::Widget) -> bool {
        let dock_widget: &gtk::Widget = self.notice_dock.upcast_ref();
        match dock_mount_decision(widget.parent().as_ref(), dock_widget) {
            DockMount::Refuse => false,
            DockMount::Keep => {
                self.notice_dock.set_visible(true);
                self.relayout_after_dock_change();
                true
            }
            DockMount::Append => {
                self.notice_dock.append(widget);
                self.notice_dock.set_visible(true);
                self.relayout_after_dock_change();
                true
            }
        }
    }

    /// Give the surface back the rows the dock no longer needs. Hiding the
    /// empty dock is what returns them: an empty visible box still asks for
    /// padding, and the child would keep the smaller grid.
    fn relayout_after_dock_change(&self) {
        if self.notice_dock.first_child().is_none() {
            self.notice_dock.set_visible(false);
        }
        self.notice_dock.queue_allocate();
        // The grid follows the viewport the dock just resized. One pass here
        // is one SIGWINCH for the child, not one per card.
        (self.layout_active_surface)();
        self.render_backend.sync_geometry_to_pty();
    }

    /// Remove a transient inline card. Safe when the widget was already
    /// detached as part of pane teardown.
    pub fn remove_inline_notice(&self, widget: &gtk::Widget) {
        if widget
            .parent()
            .is_some_and(|parent| parent == *self.notice_dock.upcast_ref::<gtk::Widget>())
        {
            self.notice_dock.remove(widget);
            self.relayout_after_dock_change();
            return;
        }
        if widget
            .parent()
            .is_some_and(|parent| parent == *self.block_list.upcast_ref::<gtk::Widget>())
        {
            self.block_list.remove(widget);
            self.block_list.queue_allocate();
        }
    }

    /// Send key bytes into the PTY (user input).
    pub fn write_input(&self, data: &[u8]) {
        // Once an Agent command has been accepted into the ordered writer,
        // refuse later prompt input until its CommandStart consumes the arm.
        // Clearing the identity while still queueing bytes would allow a user
        // action to merge into the reviewed command before the shell sees CR.
        if self.armed_agent_execution.borrow().is_some()
            || self.verified_submission.submission.borrow().is_some()
        {
            log::warn!("Ignoring terminal input while a reviewed command submission is pending");
            return;
        }
        self.selection_feed_hold.flush_now();
        self.write_input_bytes(data);
    }

    fn write_input_bytes(&self, data: &[u8]) {
        if self.bstate.get() == BlockState::AwaitingCommand
            && data.iter().any(|byte| !matches!(byte, b'\r' | b'\n'))
        {
            append_typed_command_shadow(
                &mut self.typed_cmd.borrow_mut(),
                &String::from_utf8_lossy(data),
            );
        }
        self.pty.write_bytes(data);
    }

    /// Resolve the saved PromptEnd anchor through this pane's surface policy.
    /// Reviewed submission and click-to-place-cursor use the same method on the
    /// same surface object.
    fn current_prompt_anchor(&self) -> (i64, i64) {
        self.verified_submission
            .surface
            .prompt_anchor(self.prompt_end_pos.get(), self.prompt_anchor_rows.get())
    }

    /// Review-gated commands may only target a clean, idle shell editor. The
    /// diagnostic status is shared by the inline Agent card and execution
    /// boundary so the UI never advertises a weaker condition than the write.
    pub(crate) fn command_prompt_status(&self) -> CommandPromptStatus {
        let status = classify_command_prompt_status(
            self.bstate.get(),
            self.fullscreen.get(),
            self.idle_input_dirty.get(),
            self.pty_synced.get(),
            self.typed_cmd.borrow().trim().is_empty(),
        );
        if status != CommandPromptStatus::Ready {
            return status;
        }
        if !self.prompt_anchor_ready.get() {
            return CommandPromptStatus::Initializing;
        }
        match self.pty.shell_is_foreground() {
            Some(false) => CommandPromptStatus::Running,
            None => CommandPromptStatus::ShellIntegrationUnavailable,
            Some(true) => {
                if self.verified_submission.surface.cursor_position()
                    == self.current_prompt_anchor()
                    && self.verified_submission.surface.suffix_is_empty() == Some(true)
                {
                    CommandPromptStatus::Ready
                } else {
                    CommandPromptStatus::HasInput
                }
            }
        }
    }

    pub fn can_accept_agent_command(&self) -> bool {
        self.agent_execution_supported.get()
            && self.command_prompt_status().is_ready()
            && self.armed_agent_execution.borrow().is_none()
            && self.verified_submission.submission.borrow().is_none()
    }

    pub(crate) fn agent_command_prompt_status(&self) -> CommandPromptStatus {
        if self.agent_execution_supported.get() {
            self.command_prompt_status()
        } else {
            CommandPromptStatus::ShellIntegrationUnavailable
        }
    }

    /// Put an Agent proposal at the prompt for ordinary manual review without
    /// submitting it or arming an Agent observation. This is synchronous on
    /// the GTK thread so the readiness check and write cannot be interleaved
    /// with another application action.
    pub fn try_insert_agent_command(&self, command: &str) -> bool {
        if !agent_command_is_safe(command)
            || !self.command_prompt_status().is_ready()
            || self.verified_submission.submission.borrow().is_some()
        {
            return false;
        }
        self.write_input(command.as_bytes());
        true
    }

    /// Submit one explicitly approved, locally verified review command without
    /// arming Agent observation. Readiness, validation, command bytes, and the
    /// terminating carriage return are admitted as one UI-thread operation so
    /// another input path cannot alter the reviewed text in between.
    pub fn try_run_review_command(&self, command: &str) -> bool {
        if !agent_command_is_safe(command) {
            return false;
        }
        match self.verified_submission.begin(command, None) {
            Ok(()) => true,
            Err(error) => {
                log::warn!("Could not begin verified review command: {error}");
                false
            }
        }
    }

    /// Re-check prompt readiness, arm a one-shot local identity, and submit
    /// the reviewed command without yielding to another input path between
    /// those operations.
    pub fn try_run_agent_command(
        &self,
        execution: crate::agent::AgentExecutionRef,
        command: &str,
    ) -> bool {
        if execution.generation == 0
            || !agent_command_is_safe(command)
            || !self.can_accept_agent_command()
        {
            return false;
        }
        match self.verified_submission.begin(command, Some(execution)) {
            Ok(()) => true,
            Err(error) => {
                log::warn!("Could not begin verified Agent command: {error}");
                false
            }
        }
    }

    /// Resize the PTY.
    pub fn resize(&self, cols: u16, rows: u16) {
        self.pty.resize(cols, rows);
    }

    /// Kill the child process.
    pub fn kill(&self) {
        self.pty.kill();
    }

    pub fn pid_i32(&self) -> i32 {
        self.pty.pid_i32()
    }

    pub fn pty_fd_i32(&self) -> i32 {
        self.pty.master_fd_raw()
    }

    pub fn vte(&self) -> &Terminal {
        &self.active_vte
    }

    pub fn cwd(&self) -> String {
        self.current_cwd.borrow().clone()
    }

    pub fn grab_focus(&self) {
        self.focus_requested.set(true);
        self.active_vte.grab_focus();
        if self.active_vte.has_focus() {
            self.focus_requested.set(false);
        }
        let active_vte = self.active_vte.clone();
        let focus_requested = self.focus_requested.clone();
        glib::idle_add_local_once(move || {
            if focus_requested.get() {
                active_vte.grab_focus();
                if active_vte.has_focus() {
                    focus_requested.set(false);
                }
            }
        });
    }

    /// Copy selected text to clipboard.
    /// Priority: (1) live VTE selection (alt-screen apps + idle input cell),
    /// (2) any finished-block TextBuffer with an active selection, (3) PRIMARY
    /// clipboard as a last-resort fallback. Step 2 is what makes Ctrl+Shift+C
    /// work for mouse-selected text inside finished command/output views —
    /// PRIMARY alone is unreliable across compositors (notably Wayland).
    pub fn copy_to_clipboard(&self) {
        self.copy_to_clipboard_with_modifier(false);
    }

    /// Same as `copy_to_clipboard` but also honors the Warp "copy block output
    /// only" modifier (Alt+Ctrl+Shift+C) when a whole block is selected.
    pub fn copy_to_clipboard_with_modifier(&self, alt_held: bool) {
        log::debug!(">>> TermView::copy_to_clipboard called (alt={})", alt_held);

        // (0) Whole-block selection (Warp's CopyBlock; +Alt → output only).
        // Multi-selection copies blocks in terminal order with one blank line between
        // them, so the clipboard preserves the same visual grouping as the canvas.
        {
            let selected = self.selected_block_ids.borrow();
            if !selected.is_empty() {
                let data = self.block_data.borrow();
                let parts: Vec<String> = data
                    .iter()
                    .filter(|block| selected.contains(&block.id))
                    .map(|block| block_clipboard_text(&block.cmd, &block.output, alt_held))
                    .collect();
                if !parts.is_empty() {
                    let text = parts.join("\n\n");
                    log::debug!(
                        ">>> TermView copy: copied {} selected blocks ({} chars)",
                        parts.len(),
                        text.len()
                    );
                    self.active_vte.clipboard().set_text(&text);
                    self.selection_feed_hold.flush_now();
                    return;
                }
            }
        }

        // (0.5) Cross-block drag: if more than one VTE has a selection (the
        // user dragged across block boundaries, see cross_selection.rs), copy
        // the concatenated text in widget order instead of just one widget's.
        if self.cross_selection.has_cross_selection() {
            match self.cross_selection.copy_text() {
                Some(text) => {
                    log::debug!(
                        ">>> TermView copy: got {} chars from cross-block selection",
                        text.len()
                    );
                    self.active_vte.clipboard().set_text(&text);
                }
                None => {
                    // Aggregation is deliberately atomic. Falling through to a
                    // single VTE here would silently copy only part of an
                    // oversized cross-surface selection.
                    log::warn!(
                        "Cross-block selection exceeds the clipboard safety limit; copied nothing"
                    );
                }
            }
            self.selection_feed_hold.flush_now();
            return;
        }

        // (1) Live VTE selection
        if let Some(text) = self.active_vte.text_selected(vte4::Format::Text) {
            if !text.is_empty() {
                log::debug!(">>> TermView copy: got {} chars from VTE", text.len());
                self.active_vte.clipboard().set_text(&text);
                self.selection_feed_hold.flush_now();
                return;
            }
        }

        // (2) Finished-block VTEs (output_vte / command_vte). GTK4 selection is
        // per-widget so only one block can have a live selection at a time —
        // that's the one we copy.
        for blk in self.finished_blocks.borrow().iter() {
            for vte in [&blk.output_vte, &blk.command_vte] {
                if let Some(text) = vte.text_selected(vte4::Format::Text) {
                    let s = text.to_string();
                    if !s.is_empty() {
                        log::debug!(
                            ">>> TermView copy: got {} chars from finished block VTE",
                            s.len()
                        );
                        self.active_vte.clipboard().set_text(&s);
                        self.selection_feed_hold.flush_now();
                        return;
                    }
                }
            }
        }

        // No live VTE / finished-block selection. We deliberately do NOT
        // fall back to PRIMARY — on Wayland it is empty for our own widgets
        // anyway, and on X11 GTK already mirrors widget selections into both
        // clipboards so the path was never actually load-bearing. Bailing out
        // here keeps Ctrl+Shift+C deterministic: it copies what the user can
        // see is selected, and only that.
        log::debug!(">>> TermView copy: no selection found, nothing to copy");
    }

    /// Paste clipboard text as one ordered write to block mode's shell PTY.
    ///
    /// The active VTE is display-only in this mode and has no child PTY, so
    /// `Terminal::paste_clipboard()` can lose or reorder multiline input. Read
    /// the clipboard ourselves and preserve the shell's bracketed-paste mode.
    ///
    /// This used to be three raw writes — `ESC[200~`, the clipboard verbatim,
    /// `ESC[201~` — and the middle one arrived while the PTY boundary already
    /// considered a frame open, so it was waved through untouched. A clipboard
    /// containing `ESC[201~` closed the frame early and the bytes after it
    /// reached the shell as keystrokes, i.e. ran. [`pty_input::encode_paste`]
    /// removes paste markers from the body unconditionally and returns the whole
    /// payload as one buffer, so there is no longer a body write to attack.
    pub fn paste_from_clipboard(&self) {
        self.selection_feed_hold.flush_now();
        let clipboard = self.active_vte.clipboard();
        let pty = self.pty.clone();
        let bracketed_paste = self.bracketed_paste.clone();
        let bstate = self.bstate.clone();
        let typed_cmd = self.typed_cmd.clone();
        let armed_agent_execution = self.armed_agent_execution.clone();
        let reviewed_submission = self.verified_submission.submission.clone();
        let pty_synced = self.pty_synced.clone();
        let idle_input_dirty = self.idle_input_dirty.clone();
        let human_input = self.human_input_callbacks.clone();
        clipboard.read_text_async(None::<&gtk::gio::Cancellable>, move |result| {
            let Ok(Some(text)) = result else {
                return;
            };
            if armed_agent_execution.borrow().is_some()
                || reviewed_submission.borrow().is_some()
            {
                log::warn!("Ignoring paste while a reviewed command submission is pending");
                return;
            }
            let paste = build_clipboard_paste(text.as_str(), bracketed_paste.get());
            if paste.is_empty() {
                return;
            }
            if paste.risk.had_embedded_paste_marker {
                log::warn!(
                    "Removed a bracketed-paste marker from pasted text before writing it to the shell"
                );
            }
            pty.write_bytes(&paste.bytes);
            emit_human_input(&human_input, HumanInputKind::Clipboard);
            // Mirror what the child actually received into the editor shadow, or
            // the live input cell keeps the height of a command the shell no
            // longer has and the next history recall appends to a line it thinks
            // is empty.
            record_external_input(
                bstate.get(),
                &paste.echo_text,
                &typed_cmd,
                &pty_synced,
                &idle_input_dirty,
            );
        });
    }

    pub fn connect_cwd_changed<F: Fn(&str, bool) + 'static>(&self, f: F) {
        self.cwd_callbacks.borrow_mut().push(Box::new(f));
    }

    pub fn connect_remote_session_id<F: Fn(&str) + 'static>(&self, f: F) {
        self.remote_session_callbacks.borrow_mut().push(Box::new(f));
    }

    pub fn connect_exited<F: Fn(i32) + 'static>(&self, f: F) {
        self.exited_callbacks.borrow_mut().push(Box::new(f));
    }

    pub fn connect_bell<F: Fn() + 'static>(&self, f: F) {
        self.bell_callbacks.borrow_mut().push(Box::new(f));
    }

    pub fn connect_title_changed<F: Fn(&str) + 'static>(&self, f: F) {
        self.title_callbacks.borrow_mut().push(Box::new(f));
    }

    pub fn connect_activity<F: Fn() + 'static>(&self, f: F) {
        self.activity_callbacks.borrow_mut().push(Box::new(f));
    }

    /// Observe accepted direct-human PTY input without exposing its contents.
    pub(crate) fn connect_human_input<F: Fn(HumanInputKind) + 'static>(&self, f: F) {
        self.human_input_callbacks.borrow_mut().push(Box::new(f));
    }

    /// Observe alternate-screen ownership without exposing terminal bytes.
    pub(crate) fn connect_alt_screen_transition<F>(&self, f: F)
    where
        F: Fn(AltScreenTransition) + 'static,
    {
        self.alt_screen_callbacks.borrow_mut().push(Box::new(f));
    }

    pub(crate) fn connect_command_started<F>(&self, f: F)
    where
        F: Fn(CommandStartedEvent) + 'static,
    {
        self.command_started_callbacks
            .borrow_mut()
            .push(Box::new(f));
    }

    pub(crate) fn connect_command_finished<F>(&self, f: F)
    where
        F: Fn(CommandFinishedEvent) + 'static,
    {
        self.command_finished_callbacks
            .borrow_mut()
            .push(Box::new(f));
    }

    /// Whether an identity-verified Agent command currently owns this block.
    /// Only the fact crosses into the organism; no proposal or command text.
    pub(crate) fn agent_command_active(&self) -> bool {
        self.active_agent_execution.get().is_some()
    }

    pub(crate) fn connect_agent_execution_lost<F>(&self, f: F)
    where
        F: Fn(crate::agent::AgentExecutionRef, &'static str) + 'static,
    {
        self.verified_submission
            .agent_execution_lost_callbacks
            .borrow_mut()
            .push(Box::new(f));
    }

    pub fn connect_block_finished<F>(&self, f: F)
    where
        F: Fn(String, Option<i32>, Option<crate::agent::AgentExecutionRef>, Option<u64>) + 'static,
    {
        self.block_finished_callbacks
            .borrow_mut()
            .push(BlockFinishedCallback::Metadata(Box::new(f)));
    }

    /// Register Anvil's Relm4 bridge with a per-completion output capability.
    /// The callback always receives metadata; `None` means its predicate chose
    /// not to materialize the terminal payload for this event.
    pub(crate) fn connect_block_finished_with_output_if<P, F>(&self, needs_output: P, f: F)
    where
        P: Fn(Option<crate::agent::AgentExecutionRef>) -> bool + 'static,
        F: Fn(
                String,
                Option<i32>,
                Option<String>,
                Option<crate::agent::AgentExecutionRef>,
                Option<u64>,
            ) + 'static,
    {
        self.block_finished_callbacks
            .borrow_mut()
            .push(BlockFinishedCallback::ConditionalOutput {
                needs_output: Box::new(needs_output),
                callback: Box::new(f),
            });
    }

    pub fn connect_ask_ai_about_block<F>(&self, f: F)
    where
        F: Fn(crate::ai::BlockContext) + 'static,
    {
        self.ask_ai_about_block_callbacks
            .borrow_mut()
            .push(Box::new(f));
    }

    pub fn scroll_lines(&self, lines: i32) {
        if self.render_backend.scroll_surface_lines(lines) {
            return;
        }
        // Ctrl+Up enters Warp-style block selection at the newest block; once a
        // block is selected Ctrl+Up/Down continue moving the selection. Ctrl+Down
        // with no selection retains the ordinary small scroll behavior.
        {
            let finished = self.finished_blocks.borrow();
            if (lines < 0 || self.selected_block_id.get().is_some())
                && move_finished_block_selection(
                    &finished,
                    &self.selected_block_ids,
                    &self.selected_block_id,
                    &self.selection_anchor_id,
                    &self.block_scroll,
                    lines.signum(),
                )
            {
                return;
            }
        }

        let adj = self.block_scroll.vadjustment();
        let cell_h = self.active_vte.char_height() as f64;
        let step = if cell_h > 0.0 {
            cell_h
        } else {
            adj.step_increment()
        };
        let max_val = (adj.upper() - adj.page_size()).max(adj.lower());
        let value = (adj.value() + step * lines as f64).clamp(adj.lower(), max_val);
        adj.set_value(value);
    }

    /// Select all completed blocks as one Warp-style range. The newest block is
    /// the active edge (strong outline), while the oldest is the fixed anchor
    /// used if the user subsequently contracts the range with Shift+Up.
    pub fn select_all_blocks(&self) {
        if self.fullscreen.get() {
            return;
        }
        let finished = self.finished_blocks.borrow();
        let (Some(first), Some(last)) = (finished.first(), finished.last()) else {
            return;
        };
        {
            let mut selected = self.selected_block_ids.borrow_mut();
            selected.clear();
            selected.extend(finished.iter().map(|block| block.id));
        }
        self.selection_anchor_id.set(Some(first.id));
        self.selected_block_id.set(Some(last.id));
        sync_finished_block_selection(&finished, &self.selected_block_ids, &self.selected_block_id);
        self.active.borrow().grab_focus();
    }

    /// Recall every selected command, in terminal order, into the editable live
    /// prompt. Bracketed paste keeps a multi-selection as a multiline buffer; on
    /// shells without it the existing safe first-line fallback still applies.
    pub fn reinput_selected_commands(&self) {
        if self.fullscreen.get() || self.armed_agent_execution.borrow().is_some() {
            return;
        }
        let finished = self.finished_blocks.borrow();
        let recalled = {
            let selected = self.selected_block_ids.borrow();
            recall_selected_commands_at_prompt(
                &self.pty,
                &self.pty_synced,
                &self.typed_cmd,
                self.bstate.get(),
                &finished,
                &selected,
                self.bracketed_paste.get(),
            )
        };
        if recalled {
            clear_finished_block_selection(
                &finished,
                &self.selected_block_ids,
                &self.selected_block_id,
                &self.selection_anchor_id,
            );
            self.active.borrow().grab_focus();
        }
    }

    /// Remove all completed blocks and all state indexed by those blocks. This
    /// is deliberately pane-local: command-only history remains available in
    /// Ctrl+Shift+H, while optional full block-history persistence is overwritten
    /// immediately so cleared output does not reappear after a crash/restart.
    /// Remove every finished block. Returns how many blocks were cleared; the
    /// removed data is stashed so `undo_clear_blocks` can rebuild it.
    pub fn clear_blocks(&self) -> usize {
        if !self.render_backend.supports_block_mutation() {
            log::debug!("render backend has no Block document to mutate");
            return 0;
        }
        // Bind deletion authority before mutating UI state. A failed save keeps
        // this exact resolved target armed across config path/codec changes,
        // and Undo or Drop retries it with the pane's then-current state.
        self.arm_history_explicit_replace();
        self.clear_find();
        self.active_vte.unselect_all();

        let cleared: Vec<BlockData> = mutate_block_data_and_redraw(
            &self.block_data,
            self.failure_marker_redraw.as_ref(),
            |blocks| blocks.drain(..).collect(),
        );
        let cleared_count = cleared.len();
        // Keep the previous stash when clearing an already-empty pane, so a
        // reflexive second Ctrl+Shift+K cannot destroy the undo snapshot.
        if !cleared.is_empty() {
            *self.cleared_stash.borrow_mut() = cleared;
        }

        let widgets: Vec<gtk::Box> = self
            .finished_blocks
            .borrow_mut()
            .drain(..)
            .map(|block| block.widget().clone())
            .collect();
        let mut pool = self.widget_pool.borrow_mut();
        for widget in widgets {
            self.block_list.remove(&widget);
            pool.release(widget);
        }
        drop(pool);

        // BlockData and FinishedBlock are parallel lists. Virtualization,
        // bookmarks, selection, and unread state all reference their IDs or
        // indices, so clearing only the widgets would corrupt the next block.
        self.bookmarks.borrow_mut().clear();
        self.visible_indices.borrow_mut().clear();
        self.selected_block_ids.borrow_mut().clear();
        self.selected_block_id.set(None);
        self.selection_anchor_id.set(None);
        self.unread_count.set(0);
        set_jump_fab_label(&self.jump_fab, 0);
        self.jump_fab.set_visible(false);
        {
            let mut viewport = self.viewport.borrow_mut();
            viewport.first_visible = 0;
            viewport.last_visible = 0;
        }
        self.block_list.queue_allocate();

        // At an idle prompt, also ask the shell to repaint a clean live input
        // surface. Never inject form-feed into a running/full-screen program.
        if self.bstate.get() == BlockState::AwaitingCommand
            && self.armed_agent_execution.borrow().is_none()
        {
            self.pty.write_bytes(b"\x0c");
        }
        if let Err(err) = self.save_history() {
            log::warn!("save cleared block history: {err}");
        }
        cleared_count
    }

    /// Rebuild the blocks removed by the most recent `clear_blocks`. They are
    /// older than anything created since, so they are reinserted above the
    /// current finished blocks. Returns how many blocks were restored.
    pub fn undo_clear_blocks(&self) -> usize {
        if self.fullscreen.get() {
            // An alt-screen app owns the viewport and history widgets are
            // hidden; keep the stash so undo still works after it exits.
            return 0;
        }
        let mut stash: Vec<BlockData> = std::mem::take(&mut *self.cleared_stash.borrow_mut());
        if stash.is_empty() {
            return 0;
        }
        self.clear_find();
        let max_blocks = self.config.borrow().max_visible_blocks as usize;
        let retention_plan = {
            let finished = self.finished_blocks.borrow();
            plan_completed_block_retention_with_restored(&stash, &finished, max_blocks)
        };
        log_completed_block_retention("restoring cleared blocks", retention_plan);
        let restored_evictions = retention_plan.evict_prefix.min(stash.len());
        stash.drain(..restored_evictions);
        let live_evictions = retention_plan
            .evict_prefix
            .saturating_sub(restored_evictions);
        evict_finished_block_prefix(
            live_evictions,
            &self.finished_blocks,
            &self.block_data,
            &self.block_list,
            &self.widget_pool,
            BlockRemovalRefs {
                selected_ids: &self.selected_block_ids,
                selected: &self.selected_block_id,
                anchor: &self.selection_anchor_id,
                bookmarks: &self.bookmarks,
                visible_indices: &self.visible_indices,
                failure_marker_redraw: self.failure_marker_redraw.as_ref(),
                unread_count: &self.unread_count,
                jump_fab: &self.jump_fab,
            },
        );
        let restored_count = stash.len();

        if restored_count == 0 {
            if let Err(err) = self.save_history() {
                log::warn!("save byte-pruned undo state: {err}");
            }
            return 0;
        }

        let mut restored: Vec<FinishedBlock> = Vec::with_capacity(restored_count);
        {
            // Rebuild with the same overlay the reader uses for new blocks: if a
            // dynamic OSC 10/11/12 color is active, restored blocks must match
            // the recolored live view instead of reverting to theme colors.
            let config = finished_block_config(&self.dynamic_colors, &self.config.borrow());
            let fallback_cols =
                bounded_finished_vte_columns(self.active.borrow().grid_cols() as i64);
            // Everything restored predates the current finished blocks, so the
            // insertion anchor is the pane's first finished widget — or the
            // live input block when the pane has none.
            let anchor: gtk::Widget = self
                .finished_blocks
                .borrow()
                .first()
                .map(|block| block.widget().clone().upcast())
                .unwrap_or_else(|| self.active.borrow().widget().clone().upcast());
            for block in stash.iter_mut() {
                let cols = bounded_finished_vte_columns(if block.cols > 0 {
                    block.cols as i64
                } else {
                    fallback_cols
                });
                block.estimated_height =
                    estimated_finished_block_height_for_text(&config, &block.output, cols);
                let finished = FinishedBlock::new(
                    block.id,
                    &block.prompt,
                    &block.cmd,
                    block.cmd_markup.as_deref(),
                    &block.output,
                    block.exit_code,
                    &config,
                    block.duration_ms,
                    block.end_time_ms,
                    block.cwd.as_deref(),
                    cols,
                );
                finished
                    .widget()
                    .insert_before(&self.block_list, Some(&anchor));
                finished.connect_actions(
                    &self.active_vte,
                    &self.pty,
                    &self.pty_synced,
                    &self.bracketed_paste,
                    &self.typed_cmd,
                    &self.armed_agent_execution,
                    &self.bstate,
                    &self.active,
                );
                finished.connect_scroll_forwarding(&self.block_scroll);
                install_finished_block_selection(
                    &finished,
                    &self.active,
                    &self.finished_blocks,
                    &self.selected_block_ids,
                    &self.selected_block_id,
                    &self.selection_anchor_id,
                );
                restored.push(finished);
            }

            mutate_block_data_and_redraw(
                &self.block_data,
                self.failure_marker_redraw.as_ref(),
                |data| {
                    for block in stash.into_iter().rev() {
                        data.push_front(block);
                    }
                },
            );
        }
        self.finished_blocks.borrow_mut().splice(0..0, restored);

        // Virtualization bookkeeping tracks indices; everything previously
        // visible shifted down by the restored count.
        {
            let mut visible = self.visible_indices.borrow_mut();
            let shifted: std::collections::HashSet<usize> =
                visible.iter().map(|index| index + restored_count).collect();
            *visible = shifted;
        }
        self.update_viewport();
        self.update_block_visibility();
        self.block_list.queue_allocate();
        if let Err(err) = self.save_history() {
            log::warn!("save restored block history: {err}");
        }
        restored_count
    }

    pub(crate) fn apply_failed_filter(&self) -> RecordNavigationResult {
        let Some(record_id) = self.get_failed_blocks().first().copied() else {
            return RecordNavigationResult::NoMatchingRecord;
        };
        self.navigate_to_record_id(record_id, false)
    }

    pub(crate) fn apply_slow_filter(&self) -> RecordNavigationResult {
        let Some(record_id) = self.get_slow_blocks(1000).first().copied() else {
            return RecordNavigationResult::NoMatchingRecord;
        };
        self.navigate_to_record_id(record_id, false)
    }

    pub fn apply_pinned_filter(&self) {
        let finished = self.finished_blocks.borrow();
        let bookmarks = self.bookmarks.borrow();
        if let Some((idx, _)) = finished
            .iter()
            .enumerate()
            .find(|(_, block)| bookmarks.contains(&block.id))
        {
            drop(bookmarks);
            drop(finished);
            self.scroll_to_block(idx);
        }
    }

    pub fn clear_block_filter(&self) {
        self.scroll_to_block(0);
    }

    pub fn jump_to_pinned(&self, direction: i32) {
        let marked: Vec<usize> = {
            let finished = self.finished_blocks.borrow();
            let bookmarks = self.bookmarks.borrow();
            finished
                .iter()
                .enumerate()
                .filter(|(_, block)| bookmarks.contains(&block.id))
                .map(|(idx, _)| idx)
                .collect()
        };
        self.jump_to_marked_index(&marked, direction);
    }

    /// Jump to the previous / next failed (non-zero exit) block, wrapping to
    /// the far end when there is no match in the requested direction.
    pub(crate) fn jump_to_failed(&self, direction: i32) -> RecordNavigationResult {
        let failed = self.get_failed_blocks();
        if failed.is_empty() {
            return RecordNavigationResult::NoMatchingRecord;
        }
        let record_ids = {
            let records = self.render_backend.records();
            records.iter().map(|record| record.id()).collect::<Vec<_>>()
        };
        let target = step_marked_record_ids(
            &record_ids,
            &failed,
            self.selected_block_id
                .get()
                .or_else(|| self.navigated_record_id.get()),
            direction,
        );
        let Some(record_id) = target else {
            return RecordNavigationResult::NoMatchingRecord;
        };
        self.navigate_to_record_id(record_id, false)
    }

    /// Shared stepping for pinned/failed navigation: `marked` is an ascending
    /// index list into the finished blocks.
    fn jump_to_marked_index(&self, marked: &[usize], direction: i32) {
        let cur = self.selected_block_id.get().and_then(|id| {
            self.finished_blocks
                .borrow()
                .iter()
                .position(|block| block.id == id)
        });
        if let Some(idx) = step_marked_indices(marked, cur, direction) {
            self.scroll_to_block(idx);
        }
    }

    /// Apply updated theme colors to the block widgets and the live VTE.
    ///
    /// An explicit theme change wins over anything an app set with OSC
    /// 10/11/12: every surface below is repainted from the theme, so the tracked
    /// overrides are dropped too and the next color query answers with the new
    /// theme instead of the superseded app color.
    pub fn apply_theme(&self) {
        clear_dynamic_colors(&self.dynamic_colors);
        let config = self.config.borrow();
        apply_theme_to_vte(&self.active_vte, &config);
        for block in self.finished_blocks.borrow().iter() {
            apply_snapshot_theme_to_vte(&block.command_vte, &config);
            apply_snapshot_theme_to_vte(&block.output_vte, &config);
        }
        install_block_css(&config);
    }

    /// Update font for VTE terminal and block view CSS.
    pub fn set_font(&self, font_desc: &FontDescription) {
        self.active_vte.set_font(Some(font_desc));
        for block in self.finished_blocks.borrow().iter() {
            block.command_vte.set_font(Some(font_desc));
            block.output_vte.set_font(Some(font_desc));
        }
        // Update config and regenerate CSS with new font
        self.config.borrow_mut().font_desc = font_desc.to_string();
        install_block_css(&self.config.borrow());
        (self.layout_active_surface)();
        let refit = self.layout_active_surface.clone();
        glib::idle_add_local_once(move || refit());
    }

    /// Update font scale for VTE terminal and block view CSS.
    pub fn set_font_scale(&self, scale: f64) {
        self.active_vte.set_font_scale(scale);
        for block in self.finished_blocks.borrow().iter() {
            block.command_vte.set_font_scale(scale);
            block.output_vte.set_font_scale(scale);
        }
        self.config.borrow_mut().default_font_scale = scale;
        // Regenerate CSS with updated font scale
        install_block_css(&self.config.borrow());
        (self.layout_active_surface)();
        let refit = self.layout_active_surface.clone();
        glib::idle_add_local_once(move || refit());
    }

    /// Update virtual scrolling viewport state based on scroll position.
    pub fn update_viewport(&self) {
        let adjustment = self.block_scroll.vadjustment();
        let block_data = self.block_data.borrow();
        let Some(viewport) = viewport_state_for_scroll(
            &block_data,
            adjustment.value(),
            adjustment.page_size(),
            self.config.borrow().virtual_scroll_margin,
        ) else {
            return;
        };
        *self.viewport.borrow_mut() = viewport;
    }

    /// Update block visibility based on viewport: show visible blocks, hide off-screen ones.
    pub fn update_block_visibility(&self) {
        let adjustment = self.block_scroll.vadjustment();
        let margin = self.config.borrow().virtual_scroll_margin;
        let block_data = self.block_data.borrow();
        let Some((strict, loose)) = viewport_states_for_scroll(
            &block_data,
            adjustment.value(),
            adjustment.page_size(),
            margin,
        ) else {
            return;
        };
        drop(block_data);
        let new_visible =
            stable_visible_indices(&strict, Some(&loose), &self.visible_indices.borrow());
        *self.viewport.borrow_mut() = strict;
        let finished = self.finished_blocks.borrow();
        let mut block_data = self.block_data.borrow_mut();
        let mut visible = self.visible_indices.borrow_mut();
        apply_visible_indices(&finished, &mut block_data, &mut visible, new_visible);
        (self.failure_marker_redraw)();
    }

    /// Grid size (cols, rows) of the live VTE, for the bottom bar's grid
    /// segment.
    pub fn grid_size(&self) -> (i64, i64) {
        (self.active_vte.column_count(), self.active_vte.row_count())
    }

    /// Collect a snapshot of internal runtime state for the debug dashboard.
    /// Returns labelled sections, each a list of (key, value) rows.
    pub fn debug_info(&self) -> DebugInfo {
        let out_cols = self.active_vte.column_count();
        let out_rows = self.active_vte.row_count();

        let finished_len = self.finished_blocks.borrow().len();
        let block_data_len = self.block_data.borrow().len();
        let failed = self.get_failed_blocks().len();
        let slow = self.get_slow_blocks(1000).len();
        let total_output_bytes: usize = self
            .block_data
            .borrow()
            .iter()
            .map(|b| b.output.len())
            .sum();
        let total_height: i64 = self
            .block_data
            .borrow()
            .iter()
            .map(|block| i64::from(block.estimated_height.max(1)))
            .sum();
        let viewport = self.viewport.borrow().clone();
        let visible = self.visible_indices.borrow().len();
        let selected = self
            .selected_block_id
            .get()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".to_string());
        let selected_count = self.selected_block_ids.borrow().len();

        vec![
            (
                "State",
                vec![
                    (
                        "Block state".to_string(),
                        format!("{:?}", self.bstate.get()),
                    ),
                    (
                        "Mouse reporting".to_string(),
                        format!("{:?}", self.mouse_reporting_mode.get()),
                    ),
                    (
                        "Alt screen visible".to_string(),
                        (self.fullscreen.get() || self.bstate.get() == BlockState::AltScreen)
                            .to_string(),
                    ),
                ],
            ),
            (
                "PTY",
                vec![
                    ("PID".to_string(), self.pty.pid_i32().to_string()),
                    ("CWD".to_string(), self.current_cwd.borrow().clone()),
                    (
                        "Output grid".to_string(),
                        format!("{out_cols} × {out_rows}"),
                    ),
                ],
            ),
            (
                "Blocks",
                vec![
                    (
                        "Render backend".to_string(),
                        format!(
                            "{} ({} records)",
                            self.render_backend.debug_name(),
                            self.render_backend.records().len()
                        ),
                    ),
                    ("Finished blocks".to_string(), finished_len.to_string()),
                    ("Block data entries".to_string(), block_data_len.to_string()),
                    ("Failed blocks".to_string(), failed.to_string()),
                    ("Slow blocks (>1s)".to_string(), slow.to_string()),
                    (
                        "Total output bytes".to_string(),
                        total_output_bytes.to_string(),
                    ),
                    ("Selected block id".to_string(), selected),
                    (
                        "Selected block count".to_string(),
                        selected_count.to_string(),
                    ),
                ],
            ),
            (
                "Viewport",
                vec![
                    (
                        "First visible".to_string(),
                        viewport.first_visible.to_string(),
                    ),
                    (
                        "Last visible".to_string(),
                        viewport.last_visible.to_string(),
                    ),
                    ("Total height".to_string(), format!("{total_height}px")),
                    ("Realized widgets".to_string(), visible.to_string()),
                    ("Profiling".to_string(), prof_enabled().to_string()),
                ],
            ),
        ]
    }

    pub fn scroll_to_block(&self, block_index: usize) {
        let finished = self.finished_blocks.borrow();
        if block_index >= finished.len() {
            return;
        }
        if let Some(block) = finished.get(block_index) {
            replace_finished_block_selection(
                &finished,
                &self.selected_block_ids,
                &self.selected_block_id,
                &self.selection_anchor_id,
                Some(block.id),
            );
            let adj = self.block_scroll.vadjustment();
            // A virtualized widget may be unmapped, in which case GTK reports
            // no meaningful coordinates (or 0, 0). Use the retained height
            // estimates to bring it into the realized range first; then correct
            // to the exact GTK position after layout. This makes failed/slow/
            // bookmarked-block shortcuts dependable even in long sessions.
            let estimated_top: i32 = self
                .block_data
                .borrow()
                .iter()
                .take(block_index)
                .map(|data| data.estimated_height.max(1))
                .sum();
            let max_value = (adj.upper() - adj.page_size()).max(adj.lower());
            adj.set_value((estimated_top as f64).clamp(adj.lower(), max_value));

            let widget = block.widget().clone();
            let scroll = self.block_scroll.clone();
            glib::idle_add_local_once(move || {
                if let Some(point) =
                    widget.compute_point(&scroll, &gtk::graphene::Point::new(0.0, 0.0))
                {
                    let adj = scroll.vadjustment();
                    let max_value = (adj.upper() - adj.page_size()).max(adj.lower());
                    adj.set_value((point.y() as f64).clamp(adj.lower(), max_value));
                }
            });
        }
    }

    /// Delete a block by ID (for right-click menu).
    pub fn delete_block_by_id(&self, block_id: u64) {
        let mut finished = self.finished_blocks.borrow_mut();
        let Some(pos) = finished.iter().position(|b| b.id == block_id) else {
            return;
        };
        let unread = unread_after_index_removal(finished.len(), self.unread_count.get(), pos);
        self.unread_count.set(unread);
        set_jump_fab_label(&self.jump_fab, unread);
        let block_to_remove = finished.remove(pos);
        let widget_to_release = block_to_remove.widget().clone();
        self.block_list.remove(&widget_to_release);
        // Return widget to pool for potential reuse
        self.widget_pool.borrow_mut().release(widget_to_release);
        remove_finished_block_from_selection(
            &finished,
            &self.selected_block_ids,
            &self.selected_block_id,
            &self.selection_anchor_id,
            block_id,
        );
        drop(finished);

        // Keep the serializable record list in lockstep with the widget list;
        // otherwise the two desync and count-based eviction / id lookups drift.
        mutate_block_data_and_redraw(
            &self.block_data,
            self.failure_marker_redraw.as_ref(),
            |blocks| {
                let removed = blocks.remove(pos);
                debug_assert_eq!(removed.as_ref().map(|block| block.id), Some(block_id));
            },
        );
        self.bookmarks.borrow_mut().remove(&block_id);
        // Stored indices no longer identify the same widgets after removal.
        // Recompute them on the next viewport update rather than retaining a
        // stale set that can keep an unrelated block hidden.
        self.visible_indices.borrow_mut().clear();
        self.update_viewport();
        self.update_block_visibility();
    }

    /// Most-recent-first deduplicated list of finished-block command lines.
    /// Used to populate the Ctrl+Shift+H history palette. The first entry is
    /// the most recent unique command; whitespace-only commands are dropped.
    pub fn command_history(&self) -> Vec<String> {
        let records = self.render_backend.records();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(MAX_COMMAND_HISTORY_ENTRIES.min(records.len()));
        for record in records.iter().rev() {
            let cmd = record.command().trim();
            if cmd.is_empty() || !recalled_command_is_safe(cmd) {
                continue;
            }
            if seen.insert(cmd) {
                out.push(cmd.to_string());
                if out.len() == MAX_COMMAND_HISTORY_ENTRIES {
                    break;
                }
            }
        }
        out
    }

    /// Snapshot the currently selected finished block as an `ai::BlockContext`,
    /// truncating the output to `head + tail = 2*lines_per_side + 1` lines so
    /// a `cargo build` block doesn't blow the request budget. Returns `None`
    /// when no block is selected (Ctrl+Shift+Q from the live cell etc.).
    pub fn selected_block_context(&self, lines_per_side: usize) -> Option<crate::ai::BlockContext> {
        let id = self.selected_block_id.get()?;
        let finished = self.finished_blocks.borrow();
        let block = finished.iter().find(|b| b.id == id)?;
        let data = self.block_data.borrow();
        let bd = data.iter().find(|b| b.id == id);

        let output =
            block.with_stripped_output(|s| crate::ai::truncate_for_context(s, lines_per_side));
        let truncated = output.contains("lines elided") || output.contains("bytes elided");
        Some(crate::ai::BlockContext {
            cmd: block.cmd_text.clone(),
            output,
            cwd: bd.and_then(|b| b.cwd.clone()),
            exit_code: exit_code_for_i32_api(bd.and_then(|b| b.exit_code)),
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        agent_command_is_safe, agent_prompt_boundary_is_trusted, append_bounded_section,
        append_bounded_text_tail, append_typed_command_shadow, approved_command_submission_payload,
        background_output_has_visible_text, block_clipboard_text, block_duration_ms,
        build_clipboard_paste, build_color_query_reply, build_command_recall,
        build_keyboard_query_reply, claim_next_unused_block_id, classify_command_prompt_status,
        clear_dynamic_colors, close_zone_marker, coalesce_bytes_events,
        command_capture_range_is_bounded, command_end_matches_started_id,
        command_id_uses_shell_token, decide_agent_command_end, emit_alt_screen_transition,
        estimated_finished_block_height_for_text, failed_block_marker_fractions,
        failed_block_marker_fractions_from_entries, failed_block_marker_fractions_legacy,
        feed_with_zone_marker, finished_block_config, finished_command, finished_layout_key,
        into_payload_plain_output, is_post_command_metadata, live_content_extent_for,
        live_output_text, live_visible_rows, materialize_plain_output,
        materialize_plain_output_legacy, mounted_jumpable_records, mutate_block_data_and_redraw,
        normalize_captured_command, notification_permitted, parse_color_spec, plan_prompt_zone,
        pop_typed_command_shadow, process_block_id_namespace, prompt_anchor_for_surface,
        prompt_anchor_rebases_on_row_delta, prompt_zone_to_reopen_after_alt, rebase_prompt_anchor,
        record_external_input, record_unified_zone, resolve_command_text,
        reviewed_pre_command_bytes_are_identity_neutral, reviewed_submission_matches,
        screen_relative_cpr_row, selected_blocks_markdown, selected_command_text,
        selected_id_range, shell_argv_supports_agent_ids, stable_visible_indices,
        step_marked_indices, step_marked_record_ids, stranded_focus_key_recovers, strip_ansi,
        strip_ansi_with_clear_detect, take_armed_agent_execution, take_background_output,
        unread_after_index_removal, unread_after_prefix_eviction, viewport_page_size_changed,
        viewport_state_for_scroll, viewport_states_for_scroll, visible_indices_for_viewport,
        zone_output_snapshot_from_plain, zone_output_snapshot_from_ring, AgentCommandEndDecision,
        AgentExecutionLostCallbacks, AltScreenCallbacks, AltScreenTransition, AnchorSettleArgs,
        ArmedAgentExecution, BackendRecords, BlockData, BlockFinishedCallback,
        BlockFinishedCallbacks, BlockRenderPayloadAccessor, BlockState, CommandFinishedCallbacks,
        CommandFinishedEvent, CommandPromptStatus, CommandStartedCallbacks, CommandStartedEvent,
        CommandTextSource, CompletedCommandRecord, DynamicColors, DynamicColorsRc, EngineState,
        PendingZone, ReaderCtx, RenderBackend, ResetAwareParserPart, ResetAwareParserSplitter,
        ReviewedSubmission, ReviewedSubmissionPhase, SelectionFeedHold, ShellCapabilityObserver,
        SubmissionSurface, TerminalResetKind, UnifiedZoneStore, VerifiedSubmissionCtx,
        ZoneMarkerInjector, ZoneOutputSnapshot, BLOCK_ID_SEQUENCE_LIMIT, LAST_NOTIFICATION_AT,
        MAX_RAW_OUTPUT_BYTES, MAX_RECALLED_COMMAND_BYTES, MAX_TOTAL_SNAPSHOT_BYTES,
        MAX_TYPED_COMMAND_SHADOW_BYTES, MAX_ZONE_SNAPSHOT_BYTES, NOTIFICATION_MIN_INTERVAL,
        TRUNCATED_COMMAND_PLACEHOLDER, UNAVAILABLE_COMMAND_PLACEHOLDER, ZONE_MARKER_CLOSE,
    };
    use crate::agent::{AgentExecutionRef, AgentSession};
    use crate::config::Config;
    use crate::parser::{ColorKind, CommandMeta, KeyboardProtocolQuery, ParserEvent};
    use relm4::gtk::gdk::RGBA;
    use std::cell::{Cell, RefCell};
    use std::collections::{HashSet, VecDeque};
    use std::rc::Rc;
    use std::time::{Instant, SystemTime};

    #[test]
    fn live_card_grows_with_output_but_never_past_the_viewport() {
        // Idle prompt and a command that has printed nothing yet both sit on
        // the floor, so pressing Enter changes no height at all.
        assert_eq!(live_visible_rows(1, 40), 6);
        assert_eq!(live_visible_rows(6, 40), 6);
        // Then one row per row of output ...
        assert_eq!(live_visible_rows(7, 40), 7);
        assert_eq!(live_visible_rows(39, 40), 39);
        // ... up to the viewport, which is where the old code started.
        assert_eq!(live_visible_rows(40, 40), 40);
        assert_eq!(live_visible_rows(4000, 40), 40);
        // A pane too short for the floor still gets a card, never zero rows.
        assert_eq!(live_visible_rows(1, 3), 3);
        assert_eq!(live_visible_rows(9, 3), 3);
    }

    #[test]
    fn command_start_measurements_before_output_are_provisional() {
        // CommandStart lays out synchronously while VTE applies feed() bytes
        // asynchronously. A failed measurement in that gap must leave the
        // fresh live card at its floor and, critically, must not latch a
        // viewport-sized high-water mark.
        assert_eq!(
            super::live_visible_rows_for_measurement(None, false, false, 6, 40),
            (6, 6)
        );
        // A coherent-looking sample can be stale too: a grid resize and VTE's
        // adjustment update do not settle atomically. Ignore it until output.
        assert_eq!(
            super::live_visible_rows_for_measurement(Some(38), false, false, 6, 40),
            (6, 6)
        );
        // A multiline input baseline remains visible instead of shrinking to
        // the default floor while its command waits to produce output.
        assert_eq!(
            super::live_visible_rows_for_measurement(Some(2), false, false, 9, 40),
            (9, 9)
        );

        // Raw bytes reach the capture before VTE has settled its cursor and
        // adjustment. The first chunk's transient None therefore preserves the
        // last proven height too.
        assert_eq!(
            super::live_visible_rows_for_measurement(None, true, false, 6, 40),
            (6, 6)
        );
        assert_eq!(
            super::live_visible_rows_for_measurement(Some(8), true, false, 6, 40),
            (8, 8)
        );

        // ED3/RIS is known from the parser barrier before its bytes reach VTE,
        // so it can safely expose and latch the full viewport.
        assert_eq!(
            super::live_visible_rows_for_measurement(None, true, true, 6, 40),
            (40, 40)
        );
        assert_eq!(
            super::live_visible_rows_for_measurement(Some(2), true, false, 40, 40),
            (40, 40)
        );
    }

    #[test]
    fn live_content_extent_is_screen_relative_not_an_absolute_ring_row() {
        // VTE row numbers are absolute and climb for the whole session, so the
        // extent is measured from the top of the *screen* (`upper - page`).
        // A full 36-row screen whose cursor sits on its last row:
        assert_eq!(live_content_extent_for(0.0, 63.0, 36.0, 62), Some(36));
        // The same terminal after `[H[2J`: the ring kept climbing (upper 99),
        // the cursor is six rows into the current screen.
        assert_eq!(live_content_extent_for(0.0, 99.0, 36.0, 68), Some(6));
        // A fresh block: cursor at the top of the screen is one row, not zero.
        assert_eq!(live_content_extent_for(0.0, 36.0, 36.0, 0), Some(1));
        // Scrolled-back views must not inflate it: `value` moves, `upper -
        // page` does not, and the cursor stays on the live screen.
        assert_eq!(live_content_extent_for(0.0, 200.0, 36.0, 170), Some(7));
        // The cursor resting one row past the last written row is normal.
        assert_eq!(live_content_extent_for(0.0, 54.0, 20.0, 54), Some(21));
        // `ESC[3J` renumbers the adjustment but not the cursor: measured, a
        // 20-row screen reported upper=20 with the cursor still at ring row 55.
        // Unmeasurable, so the card must fall back to the full viewport.
        assert_eq!(live_content_extent_for(0.0, 20.0, 20.0, 55), None);
        assert_eq!(live_content_extent_for(0.0, 20.0, 20.0, 113), None);
        // Degenerate adjustments never produce a card that hides output.
        assert_eq!(live_content_extent_for(0.0, 0.0, 0.0, 0), Some(1));
        assert_eq!(live_content_extent_for(f64::NAN, 99.0, 36.0, 68), None);
    }

    #[test]
    fn prompt_anchor_policy_is_shared_by_every_surface_reader() {
        assert_eq!(prompt_anchor_for_surface(false, (11, 6), 7, 5), (11, 6));
        assert_eq!(prompt_anchor_for_surface(false, (3, 2), 5, 8), (3, 2));
        assert_eq!(prompt_anchor_for_surface(true, (11, 6), 7, 5), (11, 4));
        assert_eq!(
            prompt_anchor_for_surface(true, (3, 2), 5, 8),
            rebase_prompt_anchor((3, 2), 5, 8)
        );
    }

    #[test]
    fn backend_switch_selects_block_rebase_and_unified_identity() {
        assert!(prompt_anchor_rebases_on_row_delta(false));
        assert!(!prompt_anchor_rebases_on_row_delta(true));

        let provisional = (7, 3);
        assert_eq!(
            prompt_anchor_for_surface(
                prompt_anchor_rebases_on_row_delta(false),
                provisional,
                24,
                26,
            ),
            (7, 5),
            "Block rebases across compact/full row-count changes",
        );
        assert_eq!(
            prompt_anchor_for_surface(
                prompt_anchor_rebases_on_row_delta(true),
                provisional,
                24,
                26,
            ),
            provisional,
            "Unified keeps its stable full-size surface anchor",
        );
    }

    #[test]
    fn unified_zones_retain_identity_and_drop_the_oldest_past_the_cap() {
        let finalized = |id, command: &str| CompletedCommandRecord {
            id,
            cmd: command.to_string(),
            exit_code: Some(id as i32),
            start_time_ms: Some(id),
            end_time_ms: Some(id * 100),
            duration_ms: Some(id * 10),
            cwd: Some("/work".to_string()),
            is_background: false,
        };
        let mut zones = UnifiedZoneStore::new();
        let _ = record_unified_zone(&mut zones, finalized(1, "first"), 2);
        zones.insert_snapshot(
            1,
            ZoneOutputSnapshot {
                plain: "first output".to_string(),
                truncated: false,
            },
        );
        let _ = record_unified_zone(&mut zones, finalized(2, "second"), 2);
        let _ = record_unified_zone(&mut zones, finalized(3, "third"), 2);
        assert_eq!(zones.records.len(), 2);
        assert_eq!(zones.records[0].id, 2);
        assert_eq!(
            zones.snapshot(1),
            None,
            "a drained record takes its snapshot with it"
        );
        assert_eq!(zones.snapshot_bytes, 0);
        assert_eq!(
            zones.records[1],
            CompletedCommandRecord {
                id: 3,
                cmd: "third".to_string(),
                exit_code: Some(3),
                start_time_ms: Some(3),
                end_time_ms: Some(300),
                duration_ms: Some(30),
                cwd: Some("/work".to_string()),
                is_background: false,
            }
        );

        let _ = record_unified_zone(&mut zones, finalized(4, "fourth"), 0);
        assert_eq!(
            zones.records.len(),
            1,
            "a zero configuration still stays bounded"
        );
        assert_eq!(zones.records[0].id, 4);
    }

    /// The global budget only ever removes snapshot BYTES, oldest record
    /// first; every record survives and later looks up its own snapshot by id,
    /// so eviction can never shift output onto a different command.
    #[test]
    fn snapshot_budget_evicts_oldest_first_and_keeps_ids_aligned() {
        let record = |id: u64| CompletedCommandRecord {
            id,
            cmd: format!("command-{id}"),
            exit_code: Some(0),
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: None,
            cwd: None,
            is_background: false,
        };
        let mut zones = UnifiedZoneStore::new();
        for id in 1..=3 {
            let _ = record_unified_zone(&mut zones, record(id), 10);
            zones.insert_snapshot(
                id,
                ZoneOutputSnapshot {
                    plain: format!("output-{id}-{}", "x".repeat(96)),
                    truncated: false,
                },
            );
        }
        let per_snapshot = zones.snapshot_bytes / 3;

        zones.enforce_snapshot_budget(per_snapshot * 2);
        assert_eq!(zones.records.len(), 3, "records are never removed");
        assert_eq!(zones.snapshot(1), None, "the oldest snapshot goes first");
        assert!(zones.snapshot(2).is_some());
        assert_eq!(zones.snapshot_bytes, per_snapshot * 2);
        assert!(zones
            .snapshot(3)
            .is_some_and(|snapshot| snapshot.plain.starts_with("output-3-")));

        zones.enforce_snapshot_budget(0);
        assert_eq!(zones.snapshot_bytes, 0);
        assert!((1..=3).all(|id| zones.snapshot(id).is_none()));
        assert_eq!(zones.records.len(), 3);
    }

    #[test]
    fn zone_snapshot_takes_the_bounded_tail_and_reports_truncation() {
        let mut ring: VecDeque<u8> = b"head head head\r\ntail line\r\n".iter().copied().collect();
        let snapshot = zone_output_snapshot_from_ring(&mut ring, false, 11).expect("tail snapshot");
        assert_eq!(snapshot.plain, "tail line");
        assert!(snapshot.truncated);

        let complete =
            zone_output_snapshot_from_ring(&mut ring, false, 4096).expect("full snapshot");
        assert_eq!(complete.plain, "head head head\ntail line");
        assert!(!complete.truncated);
    }

    /// A byte-bounded tail cut can land inside an escape sequence; its
    /// introducer lies before the cut, so the stripper would otherwise emit the
    /// parameter bytes as literal text.
    #[test]
    fn zone_snapshot_tail_swallows_a_leading_partial_escape_sequence() {
        let mut ring: VecDeque<u8> = b"before\r\n\x1b[38;5;196mred tail\x1b[0m\r\n"
            .iter()
            .copied()
            .collect();
        // The cut lands between `\x1b[38;` and `5;196m`: the retained bytes
        // start mid-parameters.
        let max_bytes = b"5;196mred tail\x1b[0m\r\n".len();
        let snapshot =
            zone_output_snapshot_from_ring(&mut ring, false, max_bytes).expect("tail snapshot");
        assert_eq!(snapshot.plain, "red tail");
        assert!(snapshot.truncated);

        // A cut inside a multi-byte scalar resumes at the next boundary, so
        // no replacement character leads the snapshot.
        let mut ring: VecDeque<u8> = "pad界\x1b[0mtail".bytes().collect();
        let snapshot = zone_output_snapshot_from_ring(&mut ring, false, "\x1b[0mtail".len() + 2)
            .expect("tail snapshot");
        assert_eq!(snapshot.plain, "tail");
        assert!(snapshot.truncated);
    }

    /// An escape sequence can be longer than the whole tail bound, and its
    /// terminator can be the last captured byte or absent altogether. Skipping
    /// it must never consume the buffer: an empty snapshot is stored as no
    /// snapshot at all, which reads exactly like a command that printed
    /// nothing and carries no truncation signal either.
    #[test]
    fn zone_snapshot_tail_survives_an_escape_sequence_longer_than_the_bound() {
        let max_bytes = 4096;

        // OSC 52 clipboard write terminated by the final captured byte.
        let terminated = format!(
            "200 KB of log\r\n\x1b]52;c;{}\x07",
            "QUJD".repeat(max_bytes)
        );
        // The tail helper's own invariant, whoever calls it: a non-empty
        // capture never yields an empty tail.
        let (tail, cut) = super::bounded_output_tail(terminated.as_bytes(), max_bytes);
        assert!(cut);
        assert!(!tail.is_empty(), "the escape skip must not eat the buffer");

        let mut ring: VecDeque<u8> = terminated.bytes().collect();
        let snapshot =
            zone_output_snapshot_from_ring(&mut ring, false, max_bytes).expect("tail snapshot");
        assert!(snapshot.plain.contains("QUJD"));
        assert!(snapshot.truncated);

        // The same payload with no terminator at all.
        let unterminated = format!("200 KB of log\r\n\x1b]52;c;{}", "QUJD".repeat(max_bytes));
        let (tail, cut) = super::bounded_output_tail(unterminated.as_bytes(), max_bytes);
        assert!(cut);
        assert!(!tail.is_empty(), "the escape skip must not eat the buffer");

        let mut ring: VecDeque<u8> = unterminated.bytes().collect();
        let snapshot =
            zone_output_snapshot_from_ring(&mut ring, false, max_bytes).expect("tail snapshot");
        assert!(snapshot.plain.contains("QUJD"));
        assert!(snapshot.truncated);

        // A skip that lands inside the buffer but leaves nothing visible falls
        // back to the unadjusted cut: stray parameter characters still report
        // that this command produced output.
        let mut ring: VecDeque<u8> = b"xxxxxxxx\x1b]0;title\x07\n".iter().copied().collect();
        let snapshot = zone_output_snapshot_from_ring(&mut ring, false, "title\x07\n".len())
            .expect("tail snapshot");
        assert!(snapshot.plain.starts_with("title"));
        assert!(snapshot.truncated);
    }

    /// `truncated` has exactly two sources here, because the plain replay has
    /// neither a grid budget nor cursor padding: a line far past any terminal
    /// width survives whole, and the command's verdict lines after it survive
    /// with it. A replay that ever starts discarding bytes would have to
    /// report that loss into the snapshot as well.
    #[test]
    fn zone_snapshot_keeps_every_byte_the_plain_replay_receives() {
        let raw = format!(
            "\x1b[32mbuilding\x1b[0m\r\n{}\r\nERROR: build failed\r\n",
            "w".repeat(20_000)
        );
        assert!(raw.len() < MAX_ZONE_SNAPSHOT_BYTES);
        let mut ring: VecDeque<u8> = raw.bytes().collect();

        let snapshot = zone_output_snapshot_from_ring(&mut ring, false, MAX_ZONE_SNAPSHOT_BYTES)
            .expect("captured output");
        assert!(snapshot.plain.contains("ERROR: build failed"));
        assert!(!snapshot.truncated);
    }

    /// The per-zone cap and the retention budget must bound the same quantity.
    /// The memoized-plain path holds up to the whole raw-output bound, so the
    /// cap is applied to the stripped text — the length `insert_snapshot`
    /// charges and eviction acts on — not only to a raw byte window.
    #[test]
    fn zone_snapshot_bounds_the_stripped_text_the_budget_charges() {
        let output = Rc::new(RefCell::new(
            std::iter::repeat_n(b'x', MAX_ZONE_SNAPSHOT_BYTES * 2).collect::<VecDeque<u8>>(),
        ));
        let payload = super::LazyBlockRenderPayload::new(
            String::new(),
            super::CapturedFinalizeOutput::Foreground(output),
            false,
        );
        payload.materialize();
        let snapshot = payload
            .output_snapshot(MAX_ZONE_SNAPSHOT_BYTES)
            .expect("memoized output");
        assert!(snapshot.truncated);

        let mut zones = UnifiedZoneStore::new();
        zones.insert_snapshot(1, snapshot);
        assert_eq!(
            zones.snapshot_bytes, MAX_ZONE_SNAPSHOT_BYTES,
            "the budget charges exactly the quantity the per-zone cap bounds"
        );
    }

    #[test]
    fn zone_snapshot_reports_a_wrapped_ring_as_truncated() {
        let mut ring: VecDeque<u8> = b"23456789".iter().copied().collect();
        let snapshot = zone_output_snapshot_from_ring(&mut ring, true, MAX_ZONE_SNAPSHOT_BYTES)
            .expect("snapshot");
        assert_eq!(snapshot.plain, "23456789");
        assert!(
            snapshot.truncated,
            "front bytes the ring already dropped are truncation even under the tail bound"
        );
    }

    #[test]
    fn zone_snapshot_never_fabricates_an_empty_string() {
        let mut empty: VecDeque<u8> = VecDeque::new();
        assert_eq!(
            zone_output_snapshot_from_ring(&mut empty, false, MAX_ZONE_SNAPSHOT_BYTES),
            None
        );

        let mut ansi_only: VecDeque<u8> = b"\x1b[2K\r".iter().copied().collect();
        assert_eq!(
            zone_output_snapshot_from_ring(&mut ansi_only, false, MAX_ZONE_SNAPSHOT_BYTES),
            None
        );

        assert_eq!(
            zone_output_snapshot_from_plain("   \n", MAX_ZONE_SNAPSHOT_BYTES, false),
            None
        );
        let materialized = zone_output_snapshot_from_plain("plain output", 5, false).expect("tail");
        assert_eq!(materialized.plain, "utput");
        assert!(materialized.truncated);
    }

    /// The char-boundary guard on the memoized-plain path: the byte cut can
    /// land inside a scalar there exactly as it can in the raw ring, and
    /// indexing a `str` at a non-boundary panics — on the GTK main thread, at
    /// finalize, for any command whose tail is not ASCII.
    #[test]
    fn zone_snapshot_from_plain_cuts_on_a_char_boundary() {
        let snapshot = zone_output_snapshot_from_plain("padding界tail", "界tail".len() - 1, false)
            .expect("tail snapshot");
        assert_eq!(snapshot.plain, "tail");
        assert!(snapshot.truncated);
    }

    /// Whatever the bound dropped out of the capture's front is invisible in
    /// the decoded text, and under jsh the journal materializes that text
    /// before finalize reads the ring — so the marker has to be carried, not
    /// re-derived.
    #[test]
    fn materialized_payload_still_reports_a_wrapped_capture_as_truncated() {
        let output = Rc::new(RefCell::new(
            b"23456789".iter().copied().collect::<VecDeque<u8>>(),
        ));
        let payload = super::LazyBlockRenderPayload::new(
            String::new(),
            super::CapturedFinalizeOutput::Foreground(output),
            true,
        );

        // The journal submission runs first and consumes the ring.
        payload.materialize();
        let snapshot = payload
            .output_snapshot(MAX_ZONE_SNAPSHOT_BYTES)
            .expect("memoized output");
        assert_eq!(snapshot.plain, "23456789");
        assert!(
            snapshot.truncated,
            "the capture's wrap marker survives materialization"
        );
    }

    #[test]
    fn accessor_snapshot_is_bounded_and_survives_prior_materialization() {
        let output = Rc::new(RefCell::new(
            b"\x1b[32mhello\x1b[0m\r\n"
                .iter()
                .copied()
                .collect::<VecDeque<u8>>(),
        ));
        let payload = super::LazyBlockRenderPayload::new(
            "$ ".to_string(),
            super::CapturedFinalizeOutput::Foreground(output.clone()),
            false,
        );

        let snapshot = payload
            .output_snapshot(MAX_ZONE_SNAPSHOT_BYTES)
            .expect("captured output");
        assert_eq!(snapshot.plain, "hello");
        assert!(!snapshot.truncated);
        assert_eq!(
            payload.materialization_count(),
            0,
            "the bounded path never materializes the card payload"
        );
        assert!(
            !output.borrow().is_empty(),
            "the engine-owned ring is left for later consumers"
        );

        // A journal submission may materialize first; the snapshot then comes
        // from the memoized plain text under the same tail bound.
        payload.materialize();
        let snapshot = payload.output_snapshot(3).expect("memoized output");
        assert_eq!(snapshot.plain, "lo");
        assert!(snapshot.truncated);
        assert_eq!(payload.materialization_count(), 1);

        let empty = super::LazyBlockRenderPayload::new(
            String::new(),
            super::CapturedFinalizeOutput::Background(VecDeque::new()),
            false,
        );
        assert_eq!(empty.output_snapshot(MAX_ZONE_SNAPSHOT_BYTES), None);
    }

    #[test]
    fn payload_plain_output_transfers_the_stripped_allocation() {
        let stripped = "plain output".repeat(128);
        let pointer = stripped.as_ptr();
        let capacity = stripped.capacity();

        let transferred = into_payload_plain_output(stripped);

        assert_eq!(transferred.as_ptr(), pointer);
        assert_eq!(transferred.capacity(), capacity);
        assert_eq!(
            materialize_plain_output("\x1b[32mok\x1b[0m"),
            materialize_plain_output_legacy("\x1b[32mok\x1b[0m")
        );
    }

    #[test]
    #[ignore = "micro-benchmark; run explicitly with --ignored --nocapture"]
    fn payload_plain_output_clone_micro_benchmark() {
        const BYTES: usize = 8 * 1024 * 1024;
        const REPETITIONS: usize = 16;
        let input = "x".repeat(BYTES);

        let legacy_started = Instant::now();
        for _ in 0..REPETITIONS {
            std::hint::black_box(materialize_plain_output_legacy(&input));
        }
        let legacy = legacy_started.elapsed();

        let direct_started = Instant::now();
        for _ in 0..REPETITIONS {
            std::hint::black_box(materialize_plain_output(&input));
        }
        let direct = direct_started.elapsed();

        eprintln!(
            "8 MiB payload plain output ({REPETITIONS}x): legacy={legacy:?} (2 allocations), direct={direct:?} (1 allocation), speedup={:.2}x",
            legacy.as_secs_f64() / direct.as_secs_f64()
        );
    }

    #[test]
    fn unified_cursor_position_report_row_stays_screen_relative() {
        assert_eq!(screen_relative_cpr_row(805, 800, 24), 5);
        assert_eq!(screen_relative_cpr_row(799, 800, 24), 0);
        assert_eq!(screen_relative_cpr_row(900, 800, 24), 23);
        assert_eq!(screen_relative_cpr_row(-3, 0, 0), 0);
    }

    #[test]
    fn zone_marker_injector_uses_canonical_lower_hex_and_reasserts_each_feed() {
        let marker = RefCell::new(ZoneMarkerInjector::with_nonce([
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ]));
        marker.borrow_mut().begin_zone(42);
        let open = b"\x1b]8;;block://000102030405060708090a0b0c0d0e0f/42\x1b\\".to_vec();
        let mut fed = Vec::new();
        feed_with_zone_marker(&marker, b"prompt", |part| fed.push(part.to_vec()));
        feed_with_zone_marker(
            &marker,
            b"\x1b]8;;https://guest.invalid\x1b\\guest\x1b8tail",
            |part| fed.push(part.to_vec()),
        );
        assert_eq!(
            fed,
            vec![
                open.clone(),
                b"prompt".to_vec(),
                open,
                b"\x1b]8;;https://guest.invalid\x1b\\guest\x1b8tail".to_vec(),
            ]
        );
    }

    #[test]
    fn marker_close_fails_closed_and_always_ends_guest_hyperlinks() {
        let marker = RefCell::new(ZoneMarkerInjector::with_nonce([0xab; 16]));
        marker.borrow_mut().begin_zone(7);
        let mut fed = Vec::new();
        close_zone_marker(&marker, Some(8), |part| fed.push(part.to_vec()));
        feed_with_zone_marker(&marker, b"command output", |part| fed.push(part.to_vec()));
        assert_eq!(
            fed,
            vec![ZONE_MARKER_CLOSE.to_vec(), b"command output".to_vec()],
            "a mismatched close drops stale authority before later feeds"
        );

        let disabled = RefCell::new(ZoneMarkerInjector::disabled());
        disabled.borrow_mut().begin_zone(9);
        let mut fed = Vec::new();
        close_zone_marker(&disabled, Some(9), |part| fed.push(part.to_vec()));
        feed_with_zone_marker(&disabled, b"raw", |part| fed.push(part.to_vec()));
        assert_eq!(
            fed,
            vec![ZONE_MARKER_CLOSE.to_vec(), b"raw".to_vec()],
            "accepted C closes a guest hyperlink even when marker entropy failed"
        );
    }

    fn split_resets(bytes: &[u8]) -> Vec<ResetAwareParserPart> {
        ResetAwareParserSplitter::default().feed(bytes)
    }

    fn reset_kinds(parts: &[ResetAwareParserPart]) -> Vec<TerminalResetKind> {
        parts
            .iter()
            .filter_map(|part| match part {
                ResetAwareParserPart::Reset { kind, .. } => Some(*kind),
                ResetAwareParserPart::Bytes(_)
                | ResetAwareParserPart::OpaqueBytes(_)
                | ResetAwareParserPart::ApcSequence(_) => None,
            })
            .collect()
    }

    fn split_bytes(parts: &[ResetAwareParserPart]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for part in parts {
            match part {
                ResetAwareParserPart::Bytes(part)
                | ResetAwareParserPart::OpaqueBytes(part)
                | ResetAwareParserPart::Reset { bytes: part, .. } => bytes.extend_from_slice(part),
                ResetAwareParserPart::ApcSequence(payload) => {
                    bytes.extend_from_slice(b"\x1b_");
                    bytes.extend_from_slice(payload);
                    bytes.extend_from_slice(b"\x1b\\");
                }
            }
        }
        bytes
    }

    fn split_resets_with_borrowed_fast_path(
        splitter: &mut ResetAwareParserSplitter,
        bytes: &[u8],
    ) -> Vec<ResetAwareParserPart> {
        if splitter.can_forward_borrowed(bytes) {
            vec![ResetAwareParserPart::Bytes(bytes.to_vec())]
        } else {
            splitter.feed(bytes)
        }
    }

    #[test]
    fn borrowed_plain_fast_path_matches_splitter_across_control_boundaries() {
        // Include a direct plain chunk and continuations with no ESC while the
        // splitter is inside ESC, CSI, OSC, DCS, APC, plus both reset kinds.
        // The Ground check is what keeps those continuation chunks stateful.
        let chunks: &[&[u8]] = &[
            b"plain-before",
            b"\x1b",
            b"[3",
            b"Jplain-after-ed3",
            b"\x1b]0;ti",
            b"tle",
            b"\x07plain-after-osc",
            b"\x1bP1;2|dcs",
            b"-payload\x1b",
            b"\\plain-after-dcs",
            b"\x1b_Ga=T;AAAA",
            b"BBBB\x1b",
            b"\\plain-after-apc",
            b"\x1b",
            b"cplain-after-ris",
        ];
        let mut legacy = ResetAwareParserSplitter::default();
        let mut optimized = ResetAwareParserSplitter::default();

        for (index, chunk) in chunks.iter().enumerate() {
            let expected = legacy.feed(chunk);
            let actual = split_resets_with_borrowed_fast_path(&mut optimized, chunk);
            assert_eq!(actual, expected, "chunk #{index}: {chunk:?}");
        }
    }

    #[test]
    #[ignore = "micro-benchmark; run explicitly with --ignored --nocapture"]
    fn reset_splitter_plain_borrowed_micro_benchmark() {
        use std::hint::black_box;

        const CHUNK_BYTES: usize = 32 * 1024;
        const ITERATIONS: usize = 16_384;
        let chunk = vec![b'x'; CHUNK_BYTES];

        let mut legacy = ResetAwareParserSplitter::default();
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(legacy.feed(black_box(&chunk)));
        }
        let copied = started.elapsed();

        let optimized = ResetAwareParserSplitter::default();
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            let bytes = black_box(chunk.as_slice());
            if optimized.can_forward_borrowed(bytes) {
                black_box(bytes);
            }
        }
        let borrowed = started.elapsed();

        let mib = (CHUNK_BYTES * ITERATIONS) as f64 / (1024.0 * 1024.0);
        eprintln!(
            "{mib:.0} MiB plain chunks: copied-splitter={copied:?}, borrowed-fast-path={borrowed:?}, speedup={:.2}x",
            copied.as_secs_f64() / borrowed.as_secs_f64()
        );
    }

    #[test]
    fn local_reset_splitter_accepts_only_exact_ed3_and_ris() {
        for sequence in [b"\x1b[3J".as_slice(), b"\x1b[03J", b"\x1b[0003J", b"\x1bc"] {
            let parts = split_resets(sequence);
            assert_eq!(reset_kinds(&parts).len(), 1, "{sequence:?}");
            assert_eq!(split_bytes(&parts), sequence);
        }
        for lookalike in [
            b"\x1b[2J".as_slice(),
            b"\x1b[?3J",
            b"\x1b[3;0J",
            b"\x1b[3:0J",
            b"\x1b[3 J",
            b"\x1b[3K",
            b"\x1b[42949672963J",
            b"\x1bcx\x1b[",
        ] {
            let expected = usize::from(lookalike.starts_with(b"\x1bc"));
            assert_eq!(
                reset_kinds(&split_resets(lookalike)).len(),
                expected,
                "{lookalike:?}"
            );
        }
    }

    #[test]
    fn local_reset_splitter_keeps_bel_and_plain_lookalikes_inside_control_strings() {
        let bytes = b"\x1b]0;osc [3J and c\x07\
            \x1bP1;2|dcs \x07 [3J and c\x1b\\\
            \x1b_apc \x07 [3J and c\x1b\\\
            \x1b^pm \x07 [3J and c\x1b\\\
            \x1bXsos \x07 [3J and c\x1b\\tail";
        let parts = split_resets(bytes);
        assert!(reset_kinds(&parts).is_empty());
        assert_eq!(split_bytes(&parts), bytes);
    }

    #[test]
    fn local_reset_splitter_remembers_split_sos_until_its_st() {
        let mut splitter = ResetAwareParserSplitter::default();
        let mut parts = splitter.feed(b"\x1bXsos prefix");
        parts.extend(splitter.feed(b"fake \x07 [3J and c"));
        parts.extend(splitter.feed(b"\x1b\\tail\x1b[3J"));
        assert_eq!(reset_kinds(&parts), [TerminalResetKind::EraseScrollback]);
        assert_eq!(
            split_bytes(&parts),
            b"\x1bXsos prefixfake \x07 [3J and c\x1b\\tail\x1b[3J"
        );
    }

    #[test]
    fn malformed_control_string_escape_aborts_then_replays_reset_candidate() {
        for introducer in [b'P', b'_', b'^', b'X'] {
            let mut splitter = ResetAwareParserSplitter::default();
            let mut prefix = vec![0x1b, introducer];
            prefix.extend_from_slice(b"payload\x07\x1b");
            let mut parts = splitter.feed(&prefix);
            parts.extend(splitter.feed(b"c\x1b[?2004h"));
            assert_eq!(reset_kinds(&parts), [TerminalResetKind::HardReset]);
        }
    }

    #[test]
    fn raw_reset_splitter_survives_every_byte_boundary_without_loss() {
        let input = b"before\x1b]0;inside [3J\x07middle\x1b[03Jafter\x1bcend";
        let mut splitter = ResetAwareParserSplitter::default();
        let mut parts = Vec::new();
        for byte in input {
            parts.extend(splitter.feed(std::slice::from_ref(byte)));
        }
        assert_eq!(
            reset_kinds(&parts),
            [
                TerminalResetKind::EraseScrollback,
                TerminalResetKind::HardReset
            ]
        );
        assert_eq!(split_bytes(&parts), input);
    }

    #[test]
    fn prompt_zone_plan_reuses_idle_ids_and_aligns_completed_records() {
        let mut next = 10_u64;
        let mut allocate = || {
            next += 1;
            next
        };
        let first = plan_prompt_zone(None, false, &mut allocate);
        let repeated = plan_prompt_zone(
            Some(PendingZone::Prompt(first.prompt_id)),
            false,
            &mut allocate,
        );
        assert_eq!(repeated.prompt_id, first.prompt_id);
        assert_eq!(repeated.completed_record_id, None);
        let foreground = plan_prompt_zone(
            Some(PendingZone::Command(first.prompt_id)),
            true,
            &mut allocate,
        );
        assert_eq!(foreground.completed_record_id, Some(first.prompt_id));
        assert_ne!(foreground.prompt_id, first.prompt_id);
        let background = plan_prompt_zone(
            Some(PendingZone::Prompt(foreground.prompt_id)),
            true,
            &mut allocate,
        );
        assert_eq!(background.completed_record_id, Some(foreground.prompt_id));
        assert_ne!(background.prompt_id, foreground.prompt_id);
    }

    #[test]
    fn only_pre_command_alt_restore_reopens_the_prompt_zone() {
        assert_eq!(
            prompt_zone_to_reopen_after_alt(
                BlockState::AwaitingCommand,
                Some(PendingZone::Prompt(17)),
            ),
            Some(17)
        );
        assert_eq!(
            prompt_zone_to_reopen_after_alt(
                BlockState::CollectingOutput,
                Some(PendingZone::Command(17)),
            ),
            None
        );
    }

    #[test]
    fn linux_zone_nonce_uses_full_128_bit_entropy() {
        #[cfg(target_os = "linux")]
        {
            let first = super::secure_zone_marker_nonce().expect("Linux getrandom is available");
            let second = super::secure_zone_marker_nonce().expect("Linux getrandom is available");
            assert_ne!(first, second);
            assert_eq!(first.len(), 16);
        }
    }

    // ── OSC 133 reader dispatch ──────────────────────────────────────────
    // The recording implementations model a terminal surface instead of
    // echoing the lifecycle's arguments back to it. The column queries differ
    // and Block's anchor performs a real row-delta rebase, making mutations in
    // coordinate choice and effect ordering observable without GTK.
    //
    // Deliberate architecture gaps versus Forge's final harness:
    // - Anvil has no `post_prompt_bytes` fence/ring in `ReaderCtx`, so Forge's
    //   park/overflow/drop trio has no production state to drive here.
    // - Agent correlation uses `AgentExecutionRef` plus private tokenized
    //   command ids rather than Forge's generation-only handoff. Its foreign-
    //   foreground decisions remain covered by the existing
    //   `agent_command_end_requires_shell_foreground_and_a_trusted_pair` and
    //   prompt-boundary tests instead of manufacturing incompatible state.
    // - Swapping in this backend necessarily cannot execute `BlockBackend`'s
    //   GTK finalize body; these tests pin everything the engine hands it and
    //   the surrounding fan-out order, not the widget assembly internals.
    const HARNESS_CWD: &str = "/harness/cwd";
    const SHELL_REPORTED_CWD: &str = "/shell/reported/cwd";
    const PTY_REPLY_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum DispatchQuery {
        CursorAndRows,
        CursorPositionReport,
        CommandCaptureAnchor {
            provisional: (i64, i64),
            recorded_rows: i64,
        },
        CaptureTextRange {
            start: (i64, i64),
            end: (i64, i64),
        },
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct FinalizeRecord {
        prompt: String,
        command: String,
        output_with_ansi: String,
        output_plain: String,
        plain_output_bytes: usize,
        cwd: Option<String>,
        cols: i64,
        estimated_height: i32,
        exit_code: Option<i32>,
        has_duration: bool,
        has_end_time: bool,
        is_background: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum DispatchCall {
        Feed(Vec<u8>),
        EraseScrollback,
        HardReset,
        ResetActiveSurface { preserve_scrollback: bool },
        Focus,
        SyncGeometry,
        Layout,
        MarkDirty,
        ResetLock,
        Finalize(FinalizeRecord),
        EnterChrome,
        EnterFullscreen,
        ExitChrome,
        ExitFullscreen,
        KittyFeed(Vec<u8>),
        KittyAdmitPending,
        ResetKittyPipeline,
        SetSystemClipboard(String),
        DesktopNotify { title: Option<String>, body: String },
        ScheduleAnchorSettle { prompt_generation: u64 },
        Query(DispatchQuery),
        PtyReply(Vec<u8>),
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum MarkerTrace {
        Feed(Vec<u8>),
        Begin(u64),
        Close(Option<u64>),
        Finalize(u64),
        HardReset,
    }

    struct SurfaceGrid {
        rows: Vec<String>,
        cursor: (i64, i64),
        grid_cols: i64,
        live_cols: i64,
    }

    struct RecordingBackend {
        calls: RefCell<Vec<DispatchCall>>,
        marker_trace: RefCell<Vec<MarkerTrace>>,
        /// Everything a live surface would receive, in order. Mirrors
        /// `UnifiedBackend`'s feed: the zone marker is injected here, on the
        /// backend side of the engine, and never touches a captured chunk.
        live_feed: RefCell<Vec<u8>>,
        zone_marker: RefCell<ZoneMarkerInjector>,
        grid: RefCell<SurfaceGrid>,
        finalized_ids: RefCell<Vec<u64>>,
        config: Rc<RefCell<Config>>,
        block_records: RefCell<VecDeque<BlockData>>,
        metadata_records: RefCell<UnifiedZoneStore>,
        metadata_only: Cell<bool>,
        payload_counters: RefCell<Vec<Rc<Cell<usize>>>>,
        kitty_status: Cell<crate::terminal::kitty_graphics::FeedStatus>,
        settle_anchor_now: Cell<bool>,
        admit_probe: RefCell<Option<Box<dyn Fn()>>>,
    }

    impl RecordingBackend {
        fn new(config: Rc<RefCell<Config>>) -> Rc<Self> {
            Rc::new(Self {
                calls: RefCell::new(Vec::new()),
                marker_trace: RefCell::new(Vec::new()),
                live_feed: RefCell::new(Vec::new()),
                zone_marker: RefCell::new(ZoneMarkerInjector::with_nonce([0xab; 16])),
                grid: RefCell::new(SurfaceGrid {
                    rows: vec![String::new(); 24],
                    cursor: (0, 0),
                    grid_cols: 80,
                    live_cols: 80,
                }),
                finalized_ids: RefCell::new(Vec::new()),
                config,
                block_records: RefCell::new(VecDeque::new()),
                metadata_records: RefCell::new(UnifiedZoneStore::new()),
                metadata_only: Cell::new(false),
                payload_counters: RefCell::new(Vec::new()),
                kitty_status: Cell::new(crate::terminal::kitty_graphics::FeedStatus::Pending),
                settle_anchor_now: Cell::new(true),
                admit_probe: RefCell::new(None),
            })
        }

        fn set_metadata_only(&self, metadata_only: bool) {
            self.metadata_only.set(metadata_only);
        }

        fn record(&self, call: DispatchCall) {
            self.calls.borrow_mut().push(call);
        }

        fn calls(&self) -> Vec<DispatchCall> {
            self.calls.borrow().clone()
        }

        fn take_calls(&self) -> Vec<DispatchCall> {
            std::mem::take(&mut *self.calls.borrow_mut())
        }

        fn finalized(&self) -> Vec<DispatchCall> {
            self.calls
                .borrow()
                .iter()
                .filter(|call| matches!(call, DispatchCall::Finalize(_)))
                .cloned()
                .collect()
        }

        fn geometry_pushes(&self) -> usize {
            self.calls
                .borrow()
                .iter()
                .filter(|call| matches!(call, DispatchCall::SyncGeometry))
                .count()
        }

        fn render_row(&self, row: i64, text: &str) {
            let index = usize::try_from(row).expect("a non-negative grid row");
            let mut grid = self.grid.borrow_mut();
            if grid.rows.len() <= index {
                grid.rows.resize(index + 1, String::new());
            }
            grid.rows[index] = text.to_string();
            grid.cursor = (text.chars().count() as i64, row);
        }

        fn set_columns(&self, grid_cols: i64, live_cols: i64) {
            let mut grid = self.grid.borrow_mut();
            grid.grid_cols = grid_cols;
            grid.live_cols = live_cols;
        }

        fn set_row_count(&self, rows: i64) {
            let rows = usize::try_from(rows).expect("a non-negative row count");
            self.grid.borrow_mut().rows.resize(rows, String::new());
        }

        fn set_admit_probe(&self, probe: impl Fn() + 'static) {
            *self.admit_probe.borrow_mut() = Some(Box::new(probe));
        }
    }

    impl RenderBackend for RecordingBackend {
        fn feed_live(&self, bytes: &[u8]) {
            self.marker_trace
                .borrow_mut()
                .push(MarkerTrace::Feed(bytes.to_vec()));
            self.record(DispatchCall::Feed(bytes.to_vec()));
            feed_with_zone_marker(&self.zone_marker, bytes, |part| {
                self.live_feed.borrow_mut().extend_from_slice(part)
            });
        }

        fn begin_prompt_zone(&self, zone_id: u64) {
            self.marker_trace
                .borrow_mut()
                .push(MarkerTrace::Begin(zone_id));
            let open = {
                let mut marker = self.zone_marker.borrow_mut();
                marker.begin_zone(zone_id);
                marker.open_bytes()
            };
            if let Some(open) = open {
                self.live_feed.borrow_mut().extend_from_slice(&open);
            }
        }

        fn close_prompt_zone(&self, zone_id: Option<u64>) {
            self.marker_trace
                .borrow_mut()
                .push(MarkerTrace::Close(zone_id));
            close_zone_marker(&self.zone_marker, zone_id, |bytes| {
                self.live_feed.borrow_mut().extend_from_slice(bytes)
            });
        }

        fn erase_scrollback(&self) {
            self.record(DispatchCall::EraseScrollback);
        }

        fn hard_reset(&self) {
            self.marker_trace.borrow_mut().push(MarkerTrace::HardReset);
            self.record(DispatchCall::HardReset);
        }

        fn reset_active_surface(&self, preserve_scrollback: bool) {
            self.record(DispatchCall::ResetActiveSurface {
                preserve_scrollback,
            });
        }

        fn focus_live_deferred(&self) {
            self.record(DispatchCall::Focus);
        }

        fn sync_geometry_to_pty(&self) {
            self.record(DispatchCall::SyncGeometry);
        }

        fn layout_active_surface(&self) {
            self.record(DispatchCall::Layout);
        }

        fn records(&self) -> BackendRecords<'_> {
            if self.metadata_only.get() {
                BackendRecords::Metadata(self.metadata_records.borrow())
            } else {
                BackendRecords::Blocks(self.block_records.borrow())
            }
        }

        fn record_search_target(
            &self,
            _block_id: u64,
            _is_output: bool,
        ) -> Option<super::RecordSearchTarget> {
            None
        }

        fn completed_search_surfaces(
            &self,
            _max_bytes: usize,
            _deadline_exhausted: &mut dyn FnMut() -> bool,
        ) -> super::BackendSearchBatch {
            super::BackendSearchBatch {
                surfaces: Vec::new(),
                incomplete: false,
                native_fallback: None,
            }
        }

        fn debug_name(&self) -> &'static str {
            "recording"
        }

        fn mark_scroll_dirty(&self) {
            self.record(DispatchCall::MarkDirty);
        }

        fn reset_scroll_lock(&self) {
            self.record(DispatchCall::ResetLock);
        }

        fn finalize_block(
            &self,
            record: &CompletedCommandRecord,
            payload: &dyn BlockRenderPayloadAccessor,
        ) {
            self.finalized_ids.borrow_mut().push(record.id);
            self.marker_trace
                .borrow_mut()
                .push(MarkerTrace::Finalize(record.id));
            self.payload_counters
                .borrow_mut()
                .push(payload.materialization_counter());
            if self.metadata_only.get() {
                // Mirrors `UnifiedBackend::finalize_block`: a bounded snapshot
                // through the accessor, no full materialization.
                let snapshot = payload.output_snapshot(MAX_ZONE_SNAPSHOT_BYTES);
                let mut store = self.metadata_records.borrow_mut();
                record_unified_zone(&mut store, record.clone(), usize::MAX);
                if let Some(snapshot) = snapshot {
                    store.insert_snapshot(record.id, snapshot);
                }
                store.enforce_snapshot_budget(MAX_TOTAL_SNAPSHOT_BYTES);
                return;
            }

            let payload = payload.materialize();
            let cols = super::bounded_finished_vte_columns(self.grid.borrow().grid_cols);
            let truncation_limit = self.config.borrow().truncation_threshold_lines as usize;
            let output_trimmed =
                super::truncate_output_for_journal(&payload.output_plain, truncation_limit);
            let estimated_height = estimated_finished_block_height_for_text(
                &self.config.borrow(),
                &payload.output_plain,
                cols,
            );
            let block_data = BlockData {
                id: record.id,
                prompt: payload.prompt.clone(),
                cmd: record.cmd.clone(),
                cmd_markup: None,
                output: payload.output_plain.trim().to_string(),
                exit_code: record.exit_code,
                estimated_height,
                line_count: output_trimmed.lines().count(),
                start_time_ms: record.start_time_ms,
                end_time_ms: record.end_time_ms,
                duration_ms: record.duration_ms,
                cwd: record.cwd.clone(),
                cols: cols as u16,
            };
            self.block_records
                .borrow_mut()
                .push_back(block_data.clone());
            self.record(DispatchCall::Finalize(FinalizeRecord {
                prompt: payload.prompt.clone(),
                command: record.cmd.clone(),
                output_with_ansi: payload.output_with_ansi.clone(),
                output_plain: block_data.output,
                plain_output_bytes: payload.output_plain.len(),
                cwd: record.cwd.clone(),
                cols,
                estimated_height: block_data.estimated_height,
                exit_code: record.exit_code,
                has_duration: record.duration_ms.is_some(),
                has_end_time: record.end_time_ms.is_some(),
                is_background: record.is_background,
            }));
        }

        fn enter_alt_screen_chrome(&self) {
            self.record(DispatchCall::EnterChrome);
        }

        fn exit_alt_screen_chrome(&self) {
            self.record(DispatchCall::ExitChrome);
        }

        fn enter_fullscreen(&self) {
            self.record(DispatchCall::EnterFullscreen);
        }

        fn exit_fullscreen(&self) {
            self.record(DispatchCall::ExitFullscreen);
        }

        fn kitty_feed(&self, payload: &[u8]) -> crate::terminal::kitty_graphics::FeedStatus {
            self.record(DispatchCall::KittyFeed(payload.to_vec()));
            self.kitty_status.get()
        }

        fn kitty_admit_pending(&self) {
            if let Some(probe) = self.admit_probe.borrow().as_ref() {
                probe();
            }
            self.record(DispatchCall::KittyAdmitPending);
        }

        fn reset_kitty_pipeline(&self) {
            self.record(DispatchCall::ResetKittyPipeline);
        }

        fn set_system_clipboard(&self, text: &str) {
            self.record(DispatchCall::SetSystemClipboard(text.to_string()));
        }

        fn desktop_notify(&self, title: Option<&str>, body: &str) {
            self.record(DispatchCall::DesktopNotify {
                title: title.map(str::to_string),
                body: body.to_string(),
            });
        }

        fn schedule_anchor_settle(&self, args: AnchorSettleArgs) {
            self.record(DispatchCall::ScheduleAnchorSettle {
                prompt_generation: args.prompt_generation,
            });
            if self.settle_anchor_now.get() {
                args.ready.set(true);
            }
        }

        fn cursor_and_rows(&self) -> ((i64, i64), i64) {
            self.record(DispatchCall::Query(DispatchQuery::CursorAndRows));
            let grid = self.grid.borrow();
            (grid.cursor, grid.rows.len() as i64)
        }

        fn cursor_position_report(&self) -> (i64, i64) {
            self.record(DispatchCall::Query(DispatchQuery::CursorPositionReport));
            self.grid.borrow().cursor
        }

        fn command_capture_anchor(
            &self,
            provisional: (i64, i64),
            recorded_rows: i64,
        ) -> (i64, i64) {
            self.record(DispatchCall::Query(DispatchQuery::CommandCaptureAnchor {
                provisional,
                recorded_rows,
            }));
            rebase_prompt_anchor(
                provisional,
                recorded_rows,
                self.grid.borrow().rows.len() as i64,
            )
        }

        fn grid_cols(&self) -> i64 {
            self.grid.borrow().grid_cols
        }

        fn live_column_count(&self) -> i64 {
            self.grid.borrow().live_cols
        }

        fn capture_text_range(
            &self,
            start_row: i64,
            start_col: i64,
            end_row: i64,
            end_col: i64,
        ) -> Option<String> {
            self.record(DispatchCall::Query(DispatchQuery::CaptureTextRange {
                start: (start_row, start_col),
                end: (end_row, end_col),
            }));
            let grid = self.grid.borrow();
            let row_index = |row: i64| {
                usize::try_from(row)
                    .ok()
                    .filter(|index| *index < grid.rows.len())
            };
            let (first, last) = (row_index(start_row)?, row_index(end_row)?);
            if (end_row, end_col) <= (start_row, start_col) {
                return Some(String::new());
            }
            let mut text = String::new();
            for index in first..=last {
                let row: Vec<char> = grid.rows[index].chars().collect();
                if index > first {
                    text.push('\n');
                }
                let from = if index == first {
                    usize::try_from(start_col).unwrap_or(0).min(row.len())
                } else {
                    0
                };
                let to = if index == last {
                    usize::try_from(end_col).unwrap_or(0).clamp(from, row.len())
                } else {
                    row.len()
                };
                text.extend(&row[from..to]);
            }
            Some(text)
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SurfaceRead {
        CursorPosition,
        RowCount,
        PromptAnchor {
            provisional: (i64, i64),
            recorded_rows: i64,
        },
        VisibleEditorText {
            anchor: (i64, i64),
        },
        SuffixIsEmpty,
    }

    struct RecordingSurface {
        reads: RefCell<Vec<SurfaceRead>>,
        cursor: Cell<(i64, i64)>,
        rows: Cell<i64>,
        suffix_is_empty: Cell<Option<bool>>,
    }

    impl RecordingSurface {
        fn new() -> Rc<Self> {
            Rc::new(Self {
                reads: RefCell::new(Vec::new()),
                cursor: Cell::new((0, 0)),
                rows: Cell::new(24),
                suffix_is_empty: Cell::new(None),
            })
        }

        fn reads(&self) -> Vec<SurfaceRead> {
            self.reads.borrow().clone()
        }
    }

    impl SubmissionSurface for RecordingSurface {
        fn cursor_position(&self) -> (i64, i64) {
            self.reads.borrow_mut().push(SurfaceRead::CursorPosition);
            self.cursor.get()
        }

        fn row_count(&self) -> i64 {
            self.reads.borrow_mut().push(SurfaceRead::RowCount);
            self.rows.get()
        }

        fn prompt_anchor(&self, provisional: (i64, i64), recorded_rows: i64) -> (i64, i64) {
            self.reads.borrow_mut().push(SurfaceRead::PromptAnchor {
                provisional,
                recorded_rows,
            });
            prompt_anchor_for_surface(true, provisional, recorded_rows, self.rows.get())
        }

        fn visible_editor_text(&self, anchor: (i64, i64)) -> Option<String> {
            self.reads
                .borrow_mut()
                .push(SurfaceRead::VisibleEditorText { anchor });
            // The polling half of verified submission needs a GLib main loop
            // and is deliberately outside this headless pre-check seam.
            None
        }

        fn suffix_is_empty(&self) -> Option<bool> {
            self.reads.borrow_mut().push(SurfaceRead::SuffixIsEmpty);
            self.suffix_is_empty.get()
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    struct FinishedFanOut {
        command: String,
        exit_code: Option<i32>,
        output_sample: String,
        agent_execution: Option<AgentExecutionRef>,
        blocks_finalized_before: usize,
        state_at_fan_out: BlockState,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct StartedFanOut {
        event: CommandStartedEvent,
        cmd_running: bool,
        state_at_fan_out: BlockState,
        geometry_pushes_before: usize,
    }

    struct ReaderHarness {
        ctx: ReaderCtx,
        backend: Rc<RecordingBackend>,
        surface: Rc<RecordingSurface>,
        pty: Rc<crate::pty::OwnedPty>,
        config: Rc<RefCell<Config>>,
        bstate: Rc<Cell<BlockState>>,
        live_raw_output: Rc<RefCell<VecDeque<u8>>>,
        live_extent_force_full: Rc<Cell<bool>>,
        prompt_end: Rc<Cell<(i64, i64)>>,
        prompt_rows: Rc<Cell<i64>>,
        prompt_ready: Rc<Cell<bool>>,
        blocks_finished: Rc<RefCell<Vec<FinishedFanOut>>>,
        commands_started: Rc<RefCell<Vec<StartedFanOut>>>,
        commands_finished: Rc<RefCell<Vec<CommandFinishedEvent>>>,
        alt_screen: Rc<RefCell<Vec<AltScreenTransition>>>,
        agent_lost: Rc<RefCell<Vec<(AgentExecutionRef, &'static str)>>>,
    }

    type HarnessPreconditionMutation = (&'static str, fn(&ReaderHarness));

    impl ReaderHarness {
        fn new() -> Self {
            Self::with_foreground(true)
        }

        fn with_foreground(foreground: bool) -> Self {
            let surface = RecordingSurface::new();
            let bstate = Rc::new(Cell::new(BlockState::Idle));
            let cmd_running = Rc::new(Cell::new(false));
            let live_raw_output = Rc::new(RefCell::new(VecDeque::new()));
            let live_raw_output_dropped = Rc::new(Cell::new(false));
            let live_extent_force_full = Rc::new(Cell::new(false));
            let config = {
                // Safe defaults read neither the developer's config file nor
                // ANVIL_* environment overrides. Pin every path input used by
                // this harness beside its assertions.
                let mut config = crate::config::load_safe_config().0;
                config.preserve_live_scrollback = false;
                config.truncation_threshold_lines = 50_000;
                config.finished_block_viewport_rows = 24;
                config.finished_block_max_expanded_rows = 5_000;
                config.font_desc = "Monospace 14".to_string();
                config.default_font_scale = 1.0;
                config.allow_remote_clipboard_write = false;
                config.notify_long_blocks = false;
                config.notify_long_block_threshold_ms = 10_000;
                config.max_visible_blocks = 200;
                config.command_history_enabled = false;
                config.command_history_path = None;
                Rc::new(RefCell::new(config))
            };
            let backend = RecordingBackend::new(config.clone());
            let pty = Rc::new(
                crate::pty::OwnedPty::from_openpty(Some(foreground)).expect("open a test PTY"),
            );
            let prompt_end = Rc::new(Cell::new((0, 0)));
            let prompt_rows = Rc::new(Cell::new(24));
            let prompt_ready = Rc::new(Cell::new(false));
            let typed_cmd = Rc::new(RefCell::new(String::new()));
            let idle_dirty = Rc::new(Cell::new(false));
            let pty_synced = Rc::new(Cell::new(false));
            let agent_prompt_generation = Rc::new(Cell::new(0));
            let agent_execution_supported = Rc::new(Cell::new(false));
            let armed_agent_execution = Rc::new(RefCell::new(None));

            let agent_lost = Rc::new(RefCell::new(Vec::new()));
            let agent_lost_callbacks: AgentExecutionLostCallbacks =
                Rc::new(RefCell::new(Vec::new()));
            {
                let seen = agent_lost.clone();
                agent_lost_callbacks
                    .borrow_mut()
                    .push(Box::new(move |execution, reason| {
                        seen.borrow_mut().push((execution, reason));
                    }));
            }

            let blocks_finished = Rc::new(RefCell::new(Vec::new()));
            let block_finished_cbs: BlockFinishedCallbacks = Rc::new(RefCell::new(Vec::new()));
            {
                let seen = blocks_finished.clone();
                let backend_at_fan_out = backend.clone();
                let bstate_at_fan_out = bstate.clone();
                block_finished_cbs
                    .borrow_mut()
                    .push(BlockFinishedCallback::ConditionalOutput {
                        needs_output: Box::new(|_| true),
                        callback: Box::new(
                            move |command, exit_code, output_sample, agent_execution, _duration| {
                                seen.borrow_mut().push(FinishedFanOut {
                                    command,
                                    exit_code,
                                    output_sample: output_sample
                                        .expect("the recording harness requests output"),
                                    agent_execution,
                                    blocks_finalized_before: backend_at_fan_out.finalized().len(),
                                    state_at_fan_out: bstate_at_fan_out.get(),
                                });
                            },
                        ),
                    });
            }

            let commands_started = Rc::new(RefCell::new(Vec::new()));
            let command_started_cbs: CommandStartedCallbacks = Rc::new(RefCell::new(Vec::new()));
            {
                let seen = commands_started.clone();
                let backend_at_fan_out = backend.clone();
                let bstate_at_fan_out = bstate.clone();
                let running_at_fan_out = cmd_running.clone();
                command_started_cbs
                    .borrow_mut()
                    .push(Box::new(move |event| {
                        seen.borrow_mut().push(StartedFanOut {
                            event,
                            cmd_running: running_at_fan_out.get(),
                            state_at_fan_out: bstate_at_fan_out.get(),
                            geometry_pushes_before: backend_at_fan_out.geometry_pushes(),
                        });
                    }));
            }

            let commands_finished = Rc::new(RefCell::new(Vec::new()));
            let command_finished_cbs: CommandFinishedCallbacks = Rc::new(RefCell::new(Vec::new()));
            {
                let seen = commands_finished.clone();
                command_finished_cbs
                    .borrow_mut()
                    .push(Box::new(move |event| seen.borrow_mut().push(event)));
            }

            let alt_screen = Rc::new(RefCell::new(Vec::new()));
            let alt_screen_cbs: AltScreenCallbacks = Rc::new(RefCell::new(Vec::new()));
            {
                let seen = alt_screen.clone();
                alt_screen_cbs
                    .borrow_mut()
                    .push(Box::new(move |transition| {
                        seen.borrow_mut().push(transition);
                    }));
            }

            let verified_submission = VerifiedSubmissionCtx {
                surface: surface.clone(),
                bstate: bstate.clone(),
                pty: pty.clone(),
                typed_cmd: typed_cmd.clone(),
                idle_input_dirty: idle_dirty.clone(),
                pty_synced: pty_synced.clone(),
                prompt_end_pos: prompt_end.clone(),
                prompt_anchor_rows: prompt_rows.clone(),
                prompt_anchor_ready: prompt_ready.clone(),
                prompt_generation: agent_prompt_generation.clone(),
                contents_generation: Rc::new(Cell::new(0)),
                submission: Rc::new(RefCell::new(None::<ReviewedSubmission>)),
                source_id: Rc::new(RefCell::new(None)),
                armed_agent_execution: armed_agent_execution.clone(),
                agent_execution_supported: agent_execution_supported.clone(),
                agent_execution_lost_callbacks: agent_lost_callbacks.clone(),
            };

            let ctx = ReaderCtx {
                backend: backend.clone(),
                bstate_rc: bstate.clone(),
                engine: RefCell::new(EngineState {
                    prev_state: BlockState::Idle,
                    osc133_depth: 0,
                    prompt_buf: String::new(),
                    background_output: VecDeque::new(),
                    background_output_dropped_front: false,
                    vte_typed_cmd: String::new(),
                    prompt_display: String::new(),
                    pending_exit_code: None,
                    shell_duration_ms: None,
                    execution_id_trusted: false,
                    agent_completion_trusted: false,
                    command_cwd: None,
                    pending_zone: None,
                    active_alt_screen_mode: None,
                }),
                live_raw_output_rc: live_raw_output.clone(),
                live_raw_output_dropped_rc: live_raw_output_dropped.clone(),
                live_extent_force_full_rc: live_extent_force_full.clone(),
                typed_cmd_rc: typed_cmd,
                idle_input_dirty_rc: idle_dirty,
                prompt_end_pos_rc: prompt_end.clone(),
                prompt_anchor_rows_rc: prompt_rows.clone(),
                prompt_anchor_ready_rc: prompt_ready.clone(),
                remote_session_cbs: Rc::new(RefCell::new(Vec::new())),
                exited_cbs: Rc::new(RefCell::new(Vec::new())),
                activity_cbs: Rc::new(RefCell::new(Vec::new())),
                alt_screen_cbs,
                command_started_cbs,
                command_finished_cbs,
                mouse_reporting_rc: Rc::new(Cell::new(super::MouseReportingMode::None)),
                bracketed_paste_rc: Rc::new(Cell::new(false)),
                dynamic_colors_rc: Rc::new(Cell::new(DynamicColors::default())),
                config_for_cb: config.clone(),
                parser: Rc::new(RefCell::new(crate::parser::Parser::new())),
                capability_observer: RefCell::new(ShellCapabilityObserver::default()),
                shell_capability_token: "0123456789abcdef0123456789abcdef".to_string(),
                reset_splitter: RefCell::new(ResetAwareParserSplitter::default()),
                reserved_history_block_ids: Rc::new(RefCell::new(HashSet::new())),
                pty_synced_rc: pty_synced,
                ftcs_seen_rc: Rc::new(Cell::new(false)),
                init_cmds_queue_for_cb: Rc::new(RefCell::new(VecDeque::new())),
                pty_for_init: pty.clone(),
                block_start_time_for_cb: Rc::new(Cell::new(None)),
                execution_id_rc: Rc::new(RefCell::new(None)),
                current_cwd_for_cb: Rc::new(RefCell::new(HARNESS_CWD.to_string())),
                event_buf: Rc::new(RefCell::new(Vec::new())),
                cmd_running_rc: cmd_running,
                running_cmd_rc: Rc::new(RefCell::new(String::new())),
                armed_agent_execution_rc: armed_agent_execution,
                agent_prompt_generation_rc: agent_prompt_generation,
                active_agent_execution_rc: Rc::new(Cell::new(None)),
                agent_execution_supported_rc: agent_execution_supported,
                verified_submission,
                block_finished_cbs,
                selection_feed_hold: SelectionFeedHold::new(),
            };
            Self {
                ctx,
                backend,
                surface,
                pty,
                config,
                bstate,
                live_raw_output,
                live_extent_force_full,
                prompt_end,
                prompt_rows,
                prompt_ready,
                blocks_finished,
                commands_started,
                commands_finished,
                alt_screen,
                agent_lost,
            }
        }

        fn feed(&self, event: ParserEvent) {
            self.ctx.handle_event(&event);
        }

        fn feed_all(&self, events: impl IntoIterator<Item = ParserEvent>) {
            for event in events {
                self.feed(event);
            }
        }

        fn feed_raw(&self, bytes: &[u8]) {
            self.ctx.process_parser_input(bytes);
        }

        fn live_output(&self) -> String {
            live_output_text(&self.live_raw_output)
        }

        fn arm_verified_prompt(&self) -> (i64, i64) {
            self.backend.render_row(3, "user@host $ ");
            self.feed_all([
                ParserEvent::PromptStart,
                dispatch_bytes("user@host $ "),
                ParserEvent::PromptEnd,
            ]);
            assert_eq!(self.bstate.get(), BlockState::AwaitingCommand);
            assert!(self.prompt_ready.get());
            let anchor = self.prompt_end.get();
            self.surface.rows.set(self.prompt_rows.get());
            self.surface.cursor.set(anchor);
            self.surface.suffix_is_empty.set(Some(true));
            self.surface.reads.borrow_mut().clear();
            self.backend.take_calls();
            anchor
        }
    }

    fn dispatch_command_start(command: Option<&str>) -> ParserEvent {
        ParserEvent::CommandStart(CommandMeta {
            command: command.map(str::to_string),
            ..CommandMeta::default()
        })
    }

    fn dispatch_command_start_in(command: Option<&str>, cwd: &str) -> ParserEvent {
        ParserEvent::CommandStart(CommandMeta {
            command: command.map(str::to_string),
            cwd: Some(cwd.to_string()),
            ..CommandMeta::default()
        })
    }

    fn dispatch_command_end(exit: Option<i32>) -> ParserEvent {
        ParserEvent::CommandEnd {
            exit,
            meta: CommandMeta::default(),
        }
    }

    fn dispatch_bytes(text: &str) -> ParserEvent {
        ParserEvent::Bytes(text.as_bytes().to_vec())
    }

    fn drive_simple_command(
        harness: &ReaderHarness,
        command: &str,
        output: &str,
        agent: Option<AgentExecutionRef>,
    ) {
        harness.backend.render_row(3, "$ ");
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("$ "),
            ParserEvent::PromptEnd,
        ]);
        harness.backend.render_row(3, &format!("$ {command}"));
        harness.feed(dispatch_command_start(Some(command)));
        harness.feed(dispatch_bytes(output));
        harness.feed(dispatch_command_end(Some(0)));
        if agent.is_some() {
            // This helper's synthetic marks do not carry a shell-authenticated
            // execution id.  Install the already-resolved correlation after D
            // so this test can isolate the block-finished fan-out; the command
            // end trust policy has its own focused coverage below.
            harness.ctx.active_agent_execution_rc.set(agent);
            harness.ctx.engine.borrow_mut().agent_completion_trusted = true;
            assert_eq!(harness.ctx.active_agent_execution_rc.get(), agent);
            assert_eq!(harness.ctx.pty_for_init.shell_is_foreground(), Some(true));
        }
        harness.feed(ParserEvent::PromptStart);
    }

    #[test]
    fn marker_trace_orders_a_prompt_b_c_output_and_finalize_on_one_id() {
        let harness = ReaderHarness::new();
        harness.backend.render_row(3, "$ ");
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("$ "),
            ParserEvent::PromptEnd,
        ]);
        let zone_id = harness
            .ctx
            .engine
            .borrow()
            .pending_zone
            .expect("A opens a prompt zone")
            .id();
        assert_eq!(
            harness.backend.marker_trace.borrow().as_slice(),
            &[
                MarkerTrace::Begin(zone_id),
                MarkerTrace::Feed(b"$ ".to_vec()),
            ],
            "B leaves the prompt marker open"
        );

        harness.backend.render_row(3, "$ printf ok");
        harness.feed(dispatch_command_start(Some("printf ok")));
        assert_eq!(
            harness.ctx.engine.borrow().pending_zone,
            Some(PendingZone::Command(zone_id))
        );
        harness.feed(dispatch_bytes("ok\r\n"));
        harness.feed(dispatch_command_end(Some(0)));
        harness.feed(ParserEvent::PromptStart);
        let next_id = harness
            .ctx
            .engine
            .borrow()
            .pending_zone
            .expect("the following A opens a fresh prompt zone")
            .id();
        assert_ne!(next_id, zone_id);
        assert_eq!(
            harness.backend.marker_trace.borrow().as_slice(),
            &[
                MarkerTrace::Begin(zone_id),
                MarkerTrace::Feed(b"$ ".to_vec()),
                MarkerTrace::Close(Some(zone_id)),
                MarkerTrace::Feed(b"ok\r\n".to_vec()),
                MarkerTrace::Begin(next_id),
                MarkerTrace::Finalize(zone_id),
            ]
        );
        assert_eq!(
            harness.backend.finalized_ids.borrow().as_slice(),
            &[zone_id],
            "finalize consumes the id allocated at A"
        );
        let finalized = harness.backend.finalized();
        let DispatchCall::Finalize(record) = &finalized[0] else {
            unreachable!();
        };
        assert_eq!(record.prompt, "$");
        assert_eq!(record.output_plain, "ok");
        assert!(!record.prompt.contains("block://"));
        assert!(!record.output_with_ansi.contains("block://"));
    }

    #[test]
    fn reset_splitter_preserves_capture_once_and_orders_hooks_before_exact_bytes() {
        let harness = ReaderHarness::new();
        harness.bstate.set(BlockState::CollectingOutput);
        harness.backend.take_calls();
        harness.backend.marker_trace.borrow_mut().clear();
        let bytes = b"before\x1b[3Jmiddle\x1bcafter";

        harness.feed_raw(bytes);

        assert_eq!(harness.live_output(), "\x1bcafter");
        assert_eq!(
            harness.backend.take_calls(),
            vec![
                DispatchCall::Feed(b"before".to_vec()),
                DispatchCall::EraseScrollback,
                DispatchCall::Feed(b"\x1b[3J".to_vec()),
                DispatchCall::Feed(b"middle".to_vec()),
                DispatchCall::HardReset,
                DispatchCall::ResetKittyPipeline,
                DispatchCall::Feed(b"\x1bc".to_vec()),
                DispatchCall::Feed(b"after".to_vec()),
            ]
        );
        assert_eq!(harness.ctx.engine.borrow().pending_zone, None);
    }

    #[test]
    fn reset_aborts_a_parser_owned_osc_and_fires_its_barrier_exactly_once() {
        // OSC stays parser-owned (Anvil needs core's real OSC events), so a
        // reset arriving mid-payload must be recovered by core's Osc/OscEsc
        // reprocess path: it emits the barrier event exactly once, ahead of
        // the exact reset bytes, and drops the aborted payload. The exact
        // call vectors below pin both halves — a double-fire or a dropped
        // reset would change them, and no Feed carries the aborted OSC's
        // title bytes to the terminal.
        for (bytes, expected_calls, expected_live) in [
            (
                b"\x1b]0;t\x1bc".as_slice(),
                vec![
                    DispatchCall::HardReset,
                    DispatchCall::ResetKittyPipeline,
                    DispatchCall::Feed(b"\x1bc".to_vec()),
                ],
                "\x1bc",
            ),
            (
                b"\x1b]0;t\x1b[3J".as_slice(),
                vec![
                    DispatchCall::EraseScrollback,
                    DispatchCall::Feed(b"\x1b[3J".to_vec()),
                ],
                "\x1b[3J",
            ),
        ] {
            let harness = ReaderHarness::new();
            harness.bstate.set(BlockState::CollectingOutput);
            harness.backend.take_calls();

            harness.feed_raw(bytes);

            assert_eq!(harness.backend.take_calls(), expected_calls, "{bytes:?}");
            assert_eq!(harness.live_output(), expected_live, "{bytes:?}");
        }
    }

    #[test]
    fn ris_resets_parser_modes_before_same_chunk_suffix_is_dispatched() {
        let harness = ReaderHarness::new();
        harness.bstate.set(BlockState::CollectingOutput);
        harness.backend.take_calls();

        harness.feed_raw(b"\x1b[?2004h\x1bc\x1b[?2004h");

        assert!(
            harness.pty.shell_bracketed_paste(),
            "the post-RIS DECSET must run on the fresh parser after reset"
        );
        assert_eq!(
            harness.backend.take_calls(),
            vec![
                DispatchCall::Feed(b"\x1b[?2004h".to_vec()),
                DispatchCall::HardReset,
                DispatchCall::ResetKittyPipeline,
                DispatchCall::Feed(b"\x1bc".to_vec()),
                DispatchCall::Feed(b"\x1b[?2004h".to_vec()),
            ]
        );
    }

    #[test]
    fn split_decset_and_decrst_update_owned_pty_through_parser_events() {
        let harness = ReaderHarness::new();
        harness.bstate.set(BlockState::CollectingOutput);

        harness.feed_raw(b"\x1b[?20");
        assert!(!harness.pty.shell_bracketed_paste());
        // This continuation contains no ESC, but the reset splitter is inside
        // CSI and must not take the borrowed Ground-state path.
        harness.feed_raw(b"04h");
        assert!(harness.pty.shell_bracketed_paste());

        harness.feed_raw(b"plain output without escapes");
        assert!(harness.pty.shell_bracketed_paste());

        harness.feed_raw(b"\x1b[?2004");
        assert!(harness.pty.shell_bracketed_paste());
        harness.feed_raw(b"l");
        assert!(!harness.pty.shell_bracketed_paste());
    }

    #[test]
    fn borrowed_splitter_path_still_advances_a_divergent_core_parser_state() {
        let harness = ReaderHarness::new();
        harness.bstate.set(BlockState::CollectingOutput);
        harness.backend.take_calls();

        // The local splitter aborts this malformed OSC on the repeated ESC and
        // returns to Ground. Core has consumed the same `ESC ESC` according to
        // its own recovery rule and is still in its Esc state, however.
        harness.feed_raw(b"\x1b]0;discarded-title\x1b");
        harness.feed_raw(b"\x1b");
        assert!(harness.backend.take_calls().is_empty());

        // This chunk has no ESC, so it takes the borrowed splitter fast path.
        // It must still pass through core: the pending Esc is emitted with the
        // backslash, followed by the plain suffix. Calling `on_bytes` directly
        // here would lose that ESC and strand core in the wrong state.
        harness.feed_raw(b"\\plain");
        assert_eq!(
            harness.backend.take_calls(),
            [DispatchCall::Feed(b"\x1b\\plain".to_vec())]
        );
        assert_eq!(harness.live_output(), "\x1b\\plain");
    }

    #[test]
    fn capability_observation_is_interleaved_with_same_chunk_ris() {
        let harness = ReaderHarness::new();
        let capability =
            b"\x1b]133;A\x07\x1b]7771;0123456789abcdef0123456789abcdef\x07\x1b]133;B\x07";

        let mut before = capability.to_vec();
        before.extend_from_slice(b"\x1bc");
        harness.feed_raw(&before);
        assert!(!harness.ctx.agent_execution_supported_rc.get());

        let mut after = b"\x1bc".to_vec();
        after.extend_from_slice(capability);
        harness.feed_raw(&after);
        assert!(harness.ctx.agent_execution_supported_rc.get());
    }

    #[test]
    fn ris_invalidates_unsettled_prompt_review_and_running_capture() {
        let harness = ReaderHarness::new();
        let execution = AgentExecutionRef {
            epoch: AgentSession::new(1, 2, 1).epoch(),
            generation: 19,
        };
        harness.bstate.set(BlockState::CollectingOutput);
        harness.prompt_ready.set(true);
        harness.ctx.agent_prompt_generation_rc.set(41);
        harness.ctx.agent_execution_supported_rc.set(true);
        harness.ctx.typed_cmd_rc.borrow_mut().push_str("typed");
        harness.ctx.idle_input_dirty_rc.set(true);
        harness.ctx.pty_synced_rc.set(true);
        harness.live_raw_output.borrow_mut().extend(b"live output");
        harness.ctx.running_cmd_rc.borrow_mut().push_str("running");
        harness.ctx.cmd_running_rc.set(true);
        harness
            .ctx
            .execution_id_rc
            .borrow_mut()
            .replace("exec".into());
        harness.ctx.active_agent_execution_rc.set(Some(execution));
        harness
            .ctx
            .armed_agent_execution_rc
            .borrow_mut()
            .replace(ArmedAgentExecution {
                execution,
                prompt_generation: 41,
            });
        harness
            .ctx
            .verified_submission
            .submission
            .borrow_mut()
            .replace(ReviewedSubmission {
                command: "reviewed".into(),
                execution: Some(execution),
                prompt_generation: 41,
                phase: ReviewedSubmissionPhase::Inserting,
                identity_feed_tainted: false,
            });
        {
            let mut engine = harness.ctx.engine.borrow_mut();
            engine.prompt_buf.push_str("prompt");
            engine.prompt_display.push_str("display");
            engine.vte_typed_cmd.push_str("vte command");
            engine.background_output.extend(b"background");
            engine.execution_id_trusted = true;
            engine.agent_completion_trusted = true;
        }

        harness.feed_raw(b"\x1bc");

        let engine = harness.ctx.engine.borrow();
        assert!(engine.prompt_buf.is_empty());
        assert!(engine.prompt_display.is_empty());
        assert!(engine.vte_typed_cmd.is_empty());
        assert!(engine.background_output.is_empty());
        assert!(!engine.execution_id_trusted);
        assert!(!engine.agent_completion_trusted);
        drop(engine);
        assert!(!harness.prompt_ready.get());
        assert_eq!(harness.ctx.agent_prompt_generation_rc.get(), 42);
        assert!(harness.ctx.typed_cmd_rc.borrow().is_empty());
        assert_eq!(harness.live_output(), "\x1bc");
        assert!(!harness.ctx.idle_input_dirty_rc.get());
        assert!(!harness.ctx.pty_synced_rc.get());
        assert!(!harness.ctx.cmd_running_rc.get());
        assert!(harness.ctx.running_cmd_rc.borrow().is_empty());
        assert!(harness.ctx.execution_id_rc.borrow().is_none());
        assert!(harness.ctx.active_agent_execution_rc.get().is_none());
        assert!(harness.ctx.armed_agent_execution_rc.borrow().is_none());
        assert!(harness
            .ctx
            .verified_submission
            .submission
            .borrow()
            .is_none());
        assert!(!harness.ctx.agent_execution_supported_rc.get());
    }

    #[test]
    fn explicit_reset_extent_latch_is_scoped_to_one_command() {
        let harness = ReaderHarness::new();
        harness.arm_verified_prompt();

        harness.feed(ParserEvent::EraseScrollback);
        assert!(harness.live_extent_force_full.get());

        // An accepted C starts a new command generation, so an ED3 observed
        // while the shell was idle cannot poison its live-card height.
        harness.feed(dispatch_command_start(Some("sleep 1")));
        assert!(!harness.live_extent_force_full.get());

        // A reset during this command is authoritative until the lifecycle's
        // accepted next prompt, including through finalization/reset_active.
        harness.feed(ParserEvent::EraseScrollback);
        assert!(harness.live_extent_force_full.get());
        harness.feed(dispatch_command_end(Some(0)));
        harness.feed(ParserEvent::PromptStart);
        assert!(!harness.live_extent_force_full.get());

        harness.feed(ParserEvent::HardReset);
        assert!(harness.live_extent_force_full.get());
    }

    #[test]
    fn bypassed_sos_payload_is_opaque_to_core_but_reaches_vte_once() {
        let harness = ReaderHarness::new();
        harness.bstate.set(BlockState::CollectingOutput);
        harness.backend.take_calls();
        let bytes = b"\x1bXsos \x07 [ ?2004h [6n [3J c\x1b\\";

        harness.feed_raw(bytes);

        assert!(!harness.pty.shell_bracketed_paste());
        assert!(harness.pty.drain_test_slave(PTY_REPLY_WAIT).is_empty());
        assert_eq!(
            harness.backend.take_calls(),
            [DispatchCall::Feed(bytes.to_vec())]
        );
        assert_eq!(harness.live_output().as_bytes(), bytes);
    }

    #[test]
    fn ris_precedes_same_chunk_query_and_alt_screen_events() {
        let harness = ReaderHarness::new();
        harness.bstate.set(BlockState::CollectingOutput);
        harness.backend.take_calls();

        harness.feed_raw(b"\x1bc\x1b[6n\x1b[?1049h");

        assert_eq!(harness.pty.drain_test_slave(PTY_REPLY_WAIT), b"\x1b[1;1R");
        assert_eq!(harness.bstate.get(), BlockState::AltScreen);
        assert_eq!(
            harness.backend.take_calls(),
            vec![
                DispatchCall::HardReset,
                DispatchCall::ResetKittyPipeline,
                DispatchCall::Feed(b"\x1bc".to_vec()),
                DispatchCall::Query(DispatchQuery::CursorPositionReport),
                DispatchCall::Feed(b"\x1b[6n".to_vec()),
                DispatchCall::EnterChrome,
                DispatchCall::EnterFullscreen,
                DispatchCall::SyncGeometry,
                DispatchCall::Feed(b"\x1b[?1049h".to_vec()),
            ],
            "reset hook must run before query replies and alt-screen side effects"
        );
    }

    #[test]
    fn ris_retires_open_zone_before_feed_and_keeps_completed_metadata() {
        let harness = ReaderHarness::new();
        harness.backend.set_metadata_only(true);
        drive_simple_command(&harness, "printf kept", "kept\r\n", None);
        assert_eq!(harness.backend.metadata_records.borrow().records.len(), 1);
        assert!(harness.ctx.engine.borrow().pending_zone.is_some());
        harness.backend.take_calls();
        harness.backend.marker_trace.borrow_mut().clear();

        harness.feed_raw(b"\x1bc");

        assert_eq!(harness.ctx.engine.borrow().pending_zone, None);
        {
            // Records and their snapshots both survive RIS: a snapshot comes
            // from the byte stream, not the surface, so a reset cannot
            // resurrect erased content through it.
            let store = harness.backend.metadata_records.borrow();
            assert_eq!(store.records.len(), 1);
            let id = store.records[0].id;
            assert_eq!(
                store.snapshot(id).map(|snapshot| snapshot.plain.as_str()),
                Some("kept")
            );
        }
        assert_eq!(
            harness.backend.marker_trace.borrow().as_slice(),
            &[MarkerTrace::HardReset, MarkerTrace::Feed(b"\x1bc".to_vec())],
        );
    }

    #[test]
    fn repeated_idle_a_reuses_its_zone_and_completed_a_rotates_once() {
        let harness = ReaderHarness::new();
        harness.feed(ParserEvent::PromptStart);
        let first_id = harness.ctx.engine.borrow().pending_zone.unwrap().id();
        harness.feed(ParserEvent::PromptEnd);
        harness.feed(ParserEvent::PromptStart);
        assert_eq!(
            harness.ctx.engine.borrow().pending_zone.unwrap().id(),
            first_id,
            "an idle prompt redraw reuses its globally allocated id"
        );

        harness.feed(ParserEvent::PromptEnd);
        harness.backend.render_row(0, "echo ok");
        harness.feed(dispatch_command_start(Some("echo ok")));
        harness.feed(dispatch_command_end(Some(0)));
        harness.feed(ParserEvent::PromptStart);
        assert_eq!(
            harness.backend.finalized_ids.borrow().as_slice(),
            &[first_id]
        );
        assert_ne!(
            harness.ctx.engine.borrow().pending_zone.unwrap().id(),
            first_id
        );
    }

    #[test]
    fn empty_nested_and_out_of_state_marks_do_not_mint_or_close_extra_zones() {
        let harness = ReaderHarness::new();
        harness.feed(dispatch_command_start(Some("untrusted-before-a")));
        assert!(harness.ctx.engine.borrow().pending_zone.is_none());
        assert!(harness.backend.marker_trace.borrow().is_empty());

        harness.feed_all([ParserEvent::PromptStart, ParserEvent::PromptEnd]);
        let zone_id = harness.ctx.engine.borrow().pending_zone.unwrap().id();
        harness.feed(dispatch_command_start(None));
        harness.feed(dispatch_command_start(Some("nested")));
        harness.feed(ParserEvent::PromptStart);
        harness.feed(dispatch_command_end(Some(9)));
        harness.feed(dispatch_command_end(Some(0)));
        harness.feed(ParserEvent::PromptStart);

        assert!(harness.backend.finalized_ids.borrow().is_empty());
        assert_ne!(
            harness.ctx.engine.borrow().pending_zone.unwrap().id(),
            zone_id,
            "the empty lifecycle consumes its A identity without fabricating a record"
        );
        let trace = harness.backend.marker_trace.borrow();
        assert_eq!(
            trace
                .iter()
                .filter(|event| **event == MarkerTrace::Close(Some(zone_id)))
                .count(),
            1,
            "only the accepted outer C closes the zone"
        );
        assert!(!trace
            .iter()
            .any(|event| matches!(event, MarkerTrace::Finalize(_))));
    }

    #[test]
    fn background_record_consumes_the_open_idle_prompt_zone() {
        let harness = ReaderHarness::new();
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("$ "),
            ParserEvent::PromptEnd,
        ]);
        let prompt_id = harness.ctx.engine.borrow().pending_zone.unwrap().id();
        harness.feed(dispatch_bytes("background\r\n"));
        harness.feed(ParserEvent::PromptStart);
        assert_eq!(
            harness.backend.finalized_ids.borrow().as_slice(),
            &[prompt_id]
        );
        let finalized = harness.backend.finalized();
        let DispatchCall::Finalize(record) = &finalized[0] else {
            unreachable!();
        };
        assert!(record.is_background);
        assert_eq!(record.output_plain, "background");
    }

    #[test]
    fn alt_leave_reopens_only_the_pre_c_prompt_zone_after_rmcup() {
        let harness = ReaderHarness::new();
        harness.feed_all([ParserEvent::PromptStart, ParserEvent::PromptEnd]);
        let zone_id = harness.ctx.engine.borrow().pending_zone.unwrap().id();
        harness.backend.marker_trace.borrow_mut().clear();
        harness.feed_all([
            ParserEvent::AltScreenEnter(1049),
            ParserEvent::AltScreenLeave(1049),
        ]);
        assert_eq!(
            harness.backend.marker_trace.borrow().as_slice(),
            &[
                MarkerTrace::Feed(b"\x1b[?1049h".to_vec()),
                MarkerTrace::Feed(b"\x1b[?1049l".to_vec()),
                MarkerTrace::Begin(zone_id),
            ]
        );

        harness.feed(dispatch_command_start(None));
        harness.backend.marker_trace.borrow_mut().clear();
        harness.feed_all([
            ParserEvent::AltScreenEnter(1049),
            ParserEvent::AltScreenLeave(1049),
        ]);
        assert!(!harness
            .backend
            .marker_trace
            .borrow()
            .iter()
            .any(|event| matches!(event, MarkerTrace::Begin(_))));
    }

    #[test]
    fn selection_hold_replay_uses_the_same_marker_feed_wrapper() {
        let marker = Rc::new(RefCell::new(ZoneMarkerInjector::with_nonce([0x11; 16])));
        marker.borrow_mut().begin_zone(5);
        let fed = Rc::new(RefCell::new(Vec::new()));
        let hold = SelectionFeedHold::new();
        hold.set_flush({
            let marker = marker.clone();
            let fed = fed.clone();
            move |bytes| {
                feed_with_zone_marker(&marker, &bytes, |part| fed.borrow_mut().push(part.to_vec()));
            }
        });
        hold.begin_drag();
        assert!(hold.try_buffer(b"parked shell bytes"));
        hold.flush_now();
        let open = marker.borrow().open_bytes().unwrap().to_vec();
        assert_eq!(
            fed.borrow().as_slice(),
            &[open, b"parked shell bytes".to_vec()]
        );
    }

    #[test]
    fn metadata_finalize_and_metadata_observers_never_materialize_output() {
        let harness = ReaderHarness::new();
        harness.backend.set_metadata_only(true);
        harness.ctx.block_finished_cbs.borrow_mut().clear();
        let observed = Rc::new(RefCell::new(Vec::new()));
        {
            let observed = observed.clone();
            harness
                .ctx
                .block_finished_cbs
                .borrow_mut()
                .push(BlockFinishedCallback::Metadata(Box::new(
                    move |command, exit_code, agent, duration| {
                        observed
                            .borrow_mut()
                            .push((command, exit_code, agent, duration));
                    },
                )));
        }

        drive_simple_command(&harness, "printf hi", "\x1b[32mhi\x1b[0m\r\n", None);

        assert_eq!(harness.backend.payload_counters.borrow().len(), 1);
        assert_eq!(harness.backend.payload_counters.borrow()[0].get(), 0);
        assert_eq!(observed.borrow().len(), 1);
        let records = harness.backend.records();
        let BackendRecords::Metadata(store) = records else {
            panic!("metadata backend must not manufacture BlockData");
        };
        assert_eq!(store.records.len(), 1);
        assert_eq!(store.records[0].cmd, "printf hi");
        // The bounded snapshot is captured without materializing: plain text
        // only, complete, and sourced from pre-injector PTY bytes.
        let snapshot = store
            .snapshot(store.records[0].id)
            .expect("finalize retains a bounded output snapshot");
        assert_eq!(snapshot.plain, "hi");
        assert!(!snapshot.truncated);
        drop(store);
        assert!(harness.live_output().is_empty());
    }

    /// Pin for the snapshot provenance invariant, driven through the real
    /// reader: zone-marker OSC 8 frames are a backend-side surface feed, while
    /// engine capture takes the raw PTY chunk, so no injected byte can reach
    /// the snapshot finalize stores.
    #[test]
    fn zone_marker_bytes_never_enter_the_snapshot_capture() {
        let harness = ReaderHarness::new();
        harness.backend.set_metadata_only(true);
        drive_simple_command(&harness, "printf hi", "real output\r\n", None);

        let live_feed = String::from_utf8(harness.backend.live_feed.borrow().clone())
            .expect("marker bytes are UTF-8");
        assert!(
            live_feed.contains("block://"),
            "the live surface feed IS marked"
        );
        assert!(live_feed.contains("real output"));

        let records = harness.backend.records();
        let BackendRecords::Metadata(store) = records else {
            panic!("the metadata-only backend records zones");
        };
        let snapshot = store
            .snapshot(store.records[0].id)
            .expect("finalize retains a bounded output snapshot");
        assert_eq!(snapshot.plain, "real output");
        assert!(!snapshot.plain.contains("block://"));
        assert!(!snapshot.plain.contains('\x1b'));
    }

    /// The jsh path, end to end: the journal submission materializes the
    /// payload before finalize, and `\r`-repainted progress collapses the
    /// retained window to a couple of lines — so neither the tail bound nor
    /// the text itself can show that the raw-output bound already discarded
    /// megabytes. Only the marker carried from the append site can, and it
    /// resets with the ring so the next command is not mislabelled.
    #[test]
    fn wrapped_raw_output_marks_the_snapshot_truncated_without_leaking_forward() {
        let harness = ReaderHarness::new();
        harness.backend.set_metadata_only(true);
        let repaints = MAX_RAW_OUTPUT_BYTES / 20 + 1;
        let output = format!("{}\r\ndone\r\n", "\rprogress 1234567.89".repeat(repaints));
        assert!(output.len() > MAX_RAW_OUTPUT_BYTES);

        harness.backend.render_row(3, "$ ");
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("$ "),
            ParserEvent::PromptEnd,
        ]);
        harness.backend.render_row(3, "$ cargo build");
        harness.feed(dispatch_command_start(Some("cargo build")));
        harness.feed(dispatch_bytes(&output));
        harness.feed(dispatch_command_end(Some(0)));
        // A trusted jsh correlation id makes the journal submission — which
        // runs before backend finalize — consume the ring.
        *harness.ctx.execution_id_rc.borrow_mut() = Some("execution-1".to_string());
        harness.feed(ParserEvent::PromptStart);

        assert_eq!(
            harness.backend.payload_counters.borrow()[0].get(),
            1,
            "the journal materialized the payload before finalize read the ring"
        );
        {
            let store = harness.backend.metadata_records.borrow();
            let snapshot = store
                .snapshot(store.records[0].id)
                .expect("a wrapped capture still retains its collapsed tail");
            assert!(snapshot.plain.ends_with("done"));
            assert!(
                snapshot.plain.len() < MAX_ZONE_SNAPSHOT_BYTES,
                "the repaints collapsed well inside the tail bound: {}",
                snapshot.plain.len()
            );
            assert!(
                snapshot.truncated,
                "bytes the raw-output bound discarded are truncation"
            );
        }

        drive_simple_command(&harness, "printf short", "short\r\n", None);
        let store = harness.backend.metadata_records.borrow();
        let snapshot = store
            .snapshot(store.records[1].id)
            .expect("the next command retains its own snapshot");
        assert_eq!(snapshot.plain, "short");
        assert!(
            !snapshot.truncated,
            "the drop marker is cleared with the ring it described"
        );
    }

    #[test]
    fn metadata_bridge_materializes_only_a_correlated_agent_completion() {
        let harness = ReaderHarness::new();
        harness.backend.set_metadata_only(true);
        harness.ctx.block_finished_cbs.borrow_mut().clear();
        let samples = Rc::new(RefCell::new(Vec::new()));
        {
            let samples = samples.clone();
            harness.ctx.block_finished_cbs.borrow_mut().push(
                BlockFinishedCallback::ConditionalOutput {
                    needs_output: Box::new(|agent| agent.is_some()),
                    callback: Box::new(move |_, _, sample, _, _| {
                        samples.borrow_mut().push(sample);
                    }),
                },
            );
        }

        drive_simple_command(&harness, "printf ordinary", "ordinary output\r\n", None);
        assert_eq!(harness.backend.payload_counters.borrow()[0].get(), 0);
        assert_eq!(samples.borrow().as_slice(), &[None]);

        let execution = AgentExecutionRef {
            epoch: AgentSession::new(1, 2, 1).epoch(),
            generation: 7,
        };
        drive_simple_command(
            &harness,
            "printf agent",
            "agent output\r\n",
            Some(execution),
        );
        assert_eq!(harness.backend.payload_counters.borrow()[1].get(), 1);
        assert_eq!(
            samples.borrow()[1].as_deref(),
            Some("agent output\n"),
            "the Agent bridge gets the bounded plain sample"
        );
    }

    #[test]
    fn metadata_background_finalize_skips_command_output_observers() {
        let harness = ReaderHarness::new();
        harness.backend.set_metadata_only(true);
        harness.ctx.block_finished_cbs.borrow_mut().clear();
        let observer_calls = Rc::new(Cell::new(0usize));
        {
            let observer_calls = observer_calls.clone();
            harness.ctx.block_finished_cbs.borrow_mut().push(
                BlockFinishedCallback::ConditionalOutput {
                    needs_output: Box::new(|_| true),
                    callback: Box::new(move |_, _, _, _, _| {
                        observer_calls.set(observer_calls.get() + 1);
                    }),
                },
            );
        }
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("$ "),
            ParserEvent::PromptEnd,
            dispatch_bytes("async metadata only\r\n"),
            ParserEvent::PromptStart,
        ]);

        assert_eq!(observer_calls.get(), 0);
        assert_eq!(harness.backend.payload_counters.borrow().len(), 1);
        assert_eq!(harness.backend.payload_counters.borrow()[0].get(), 0);
        let records = harness.backend.records();
        let BackendRecords::Metadata(store) = records else {
            panic!("metadata backend must not manufacture a BlockData");
        };
        assert_eq!(store.records.len(), 1);
        assert!(store.records[0].is_background);
    }

    #[test]
    fn journal_capability_matches_pinned_jterm_core_enabled_semantics() {
        use super::execution_journal_output_capture_enabled_for as enabled;

        for disabled in ["", "0", "false", "no", "off", " FALSE ", "No"] {
            assert!(!enabled(Some(disabled)), "{disabled:?}");
        }
        for enabled_value in ["1", "true", "yes", "on", "anything"] {
            assert!(enabled(Some(enabled_value)), "{enabled_value:?}");
        }
        assert!(enabled(None), "missing/non-Unicode values default enabled");
    }

    #[test]
    fn journal_id_and_capability_gate_lazy_output_materialization() {
        let disabled = super::LazyBlockRenderPayload::new(
            "$ ".to_string(),
            super::CapturedFinalizeOutput::Background(b"disabled\r\n".iter().copied().collect()),
            false,
        );
        assert!(super::build_journal_completion(
            Some("execution-disabled".to_string()),
            false,
            &disabled,
            100,
        )
        .is_none());
        assert_eq!(disabled.materialization_count(), 0);

        let missing_id = super::LazyBlockRenderPayload::new(
            "$ ".to_string(),
            super::CapturedFinalizeOutput::Background(b"missing id\r\n".iter().copied().collect()),
            false,
        );
        assert!(super::build_journal_completion(None, true, &missing_id, 100).is_none());
        assert_eq!(missing_id.materialization_count(), 0);

        let enabled = super::LazyBlockRenderPayload::new(
            "$ ".to_string(),
            super::CapturedFinalizeOutput::Background(b"enabled\r\n".iter().copied().collect()),
            false,
        );
        let completion = super::build_journal_completion(
            Some("execution-enabled".to_string()),
            true,
            &enabled,
            100,
        )
        .expect("enabled correlated output is consumed");
        assert_eq!(enabled.materialization_count(), 1);
        assert_eq!(completion.output, "enabled");
    }

    #[test]
    fn bounded_vte_capture_limits_each_call_and_total_work() {
        let spans = Rc::new(RefCell::new(Vec::new()));
        let seen = spans.clone();
        let captured = super::capture_vte_rows_bounded(
            -10_000,
            10_000,
            80,
            73,
            || false,
            move |span| {
                seen.borrow_mut().push(span);
                Some("雪".repeat(span.work_cells))
            },
        );

        assert!(captured.incomplete);
        assert!(captured.text.len() <= 73);
        let spans = spans.borrow();
        assert!(spans.iter().all(|span| span.start_row == span.end_row));
        assert!(spans
            .iter()
            .all(|span| usize::try_from(span.end_col).ok() == Some(span.work_cells)));
        assert!(spans.iter().map(|span| span.work_cells).sum::<usize>() <= 73);
        assert!(spans.len() < 20_001, "the whole ring was not requested");

        let calls = Cell::new(0usize);
        let timed_out = super::capture_vte_rows_bounded(
            0,
            999,
            80,
            4 * 1024 * 1024,
            || true,
            |_| {
                calls.set(calls.get() + 1);
                Some(String::new())
            },
        );
        assert!(timed_out.incomplete);
        assert!(timed_out.text.is_empty());
        assert_eq!(calls.get(), 0, "deadline is checked before a VTE call");
    }

    #[test]
    fn unified_search_extracts_viewport_before_huge_old_history() {
        let mut requested_rows = Vec::new();
        let captured = super::capture_vte_search_windows_bounded(
            -100_000,
            4,
            0,
            80,
            400,
            || false,
            |span| {
                requested_rows.push(span.start_row);
                Some(String::new())
            },
        );

        assert_eq!(requested_rows[..4], [0, 1, 2, 3]);
        assert_eq!(requested_rows[4], -100_000);
        assert!(!captured.viewport_to_tail.incomplete);
        assert_eq!(captured.viewport_to_tail.work_cells, 320);
        let history = captured.oldest_history.expect("remaining bounded history");
        assert!(history.incomplete);
        assert_eq!(history.work_cells, 80);
        assert_eq!(
            captured.viewport_to_tail.work_cells + history.work_cells,
            400,
            "both windows share one hard extraction budget"
        );
    }

    #[test]
    fn bounded_vte_capture_charges_each_half_open_row_exactly() {
        let spans = Rc::new(RefCell::new(Vec::new()));
        let seen = spans.clone();
        let captured = super::capture_vte_rows_bounded(
            5,
            99,
            10,
            25,
            || false,
            move |span| {
                seen.borrow_mut().push(span);
                Some(String::new())
            },
        );
        assert_eq!(
            spans.borrow().as_slice(),
            &[
                super::VteCaptureSpan {
                    start_row: 5,
                    end_row: 5,
                    end_col: 10,
                    work_cells: 10,
                },
                super::VteCaptureSpan {
                    start_row: 6,
                    end_row: 6,
                    end_col: 10,
                    work_cells: 10,
                },
                super::VteCaptureSpan {
                    start_row: 7,
                    end_row: 7,
                    end_col: 5,
                    work_cells: 5,
                },
            ]
        );
        assert!(captured.incomplete);
    }

    #[test]
    fn completed_surface_materialization_checks_deadline_on_both_sides() {
        let materialized = Cell::new(0usize);
        let mut surfaces = Vec::new();
        let mut already_expired = || true;
        assert!(!super::push_search_surface_before_deadline(
            &mut surfaces,
            &mut already_expired,
            || {
                materialized.set(materialized.get() + 1);
                1usize
            },
        ));
        assert!(surfaces.is_empty());
        assert_eq!(materialized.get(), 0);

        let checks = Cell::new(0usize);
        let mut expires_after_surface = || {
            checks.set(checks.get() + 1);
            checks.get() >= 2
        };
        assert!(!super::push_search_surface_before_deadline(
            &mut surfaces,
            &mut expires_after_surface,
            || {
                materialized.set(materialized.get() + 1);
                2usize
            },
        ));
        assert_eq!(surfaces, vec![2]);
        assert_eq!(materialized.get(), 1);
    }

    /// The dock must never reparent a widget another region still owns: GTK
    /// would warn and the card would end up in neither place.
    #[test]
    #[ignore = "requires DISPLAY"]
    fn dock_mount_refuses_a_widget_another_region_owns() {
        use relm4::gtk;
        use relm4::gtk::prelude::*;

        gtk::init().expect("gtk init");
        let dock = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let elsewhere = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let dock_widget: &gtk::Widget = dock.upcast_ref();

        let card = gtk::Label::new(Some("card"));
        let card_widget: gtk::Widget = card.clone().upcast();
        assert_eq!(
            super::dock_mount_decision(card_widget.parent().as_ref(), dock_widget),
            super::DockMount::Append,
            "an unparented card is taken by the dock"
        );

        dock.append(&card);
        assert_eq!(
            super::dock_mount_decision(card_widget.parent().as_ref(), dock_widget),
            super::DockMount::Keep,
            "a docked card stays where it is; the dock is already beside the prompt"
        );

        dock.remove(&card);
        elsewhere.append(&card);
        assert_eq!(
            super::dock_mount_decision(card_widget.parent().as_ref(), dock_widget),
            super::DockMount::Refuse,
            "a card the scrolling document owns is refused, not stolen"
        );
    }

    #[test]
    #[ignore = "requires DISPLAY"]
    fn bounded_vte_capture_real_vte_uses_half_open_column_boundary() {
        use relm4::gtk;
        use relm4::gtk::prelude::*;
        use std::time::Duration;
        use vte4::TerminalExt;

        gtk::init().expect("gtk init");
        let terminal = vte4::Terminal::new();
        terminal.set_size(5, 2);
        let window = gtk::Window::new();
        window.set_child(Some(&terminal));
        window.present();
        terminal.feed(b"ABCDE");
        let context = gtk::glib::MainContext::default();
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(100) {
            while context.iteration(false) {}
            std::thread::sleep(Duration::from_millis(2));
        }

        let spans = Rc::new(RefCell::new(Vec::new()));
        let seen = spans.clone();
        let captured = super::capture_vte_rows_bounded(
            0,
            0,
            5,
            4,
            || false,
            move |span| {
                seen.borrow_mut().push(span);
                super::capture_vte_text_range(
                    &terminal,
                    span.start_row,
                    0,
                    span.end_row,
                    span.end_col,
                )
            },
        );
        assert_eq!(spans.borrow().len(), 1);
        assert_eq!(spans.borrow()[0].end_col, 4);
        assert_eq!(spans.borrow()[0].work_cells, 4);
        assert_eq!(captured.text, "ABCD");
        assert!(captured.incomplete);
        window.close();
        while context.iteration(false) {}
    }

    #[test]
    fn reader_dispatch_full_cycle_finalizes_content_and_orders_fan_outs() {
        let harness = ReaderHarness::new();
        harness.backend.set_columns(100, 120);
        harness.backend.render_row(3, "user@host $ ");
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("top-line\r\nuser@host $ "),
            ParserEvent::PromptEnd,
        ]);
        assert_eq!(harness.prompt_end.get(), (12, 3));
        assert_eq!(harness.prompt_rows.get(), 24);

        harness.backend.render_row(3, "user@host $ echo hi");
        harness.backend.take_calls();
        harness.feed(dispatch_command_start_in(None, SHELL_REPORTED_CWD));

        assert_eq!(harness.bstate.get(), BlockState::CollectingOutput);
        assert_eq!(
            harness.backend.take_calls(),
            vec![
                DispatchCall::Query(DispatchQuery::CursorAndRows),
                DispatchCall::Query(DispatchQuery::CommandCaptureAnchor {
                    provisional: (12, 3),
                    recorded_rows: 24,
                }),
                DispatchCall::Query(DispatchQuery::CaptureTextRange {
                    start: (3, 12),
                    end: (3, 19),
                }),
                DispatchCall::SyncGeometry,
                DispatchCall::MarkDirty,
            ]
        );
        assert_eq!(
            harness.commands_started.borrow().as_slice(),
            &[StartedFanOut {
                event: CommandStartedEvent {
                    command: "echo hi".to_string(),
                    cwd: Some(SHELL_REPORTED_CWD.to_string()),
                },
                cmd_running: true,
                state_at_fan_out: BlockState::CollectingOutput,
                geometry_pushes_before: 0,
            }]
        );

        let wide_line = "w".repeat(110);
        let raw_output = format!("\x1b[32mhi\x1b[0m\r\n{wide_line}\r\n");
        let plain_output = format!("hi\n{wide_line}\n");
        harness.feed(dispatch_bytes(&raw_output));
        assert_eq!(harness.live_output(), raw_output);
        harness.feed(dispatch_command_end(Some(0)));
        assert_eq!(harness.bstate.get(), BlockState::PostCommand);

        let before_finalize = harness.backend.take_calls();
        assert!(before_finalize.contains(&DispatchCall::Feed(raw_output.as_bytes().to_vec())));
        assert!(!before_finalize
            .iter()
            .any(|call| matches!(call, DispatchCall::Finalize(_))));

        let expected_height =
            estimated_finished_block_height_for_text(&harness.config.borrow(), &plain_output, 100);
        let wrong_height =
            estimated_finished_block_height_for_text(&harness.config.borrow(), &plain_output, 120);
        assert_ne!(expected_height, wrong_height);

        harness.feed(ParserEvent::PromptStart);
        assert_eq!(
            harness.backend.calls(),
            vec![
                DispatchCall::Finalize(FinalizeRecord {
                    prompt: "user@host $".to_string(),
                    command: "echo hi".to_string(),
                    output_with_ansi: raw_output,
                    output_plain: plain_output.trim().to_string(),
                    plain_output_bytes: plain_output.len(),
                    cwd: Some(SHELL_REPORTED_CWD.to_string()),
                    cols: 100,
                    estimated_height: expected_height,
                    exit_code: Some(0),
                    has_duration: true,
                    has_end_time: true,
                    is_background: false,
                }),
                DispatchCall::SyncGeometry,
                DispatchCall::MarkDirty,
            ]
        );
        assert_eq!(harness.backend.finalized_ids.borrow().len(), 1);
        assert_eq!(harness.backend.payload_counters.borrow()[0].get(), 1);
        assert!(harness.live_output().is_empty());
        assert_eq!(harness.bstate.get(), BlockState::CollectingPrompt);
        assert_eq!(
            harness.blocks_finished.borrow().as_slice(),
            &[FinishedFanOut {
                command: "echo hi".to_string(),
                exit_code: Some(0),
                output_sample: plain_output,
                agent_execution: None,
                blocks_finalized_before: 1,
                state_at_fan_out: BlockState::PostCommand,
            }]
        );
        assert_eq!(harness.commands_finished.borrow().len(), 1);
    }

    #[test]
    fn reader_dispatch_second_cycle_keeps_its_unreported_status() {
        let harness = ReaderHarness::new();
        harness.backend.render_row(3, "$ ");
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("$ "),
            ParserEvent::PromptEnd,
        ]);
        harness.backend.render_row(3, "$ first");
        harness.feed(dispatch_command_start(None));
        harness.feed(dispatch_bytes("one\r\n"));
        harness.feed(dispatch_command_end(Some(0)));
        harness.feed(ParserEvent::PromptStart);

        harness.backend.render_row(3, "$ ");
        harness.feed_all([dispatch_bytes("$ "), ParserEvent::PromptEnd]);
        harness.backend.render_row(3, "$ second");
        harness.feed(dispatch_command_start(None));
        harness.feed(dispatch_bytes("two\r\n"));
        harness.feed(dispatch_command_end(None));
        harness.feed(ParserEvent::PromptStart);

        let finalized = harness.backend.finalized();
        assert_eq!(finalized.len(), 2);
        let DispatchCall::Finalize(second) = &finalized[1] else {
            panic!("the second record must be a finalize: {finalized:?}");
        };
        assert_eq!(second.command, "second");
        assert_eq!(second.output_plain, "two");
        assert_eq!(second.exit_code, None);
        assert!(second.has_duration);
        assert_eq!(
            harness
                .commands_finished
                .borrow()
                .iter()
                .map(|event| event.exit_code)
                .collect::<Vec<_>>(),
            vec![Some(0), None]
        );
    }

    #[test]
    fn reader_dispatch_capture_uses_live_columns_for_the_size_guard() {
        assert!(command_capture_range_is_bounded(0, 2499, 100));
        assert!(!command_capture_range_is_bounded(0, 2499, 120));
        let harness = ReaderHarness::new();
        harness.backend.set_row_count(2600);
        harness.backend.set_columns(100, 120);
        harness.backend.render_row(0, "");
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("$ "),
            ParserEvent::PromptEnd,
        ]);
        harness.backend.render_row(2499, "wrapped-tail");
        harness.backend.take_calls();

        harness.feed(dispatch_command_start(None));
        assert!(!harness.backend.take_calls().iter().any(|call| matches!(
            call,
            DispatchCall::Query(DispatchQuery::CaptureTextRange { .. })
        )));
        assert_eq!(
            harness.commands_started.borrow()[0].event.command,
            TRUNCATED_COMMAND_PLACEHOLDER
        );
    }

    #[test]
    fn reader_dispatch_rebases_the_saved_anchor_before_capture() {
        let harness = ReaderHarness::new();
        harness.backend.render_row(3, "$ ");
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("$ "),
            ParserEvent::PromptEnd,
        ]);
        harness.backend.set_row_count(28);
        harness.backend.render_row(7, "$ echo hi");
        harness.backend.take_calls();

        harness.feed(dispatch_command_start(None));
        assert_eq!(
            harness.backend.take_calls(),
            vec![
                DispatchCall::Query(DispatchQuery::CursorAndRows),
                DispatchCall::Query(DispatchQuery::CommandCaptureAnchor {
                    provisional: (2, 3),
                    recorded_rows: 24,
                }),
                DispatchCall::Query(DispatchQuery::CaptureTextRange {
                    start: (7, 2),
                    end: (7, 9),
                }),
                DispatchCall::SyncGeometry,
                DispatchCall::MarkDirty,
            ]
        );
        assert_eq!(
            harness.commands_started.borrow()[0].event.command,
            "echo hi"
        );
        assert_eq!(harness.prompt_end.get(), (2, 7));
        assert_eq!(harness.prompt_rows.get(), 28);
    }

    #[test]
    fn reader_dispatch_unsettled_anchor_is_never_read_for_command_text() {
        let harness = ReaderHarness::new();
        harness.backend.settle_anchor_now.set(false);
        harness.backend.render_row(3, "user@host $ ");
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("user@host $ "),
            ParserEvent::PromptEnd,
        ]);
        assert!(!harness.prompt_ready.get());
        harness.backend.render_row(3, "user@host $ echo hi");
        harness.backend.take_calls();
        harness.feed(dispatch_command_start(None));

        let calls = harness.backend.take_calls();
        assert!(calls.contains(&DispatchCall::Query(DispatchQuery::CursorAndRows)));
        assert!(calls.iter().any(|call| matches!(
            call,
            DispatchCall::Query(DispatchQuery::CommandCaptureAnchor { .. })
        )));
        assert!(!calls.iter().any(|call| matches!(
            call,
            DispatchCall::Query(DispatchQuery::CaptureTextRange { .. })
        )));
        assert_eq!(harness.commands_started.borrow()[0].event.command, "");
    }

    #[test]
    fn reader_dispatch_empty_command_resets_without_finalize_or_geometry() {
        let harness = ReaderHarness::new();
        harness.config.borrow_mut().preserve_live_scrollback = true;
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("user@host $ "),
            ParserEvent::PromptEnd,
            dispatch_command_start(None),
            dispatch_command_end(Some(0)),
        ]);
        harness.backend.take_calls();

        harness.feed(ParserEvent::PromptStart);
        assert_eq!(
            harness.backend.take_calls(),
            vec![
                DispatchCall::ResetActiveSurface {
                    preserve_scrollback: true,
                },
                DispatchCall::ResetKittyPipeline,
                DispatchCall::MarkDirty,
            ]
        );
        assert!(harness.blocks_finished.borrow().is_empty());
        assert_eq!(harness.bstate.get(), BlockState::CollectingPrompt);
    }

    #[test]
    fn missing_command_with_visible_output_preserves_metadata_without_materializing() {
        let harness = ReaderHarness::new();
        harness.backend.set_metadata_only(true);
        harness.ctx.block_finished_cbs.borrow_mut().clear();
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("user@host $ "),
            ParserEvent::PromptEnd,
            dispatch_command_start(None),
            dispatch_bytes("the command really ran\r\n"),
            dispatch_command_end(Some(0)),
            ParserEvent::PromptStart,
        ]);

        assert_eq!(harness.backend.payload_counters.borrow().len(), 1);
        assert_eq!(harness.backend.payload_counters.borrow()[0].get(), 0);
        let records = harness.backend.records();
        let BackendRecords::Metadata(store) = records else {
            panic!("metadata backend must not manufacture a BlockData");
        };
        assert_eq!(store.records.len(), 1);
        assert_eq!(store.records[0].cmd, UNAVAILABLE_COMMAND_PLACEHOLDER);
        assert_eq!(store.records[0].exit_code, Some(0));
        drop(store);
        assert!(harness.live_output().is_empty());
    }

    #[test]
    fn reader_dispatch_background_output_becomes_a_commandless_block() {
        let harness = ReaderHarness::new();
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("user@host $ "),
            ParserEvent::PromptEnd,
        ]);
        harness.backend.take_calls();
        harness.feed(dispatch_bytes("cron: backup done\r\n"));
        assert_eq!(
            harness.backend.take_calls(),
            vec![
                DispatchCall::MarkDirty,
                DispatchCall::Feed(b"cron: backup done\r\n".to_vec()),
            ]
        );

        harness.feed(ParserEvent::PromptStart);
        let expected_height = estimated_finished_block_height_for_text(
            &harness.config.borrow(),
            "cron: backup done\n",
            80,
        );
        assert_eq!(
            harness.backend.calls(),
            vec![
                DispatchCall::Finalize(FinalizeRecord {
                    prompt: String::new(),
                    command: String::new(),
                    output_with_ansi: "cron: backup done\r\n".to_string(),
                    output_plain: "cron: backup done".to_string(),
                    plain_output_bytes: 18,
                    cwd: Some(HARNESS_CWD.to_string()),
                    cols: 80,
                    estimated_height: expected_height,
                    exit_code: None,
                    has_duration: false,
                    has_end_time: true,
                    is_background: true,
                }),
                DispatchCall::SyncGeometry,
                DispatchCall::MarkDirty,
            ]
        );
        assert!(harness.blocks_finished.borrow().is_empty());
        assert!(harness.commands_started.borrow().is_empty());
    }

    #[test]
    fn reader_dispatch_dirty_prompt_output_stays_inline() {
        let harness = ReaderHarness::new();
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("user@host $ "),
            ParserEvent::PromptEnd,
        ]);
        harness.ctx.idle_input_dirty_rc.set(true);
        harness.backend.take_calls();
        harness.feed(dispatch_bytes("ec\r\n"));
        assert_eq!(
            harness.backend.take_calls(),
            vec![
                DispatchCall::MarkDirty,
                DispatchCall::Feed(b"ec\r\n".to_vec()),
            ]
        );
        assert!(harness.ctx.engine.borrow().background_output.is_empty());
        harness.feed(ParserEvent::PromptStart);
        assert!(harness.backend.finalized().is_empty());
    }

    #[test]
    fn reader_dispatch_command_start_drops_buffered_background_output() {
        let harness = ReaderHarness::new();
        harness.backend.render_row(3, "user@host $ ");
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("user@host $ "),
            ParserEvent::PromptEnd,
        ]);
        harness.feed(dispatch_bytes("cron: backup done\r\n"));
        assert!(!harness.ctx.engine.borrow().background_output.is_empty());
        harness.backend.render_row(3, "user@host $ echo hi");
        harness.feed(dispatch_command_start(None));
        assert!(harness.ctx.engine.borrow().background_output.is_empty());
        harness.feed(dispatch_bytes("hi\r\n"));
        harness.feed(dispatch_command_end(Some(0)));
        harness.feed(ParserEvent::PromptStart);
        let finalized = harness.backend.finalized();
        assert_eq!(finalized.len(), 1);
        let DispatchCall::Finalize(block) = &finalized[0] else {
            unreachable!();
        };
        assert_eq!(block.command, "echo hi");
        assert_eq!(block.output_plain, "hi");
        assert!(!block.is_background);
    }

    #[test]
    fn reader_dispatch_alt_screen_preserves_both_entry_states() {
        let harness = ReaderHarness::new();
        harness.backend.render_row(3, "user@host $ ");
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("user@host $ "),
            ParserEvent::PromptEnd,
        ]);
        harness.backend.take_calls();

        harness.feed(ParserEvent::AltScreenEnter(1049));
        assert_eq!(
            harness.backend.take_calls(),
            vec![
                DispatchCall::EnterChrome,
                DispatchCall::EnterFullscreen,
                DispatchCall::SyncGeometry,
                DispatchCall::Feed(b"\x1b[?1049h".to_vec()),
            ]
        );
        assert_eq!(harness.bstate.get(), BlockState::AltScreen);
        harness.feed(ParserEvent::AltScreenLeave(1049));
        assert_eq!(
            harness.backend.take_calls(),
            vec![
                DispatchCall::ExitChrome,
                DispatchCall::Feed(b"\x1b[?1049l".to_vec()),
                DispatchCall::ExitFullscreen,
                DispatchCall::SyncGeometry,
                DispatchCall::Focus,
            ]
        );
        assert_eq!(harness.bstate.get(), BlockState::AwaitingCommand);

        harness.backend.render_row(3, "user@host $ git log");
        harness.feed(dispatch_command_start(None));
        harness.backend.take_calls();
        harness.feed(ParserEvent::AltScreenEnter(1049));
        assert_eq!(harness.bstate.get(), BlockState::AltScreen);
        harness.feed(ParserEvent::AltScreenLeave(1049));
        assert_eq!(harness.bstate.get(), BlockState::CollectingOutput);
        harness.feed(dispatch_bytes("after-pager\r\n"));
        assert_eq!(harness.live_output(), "after-pager\r\n");
        harness.feed(dispatch_command_end(Some(0)));
        harness.feed(ParserEvent::PromptStart);
        let finalized = harness.backend.finalized();
        assert_eq!(finalized.len(), 1);
        let DispatchCall::Finalize(block) = &finalized[0] else {
            unreachable!();
        };
        assert_eq!(block.command, "git log");
        assert_eq!(block.output_plain, "after-pager");
        assert_eq!(
            harness.alt_screen.borrow().as_slice(),
            &[
                AltScreenTransition::Entered,
                AltScreenTransition::Left,
                AltScreenTransition::Entered,
                AltScreenTransition::Left,
            ]
        );
    }

    #[test]
    fn reader_dispatch_nested_command_marks_mint_only_the_outer_block() {
        let harness = ReaderHarness::new();
        harness.backend.render_row(3, "user@host $ ");
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("user@host $ "),
            ParserEvent::PromptEnd,
        ]);
        harness.backend.render_row(3, "user@host $ run-nested");
        harness.feed(dispatch_command_start(None));
        harness.feed(dispatch_command_start(Some("inner")));
        assert_eq!(harness.bstate.get(), BlockState::CollectingOutput);
        harness.feed(dispatch_command_end(Some(7)));
        assert_eq!(harness.bstate.get(), BlockState::CollectingOutput);
        assert!(harness.commands_finished.borrow().is_empty());
        harness.feed(dispatch_command_end(Some(0)));
        harness.feed(ParserEvent::PromptStart);

        let finalized = harness.backend.finalized();
        assert_eq!(finalized.len(), 1);
        let DispatchCall::Finalize(block) = &finalized[0] else {
            unreachable!();
        };
        assert_eq!(block.command, "run-nested");
        assert_eq!(block.exit_code, Some(0));
        assert_eq!(harness.commands_started.borrow().len(), 1);
        assert_eq!(harness.commands_finished.borrow().len(), 1);
        assert_eq!(harness.blocks_finished.borrow().len(), 1);
    }

    #[test]
    fn reader_dispatch_kitty_reply_precedes_image_admission() {
        let harness = ReaderHarness::new();
        harness
            .backend
            .kitty_status
            .set(crate::terminal::kitty_graphics::FeedStatus::Complete);
        {
            let probe_backend = harness.backend.clone();
            let probe_pty = harness.pty.clone();
            harness.backend.set_admit_probe(move || {
                probe_backend.record(DispatchCall::PtyReply(
                    probe_pty.drain_test_slave(PTY_REPLY_WAIT),
                ));
            });
        }
        harness.feed(ParserEvent::ApcSequence(b"Gi=31,a=T;AAAA".to_vec()));
        assert_eq!(
            harness.backend.take_calls(),
            vec![
                DispatchCall::KittyFeed(b"Gi=31,a=T;AAAA".to_vec()),
                DispatchCall::PtyReply(b"\x1b_Gi=31;OK\x1b\\".to_vec()),
                DispatchCall::KittyAdmitPending,
            ]
        );
    }

    #[test]
    fn reader_dispatch_cpr_uses_the_dedicated_row_then_column_query() {
        let harness = ReaderHarness::new();
        harness.backend.render_row(3, "user@host $ ");
        harness.feed(ParserEvent::KeyboardProtocolQuery(
            KeyboardProtocolQuery::CursorPosition,
        ));
        assert_eq!(harness.pty.drain_test_slave(PTY_REPLY_WAIT), b"\x1b[4;13R");
        assert_eq!(
            harness.backend.take_calls(),
            vec![DispatchCall::Query(DispatchQuery::CursorPositionReport)]
        );
    }

    #[test]
    fn reader_dispatch_clipboard_query_discloses_nothing() {
        let harness = ReaderHarness::new();
        harness.config.borrow_mut().allow_remote_clipboard_write = true;
        harness.feed(ParserEvent::ClipboardQuery);
        assert_eq!(
            harness.pty.drain_test_slave(PTY_REPLY_WAIT),
            b"\x1b]52;c;\x1b\\"
        );
        assert!(harness.backend.take_calls().is_empty());
    }

    #[test]
    fn reader_dispatch_color_queries_track_set_and_reset() {
        let harness = ReaderHarness::new();
        harness.config.borrow_mut().cursor = RGBA::new(0.0, 0.0, 1.0, 1.0);
        harness.feed(ParserEvent::ColorQuery(ColorKind::Cursor));
        assert_eq!(
            harness.pty.drain_test_slave(PTY_REPLY_WAIT),
            b"\x1b]12;rgb:0000/0000/ffff\x1b\\"
        );

        harness.config.borrow_mut().background = RGBA::new(1.0, 0.0, 0.0, 1.0);
        harness.feed(ParserEvent::ColorQuery(ColorKind::Background));
        assert_eq!(
            harness.pty.drain_test_slave(PTY_REPLY_WAIT),
            b"\x1b]11;rgb:ffff/0000/0000\x1b\\"
        );
        harness.feed(ParserEvent::ColorSet {
            kind: ColorKind::Background,
            spec: "rgb:0000/ffff/0000".to_string(),
        });
        harness.feed(ParserEvent::ColorQuery(ColorKind::Background));
        assert_eq!(
            harness.pty.drain_test_slave(PTY_REPLY_WAIT),
            b"\x1b]11;rgb:0000/ffff/0000\x1b\\"
        );
        harness.feed(ParserEvent::ColorReset(ColorKind::Background));
        harness.feed(ParserEvent::ColorQuery(ColorKind::Background));
        assert_eq!(
            harness.pty.drain_test_slave(PTY_REPLY_WAIT),
            b"\x1b]11;rgb:ffff/0000/0000\x1b\\"
        );
        assert!(harness.backend.take_calls().is_empty());
    }

    #[test]
    fn reader_dispatch_clipboard_write_is_gated_and_notification_keeps_title() {
        let harness = ReaderHarness::new();
        LAST_NOTIFICATION_AT.with(|last| last.set(None));
        harness.feed(ParserEvent::ClipboardSet("secret".to_string()));
        assert!(harness.backend.take_calls().is_empty());

        harness.config.borrow_mut().allow_remote_clipboard_write = true;
        harness.feed(ParserEvent::ClipboardSet("secret".to_string()));
        harness.feed(ParserEvent::Notification {
            title: Some("build".to_string()),
            body: "finished".to_string(),
        });
        assert_eq!(
            harness.backend.take_calls(),
            vec![
                DispatchCall::SetSystemClipboard("secret".to_string()),
                DispatchCall::DesktopNotify {
                    title: Some("build".to_string()),
                    body: "finished".to_string(),
                },
            ]
        );
    }

    #[test]
    fn verified_submission_rebases_against_the_surface_row_count() {
        let harness = ReaderHarness::new();
        harness.backend.set_row_count(20);
        harness.backend.render_row(10, "$ 123");
        harness.feed_all([
            ParserEvent::PromptStart,
            dispatch_bytes("$ 123"),
            ParserEvent::PromptEnd,
        ]);
        assert_eq!(harness.prompt_end.get(), (5, 10));
        assert_eq!(harness.prompt_rows.get(), 20);
        harness.surface.rows.set(24);
        // Correct rebase is row 14; row 6 is where an inverted delta lands.
        harness.surface.cursor.set((5, 6));
        harness.surface.suffix_is_empty.set(Some(true));
        harness.surface.reads.borrow_mut().clear();

        assert_eq!(
            harness.ctx.verified_submission.begin("echo hi", None),
            Err("the shell prompt visibly contains input".to_string())
        );
        assert_eq!(
            harness.surface.reads(),
            vec![
                SurfaceRead::PromptAnchor {
                    provisional: (5, 10),
                    recorded_rows: 20,
                },
                SurfaceRead::CursorPosition,
                SurfaceRead::SuffixIsEmpty,
            ]
        );
    }

    #[test]
    fn verified_submission_refuses_a_cursor_past_the_anchor() {
        let harness = ReaderHarness::new();
        let anchor = harness.arm_verified_prompt();
        harness.surface.cursor.set((anchor.0 + 1, anchor.1));
        assert_eq!(
            harness.ctx.verified_submission.begin("echo hi", None),
            Err("the shell prompt visibly contains input".to_string())
        );
        assert!(harness
            .surface
            .reads()
            .contains(&SurfaceRead::CursorPosition));
    }

    #[test]
    fn verified_submission_refuses_every_unproven_suffix() {
        for suffix in [None, Some(false)] {
            let harness = ReaderHarness::new();
            harness.arm_verified_prompt();
            harness.surface.suffix_is_empty.set(suffix);
            assert_eq!(
                harness.ctx.verified_submission.begin("echo hi", None),
                Err("the shell prompt visibly contains input".to_string()),
                "suffix answer {suffix:?} must fail closed"
            );
            assert!(harness
                .surface
                .reads()
                .contains(&SurfaceRead::SuffixIsEmpty));
        }
    }

    #[test]
    fn verified_submission_preconditions_fail_before_surface_reads() {
        let cases: [HarnessPreconditionMutation; 4] = [
            ("state", |harness| {
                harness.bstate.set(BlockState::CollectingPrompt)
            }),
            ("anchor readiness", |harness| {
                harness.prompt_ready.set(false)
            }),
            ("dirty input", |harness| {
                harness.ctx.idle_input_dirty_rc.set(true)
            }),
            ("prior PTY write", |harness| {
                harness.ctx.pty_synced_rc.set(true)
            }),
        ];
        for (name, mutate) in cases {
            let harness = ReaderHarness::new();
            harness.arm_verified_prompt();
            mutate(&harness);
            harness.surface.reads.borrow_mut().clear();
            assert_eq!(
                harness.ctx.verified_submission.begin("echo hi", None),
                Err("the shell prompt is no longer verified empty".to_string()),
                "{name} must refuse"
            );
            assert!(harness.surface.reads().is_empty());
        }

        let foreign = ReaderHarness::with_foreground(false);
        foreign.arm_verified_prompt();
        foreign.surface.reads.borrow_mut().clear();
        assert_eq!(
            foreign.ctx.verified_submission.begin("echo hi", None),
            Err("the shell prompt is no longer verified empty".to_string())
        );
        assert!(foreign.surface.reads().is_empty());
    }

    #[test]
    fn verified_agent_submission_requires_token_aware_integration() {
        let harness = ReaderHarness::new();
        harness.arm_verified_prompt();
        let execution = AgentExecutionRef {
            epoch: AgentSession::new(1, 2, 1).epoch(),
            generation: 9,
        };
        assert_eq!(
            harness
                .ctx
                .verified_submission
                .begin("echo hi", Some(execution)),
            Err(
                "Shell Agent execution requires the bundled token-aware bash/zsh integration"
                    .to_string()
            )
        );
        assert!(harness.surface.reads().is_empty());
        assert!(harness.agent_lost.borrow().is_empty());
    }

    #[test]
    fn alt_screen_boundaries_are_typed_content_free_and_not_coalesced() {
        let callbacks: AltScreenCallbacks = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::new(RefCell::new(Vec::new()));
        let seen_for_callback = seen.clone();
        callbacks.borrow_mut().push(Box::new(move |transition| {
            seen_for_callback.borrow_mut().push(transition)
        }));

        emit_alt_screen_transition(&callbacks, AltScreenTransition::Entered);
        emit_alt_screen_transition(&callbacks, AltScreenTransition::Left);

        assert_eq!(
            seen.borrow().as_slice(),
            &[AltScreenTransition::Entered, AltScreenTransition::Left]
        );
    }

    #[test]
    fn block_id_allocator_skips_reserved_history_without_retaining_live_ids() {
        let mut reserved = HashSet::from([0, 2, u64::MAX]);
        let mut candidate = 0_u64;
        let claimed = claim_next_unused_block_id(&mut reserved, || {
            let current = candidate;
            candidate += 1;
            current
        });
        assert_eq!(claimed, 1);
        assert!(!reserved.contains(&0));
        assert!(reserved.contains(&2));
        assert!(reserved.contains(&u64::MAX));
        assert!(!reserved.contains(&1));
    }

    #[test]
    fn process_block_ids_have_a_random_namespace_and_checked_sequence_space() {
        let namespace = process_block_id_namespace();
        assert_ne!(namespace, 0);
        assert_eq!(namespace & (BLOCK_ID_SEQUENCE_LIMIT - 1), 0);
        assert!(namespace.checked_add(BLOCK_ID_SEQUENCE_LIMIT - 1).is_some());
    }

    #[test]
    fn unread_badge_tracks_only_retained_newest_blocks() {
        // Three read blocks followed by two unread blocks.
        assert_eq!(unread_after_prefix_eviction(5, 2, 2), 2);
        assert_eq!(unread_after_prefix_eviction(5, 2, 4), 1);
        assert_eq!(unread_after_prefix_eviction(5, 2, 5), 0);

        assert_eq!(unread_after_index_removal(5, 2, 1), 2);
        assert_eq!(unread_after_index_removal(5, 2, 3), 1);
        assert_eq!(unread_after_index_removal(5, 2, 4), 1);
        assert_eq!(unread_after_prefix_eviction(1, u32::MAX, 1), 0);
    }

    #[test]
    fn private_command_ids_require_the_exact_token_and_decimal_sequence() {
        let token = "0123456789abcdef0123456789abcdef";
        assert!(command_id_uses_shell_token(
            "0123456789abcdef0123456789abcdef-17",
            token
        ));
        assert!(!command_id_uses_shell_token(token, token));
        assert!(!command_id_uses_shell_token(
            "0123456789abcdef0123456789abcdef-other",
            token
        ));
        assert!(!command_id_uses_shell_token("anvil-bash-1-17", token));
    }

    #[test]
    fn agent_token_request_is_limited_to_direct_interactive_bash_and_zsh() {
        let strings = |values: &[&str]| {
            values
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
        };
        assert!(shell_argv_supports_agent_ids(&strings(&["bash", "-l"])));
        assert!(shell_argv_supports_agent_ids(&strings(&["/bin/zsh", "-i"])));
        assert!(!shell_argv_supports_agent_ids(&strings(&[
            "bash", "-lc", "true"
        ])));
        assert!(!shell_argv_supports_agent_ids(&strings(&[
            "zsh",
            "--command",
            "true"
        ])));
        assert!(!shell_argv_supports_agent_ids(&strings(&["fish"])));
        assert!(!shell_argv_supports_agent_ids(&strings(&[
            "ssh", "host", "bash"
        ])));
        assert!(!shell_argv_supports_agent_ids(&strings(&[
            "bash",
            "/usr/bin/jsh"
        ])));
    }

    fn legacy_capability_observer_feed(
        observer: &mut ShellCapabilityObserver,
        bytes: &[u8],
        expected: &str,
        ready: &Cell<bool>,
    ) {
        use super::CapabilityOscState;

        for &byte in bytes {
            let state = std::mem::take(&mut observer.state);
            observer.state = match state {
                CapabilityOscState::Ground => {
                    if byte == 0x1b {
                        CapabilityOscState::Escape
                    } else {
                        CapabilityOscState::Ground
                    }
                }
                CapabilityOscState::Escape => match byte {
                    b']' => CapabilityOscState::Osc(Vec::new()),
                    0x1b => CapabilityOscState::Escape,
                    _ => CapabilityOscState::Ground,
                },
                CapabilityOscState::Osc(mut payload) => match byte {
                    0x07 => {
                        observer.finish_osc(&payload, expected, ready);
                        CapabilityOscState::Ground
                    }
                    0x1b => CapabilityOscState::OscEscape(payload),
                    _ if payload.len() < super::MAX_CAPABILITY_OSC_BYTES => {
                        payload.push(byte);
                        CapabilityOscState::Osc(payload)
                    }
                    _ => CapabilityOscState::Discard,
                },
                CapabilityOscState::OscEscape(payload) => match byte {
                    b'\\' => {
                        observer.finish_osc(&payload, expected, ready);
                        CapabilityOscState::Ground
                    }
                    0x1b => CapabilityOscState::OscEscape(payload),
                    _ => CapabilityOscState::Discard,
                },
                CapabilityOscState::Discard => {
                    if byte == 0x1b {
                        CapabilityOscState::DiscardEscape
                    } else if byte == 0x07 {
                        CapabilityOscState::Ground
                    } else {
                        CapabilityOscState::Discard
                    }
                }
                CapabilityOscState::DiscardEscape => match byte {
                    b'\\' => CapabilityOscState::Ground,
                    0x1b => CapabilityOscState::DiscardEscape,
                    _ => CapabilityOscState::Discard,
                },
            };
        }
    }

    #[test]
    fn capability_observer_ground_fast_path_matches_legacy_at_every_split() {
        let token = "0123456789abcdef0123456789abcdef";
        let mut oversized_bel = b"\x1b]".to_vec();
        oversized_bel.extend(std::iter::repeat_n(
            b'x',
            super::MAX_CAPABILITY_OSC_BYTES + 17,
        ));
        oversized_bel.extend_from_slice(b"\x07tail");
        let corpora: Vec<Vec<u8>> = vec![
            b"plain compiler output without escapes".to_vec(),
            format!("\x1b]133;A\x07plain\x1b]7771;{token}\x07\x1b]133;B\x07tail").into_bytes(),
            b"\x1b]0;ordinary title\x07tail".to_vec(),
            b"\x1b]0;st title\x1b\\tail".to_vec(),
            b"\x1b]0;lenient-repeat\x1b\x1b\\tail".to_vec(),
            b"\x1b]0;discard-on-malformed\x1bcpast-reset".to_vec(),
            b"\x1bP1;2|dcs payload\x1b\\tail".to_vec(),
            b"\x1b_apc payload\x1b\\tail".to_vec(),
            b"\x1b[3Jplain\x1bcplain".to_vec(),
            oversized_bel,
        ];

        for bytes in corpora {
            for split in 0..=bytes.len() {
                let mut legacy = ShellCapabilityObserver::default();
                let mut optimized = ShellCapabilityObserver::default();
                let legacy_ready = Cell::new(false);
                let optimized_ready = Cell::new(false);
                for chunk in [&bytes[..split], &bytes[split..]] {
                    legacy_capability_observer_feed(&mut legacy, chunk, token, &legacy_ready);
                    optimized.feed(chunk, token, &optimized_ready);
                }
                assert_eq!(optimized, legacy, "split {split} of {bytes:?}");
                assert_eq!(
                    optimized_ready.get(),
                    legacy_ready.get(),
                    "readiness at split {split} of {bytes:?}"
                );
            }

            let mut legacy = ShellCapabilityObserver::default();
            let mut optimized = ShellCapabilityObserver::default();
            let legacy_ready = Cell::new(false);
            let optimized_ready = Cell::new(false);
            for byte in &bytes {
                legacy_capability_observer_feed(
                    &mut legacy,
                    std::slice::from_ref(byte),
                    token,
                    &legacy_ready,
                );
                optimized.feed(std::slice::from_ref(byte), token, &optimized_ready);
            }
            assert_eq!(optimized, legacy, "bytewise {bytes:?}");
            assert_eq!(optimized_ready.get(), legacy_ready.get());
        }
    }

    #[test]
    #[ignore = "micro-benchmark; run explicitly with --ignored --nocapture"]
    fn capability_observer_ground_fast_path_micro_benchmark() {
        use std::hint::black_box;

        const CHUNK_BYTES: usize = 32 * 1024;
        const ITERATIONS: usize = 16_384;
        let bytes = vec![b'x'; CHUNK_BYTES];
        let token = "0123456789abcdef0123456789abcdef";

        let legacy_ready = Cell::new(false);
        let mut legacy_observer = ShellCapabilityObserver::default();
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            legacy_capability_observer_feed(
                &mut legacy_observer,
                black_box(&bytes),
                token,
                &legacy_ready,
            );
        }
        black_box((&legacy_observer, legacy_ready.get()));
        let legacy = started.elapsed();

        let optimized_ready = Cell::new(false);
        let mut optimized_observer = ShellCapabilityObserver::default();
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            optimized_observer.feed(black_box(&bytes), token, &optimized_ready);
        }
        black_box((&optimized_observer, optimized_ready.get()));
        let optimized = started.elapsed();

        let mib = (CHUNK_BYTES * ITERATIONS) as f64 / (1024.0 * 1024.0);
        eprintln!(
            "{mib:.0} MiB plain chunks: legacy-capability-observer={legacy:?}, ground-memchr={optimized:?}, speedup={:.2}x",
            legacy.as_secs_f64() / optimized.as_secs_f64()
        );
    }

    #[test]
    fn capability_observer_is_strict_hidden_streaming_and_prompt_scoped() {
        let token = "0123456789abcdef0123456789abcdef";
        let ready = Cell::new(false);
        let mut observer = ShellCapabilityObserver::default();

        observer.feed(
            b"\x1b]7771;0123456789abcdef0123456789abcdef\x07",
            token,
            &ready,
        );
        assert!(
            !ready.get(),
            "an announcement outside A..B is not a capability"
        );
        observer.feed(b"\x1b]133;A\x07\x1b]7771;0123456789ab", token, &ready);
        assert!(!ready.get(), "a split packet is not accepted early");
        observer.feed(b"cdef0123456789abcdef\x1b\\", token, &ready);
        assert!(ready.get());
        observer.feed(b"\x1b]133;B\x07", token, &ready);
        assert!(
            ready.get(),
            "PromptEnd retains capability for that idle prompt"
        );
        observer.feed(b"\x1b]133;A\x07", token, &ready);
        assert!(!ready.get(), "the next prompt must announce again");
        observer.feed(
            b"\x1b]7771;ffffffffffffffffffffffffffffffff\x07",
            token,
            &ready,
        );
        assert!(!ready.get(), "a different well-formed token is untrusted");

        let mut oversized = b"\x1b]".to_vec();
        oversized.extend(std::iter::repeat_n(b'x', 256));
        oversized
            .extend_from_slice(b"\x07\x1b]133;A\x07\x1b]7771;0123456789abcdef0123456789abcdef\x07");
        observer.feed(&oversized, token, &ready);
        assert!(
            ready.get(),
            "discarding an oversized OSC must recover at BEL"
        );
    }

    #[test]
    fn reviewed_identity_is_exact_and_post_enter_bytes_fail_closed() {
        assert_eq!(normalize_captured_command("$HOME", "$"), "$HOME");
        assert_eq!(
            normalize_captured_command("git status", "git"),
            "git status"
        );
        assert!(reviewed_submission_matches(
            None,
            "printf ok",
            "printf ok",
            false
        ));
        assert!(!reviewed_submission_matches(
            None,
            " printf ok",
            "printf ok",
            false
        ));
        assert!(!reviewed_submission_matches(
            None,
            "printf ok",
            "printf ok",
            true
        ));
        assert!(!reviewed_submission_matches(
            Some("printf other"),
            "printf ok",
            "printf ok",
            false
        ));

        assert!(reviewed_pre_command_bytes_are_identity_neutral(
            b"\r\n\x1b[?2004l\x1b[0m"
        ));
        assert!(!reviewed_pre_command_bytes_are_identity_neutral(b"suffix"));
        assert!(!reviewed_pre_command_bytes_are_identity_neutral(b"\x1b[2K"));
    }

    #[test]
    fn reviewed_execution_payload_rejects_whitespace_and_controls() {
        assert_eq!(
            approved_command_submission_payload("printf ok").unwrap(),
            b"printf ok"
        );
        assert!(approved_command_submission_payload(" printf ok").is_err());
        assert!(approved_command_submission_payload("printf ok ").is_err());
        assert!(approved_command_submission_payload("printf ok\nwhoami").is_err());
    }

    #[test]
    fn agent_command_end_requires_shell_foreground_and_a_trusted_pair() {
        assert_eq!(
            decide_agent_command_end(false, None, false),
            AgentCommandEndDecision::Accept
        );
        assert_eq!(
            decide_agent_command_end(true, Some(false), true),
            AgentCommandEndDecision::IgnoreUntilShellOwnsForeground
        );
        for (foreground, trusted) in [(None, true), (Some(true), false)] {
            assert_eq!(
                decide_agent_command_end(true, foreground, trusted),
                AgentCommandEndDecision::AcceptWithoutAgentCorrelation
            );
        }
        assert_eq!(
            decide_agent_command_end(true, Some(true), true),
            AgentCommandEndDecision::Accept
        );
    }

    #[test]
    fn command_prompt_status_explains_every_agent_gate() {
        assert_eq!(
            classify_command_prompt_status(BlockState::AwaitingCommand, false, false, false, true,),
            CommandPromptStatus::Ready
        );
        for status in [
            classify_command_prompt_status(BlockState::AwaitingCommand, false, true, false, true),
            classify_command_prompt_status(BlockState::AwaitingCommand, false, false, true, true),
            classify_command_prompt_status(BlockState::AwaitingCommand, false, false, false, false),
        ] {
            assert_eq!(status, CommandPromptStatus::HasInput);
        }
        assert_eq!(
            classify_command_prompt_status(BlockState::CollectingOutput, false, false, false, true,),
            CommandPromptStatus::Running
        );
        assert_eq!(
            classify_command_prompt_status(BlockState::AwaitingCommand, true, false, false, true,),
            CommandPromptStatus::Fullscreen
        );
        assert_eq!(
            classify_command_prompt_status(BlockState::Idle, false, false, false, true),
            CommandPromptStatus::Initializing
        );
        assert_eq!(
            classify_command_prompt_status(BlockState::RawFallback, false, false, false, true),
            CommandPromptStatus::ShellIntegrationUnavailable
        );
    }

    #[test]
    fn stranded_focus_recovers_on_typing_keys_only() {
        use gtk::gdk::{Key, ModifierType};
        use relm4::gtk;

        // Typing-shaped keys pull focus back to the live prompt.
        assert!(stranded_focus_key_recovers(Key::a, ModifierType::empty()));
        assert!(stranded_focus_key_recovers(
            Key::A,
            ModifierType::SHIFT_MASK
        ));
        assert!(stranded_focus_key_recovers(
            Key::space,
            ModifierType::empty()
        ));
        assert!(stranded_focus_key_recovers(
            Key::Return,
            ModifierType::empty()
        ));
        assert!(stranded_focus_key_recovers(
            Key::BackSpace,
            ModifierType::empty()
        ));
        assert!(stranded_focus_key_recovers(
            Key::Escape,
            ModifierType::empty()
        ));

        // Chords stay on their normal dispatch paths — in particular the
        // unbound Ctrl+C interrupt fallback in the running-root handler.
        assert!(!stranded_focus_key_recovers(
            Key::c,
            ModifierType::CONTROL_MASK
        ));
        assert!(!stranded_focus_key_recovers(Key::a, ModifierType::ALT_MASK));
        assert!(!stranded_focus_key_recovers(
            Key::t,
            ModifierType::SUPER_MASK
        ));

        // A modifier press on its own is not typing.
        assert!(!stranded_focus_key_recovers(
            Key::Shift_L,
            ModifierType::empty()
        ));
        assert!(!stranded_focus_key_recovers(
            Key::Control_R,
            ModifierType::CONTROL_MASK
        ));

        // Focus navigation and reading keys keep their meaning; block find
        // deliberately lands focus on the picked block for reading.
        assert!(!stranded_focus_key_recovers(
            Key::Tab,
            ModifierType::empty()
        ));
        assert!(!stranded_focus_key_recovers(
            Key::ISO_Left_Tab,
            ModifierType::SHIFT_MASK
        ));
        assert!(!stranded_focus_key_recovers(Key::Up, ModifierType::empty()));
        assert!(!stranded_focus_key_recovers(
            Key::Page_Down,
            ModifierType::empty()
        ));
        assert!(!stranded_focus_key_recovers(
            Key::End,
            ModifierType::empty()
        ));
    }

    #[test]
    fn keyboard_protocol_queries_have_safe_fallback_replies() {
        assert_eq!(
            build_keyboard_query_reply(KeyboardProtocolQuery::KittyQuery, 0, 0),
            "\x1b[?0u"
        );
        assert_eq!(
            build_keyboard_query_reply(KeyboardProtocolQuery::ModifyOtherKeysQuery, 0, 0),
            "\x1b[>4;0m"
        );
        assert_eq!(
            build_keyboard_query_reply(KeyboardProtocolQuery::PrimaryDeviceAttributes, 0, 0),
            "\x1b[?1;2c"
        );
        assert_eq!(
            build_keyboard_query_reply(KeyboardProtocolQuery::DeviceStatus, 0, 0),
            "\x1b[0n"
        );
        assert_eq!(
            build_keyboard_query_reply(KeyboardProtocolQuery::CursorPosition, 4, 2),
            "\x1b[3;5R"
        );
        assert_eq!(
            build_keyboard_query_reply(KeyboardProtocolQuery::CursorPosition, -8, -2),
            "\x1b[1;1R"
        );
        let version = build_keyboard_query_reply(KeyboardProtocolQuery::XtVersion, 0, 0);
        assert!(version.starts_with("\x1bP>|anvil "));
        assert!(version.contains(env!("CARGO_PKG_VERSION")));
        assert!(version.ends_with("\x1b\\"));
    }

    #[test]
    fn dynamic_color_set_changes_query_reply_and_reset_restores_theme() {
        let config = Config::safe_defaults();
        let mut dynamic = DynamicColors::default();
        let theme_reply = build_color_query_reply(&config, dynamic, ColorKind::Background);

        dynamic.set(ColorKind::Background, "#ff8000");
        assert_eq!(
            build_color_query_reply(&config, dynamic, ColorKind::Background),
            "\x1b]11;rgb:ffff/8080/0000\x1b\\"
        );
        // Only the set slot is overridden; foreground still answers from theme.
        assert_eq!(
            build_color_query_reply(&config, dynamic, ColorKind::Foreground),
            build_color_query_reply(&config, DynamicColors::default(), ColorKind::Foreground)
        );

        dynamic.reset(ColorKind::Background);
        assert_eq!(
            build_color_query_reply(&config, dynamic, ColorKind::Background),
            theme_reply
        );
    }

    #[test]
    fn dynamic_color_specs_accept_xparsecolor_and_hex_forms() {
        // XParseColor `rgb:` channels scale by digit count: "8" and "80" both
        // mean mid intensity relative to their own maximum.
        let mut dynamic = DynamicColors::default();
        dynamic.set(ColorKind::Foreground, "rgb:ff/80/00");
        let config = Config::safe_defaults();
        assert_eq!(
            build_color_query_reply(&config, dynamic, ColorKind::Foreground),
            "\x1b]10;rgb:ffff/8080/0000\x1b\\"
        );
        dynamic.set(ColorKind::Cursor, "rgb:ffff/8080/0000");
        assert_eq!(
            build_color_query_reply(&config, dynamic, ColorKind::Cursor),
            "\x1b]12;rgb:ffff/8080/0000\x1b\\"
        );
        assert!(parse_color_spec("#fff").is_some());
        assert!(parse_color_spec("red").is_some());
    }

    #[test]
    fn junk_or_palette_color_specs_leave_tracking_unchanged() {
        let mut dynamic = DynamicColors::default();
        dynamic.set(ColorKind::Background, "definitely-not-a-color");
        assert!(dynamic.get(ColorKind::Background).is_none());
        dynamic.set(ColorKind::Background, "rgb:zz/00/00");
        assert!(dynamic.get(ColorKind::Background).is_none());
        dynamic.set(ColorKind::Background, "rgb:11/22");
        assert!(dynamic.get(ColorKind::Background).is_none());
        // OSC 4 palette entries are VTE-native and intentionally not tracked.
        dynamic.set(ColorKind::Palette(3), "#102030");
        assert!(dynamic.get(ColorKind::Palette(3)).is_none());
    }

    #[test]
    fn dynamic_overlay_substitutes_only_overridden_slots() {
        let config = Config::safe_defaults();
        let mut dynamic = DynamicColors::default();
        dynamic.set(ColorKind::Background, "#102030");
        let effective = dynamic.overlay(&config);
        assert_eq!(effective.background, parse_color_spec("#102030").unwrap());
        assert_eq!(effective.foreground, config.foreground);
        assert_eq!(effective.cursor, config.cursor);
    }

    /// The pane-shared cell, seeded like a program that changed background and
    /// cursor color at runtime.
    fn tracked_colors(sets: &[(ColorKind, &str)]) -> DynamicColorsRc {
        let mut colors = DynamicColors::default();
        for (kind, spec) in sets {
            colors.set(*kind, spec);
        }
        Rc::new(Cell::new(colors))
    }

    #[test]
    fn rebuilt_blocks_use_the_dynamic_overlay_not_the_plain_theme() {
        // Undo-clear rebuilds stashed blocks through the same helper the reader
        // uses, so a restored block cannot render theme-colored next to blocks
        // that were recolored by an OSC 11 change.
        let config = Config::safe_defaults();
        let dynamic = tracked_colors(&[
            (ColorKind::Background, "#102030"),
            (ColorKind::Cursor, "rgb:ff/80/00"),
        ]);
        let rebuilt = finished_block_config(&dynamic, &config);
        assert_eq!(rebuilt.background, parse_color_spec("#102030").unwrap());
        assert_eq!(rebuilt.cursor, parse_color_spec("rgb:ff/80/00").unwrap());
        // Untouched slots stay on the theme.
        assert_eq!(rebuilt.foreground, config.foreground);

        // With nothing tracked the rebuild is the plain theme again.
        let plain = finished_block_config(&Rc::new(Cell::new(DynamicColors::default())), &config);
        assert_eq!(plain.background, config.background);
        assert_eq!(plain.cursor, config.cursor);
    }

    #[test]
    fn applying_a_theme_clears_dynamic_tracking() {
        let config = Config::safe_defaults();
        let dynamic = tracked_colors(&[
            (ColorKind::Foreground, "#ff8000"),
            (ColorKind::Background, "#102030"),
            (ColorKind::Cursor, "#00ff00"),
        ]);
        assert_ne!(
            build_color_query_reply(&config, dynamic.get(), ColorKind::Background),
            build_color_query_reply(&config, DynamicColors::default(), ColorKind::Background)
        );

        clear_dynamic_colors(&dynamic);

        // Every slot answers from the theme again, and blocks built afterwards
        // are theme-colored like the repainted live view.
        for kind in [
            ColorKind::Foreground,
            ColorKind::Background,
            ColorKind::Cursor,
        ] {
            assert!(dynamic.get().get(kind).is_none());
            assert_eq!(
                build_color_query_reply(&config, dynamic.get(), kind),
                build_color_query_reply(&config, DynamicColors::default(), kind)
            );
        }
        let rebuilt = finished_block_config(&dynamic, &config);
        assert_eq!(rebuilt.background, config.background);
        assert_eq!(rebuilt.foreground, config.foreground);
        assert_eq!(rebuilt.cursor, config.cursor);
    }

    #[test]
    fn background_output_requires_visible_text() {
        assert!(!background_output_has_visible_text(b"\r\n\x1b[0m"));
        assert!(background_output_has_visible_text(
            b"\x1b[36mworker finished\x1b[0m\r\n"
        ));
        assert!(background_output_has_visible_text(b"visible\rhidden"));
        assert!(!background_output_has_visible_text(b"visible\r\x1b[2K"));
        assert!(!background_output_has_visible_text(
            b"\x1b]8;;https://example.invalid\x1b\\\x1b]8;;\x1b\\"
        ));
    }

    #[test]
    fn invalid_utf8_visibility_does_not_materialize_the_lazy_payload() {
        let invalid = vec![0xff; 4 * 1024 * 1024];
        let payload = super::LazyBlockRenderPayload::new(
            "$ ".to_string(),
            super::CapturedFinalizeOutput::Background(invalid.iter().copied().collect()),
            false,
        );
        assert!(background_output_has_visible_text(&invalid));
        assert_eq!(
            payload.materialization_count(),
            0,
            "visibility must inspect raw bytes without constructing render strings"
        );
    }

    #[test]
    fn taking_background_output_drains_the_pending_buffer() {
        let mut pending = VecDeque::from(b"async line\r\n".to_vec());
        let mut taken = take_background_output(&mut pending).expect("visible output");
        assert_eq!(taken.make_contiguous(), b"async line\r\n");
        assert!(pending.is_empty());
        assert!(take_background_output(&mut pending).is_none());
    }

    #[test]
    fn background_block_copy_has_no_blank_command_line() {
        assert_eq!(
            block_clipboard_text("", "worker finished\nnext line", false),
            "worker finished\nnext line"
        );
        assert_eq!(block_clipboard_text("echo ok", "ok", false), "echo ok\nok");
        assert_eq!(block_clipboard_text("echo ok", "ok", true), "ok");
    }

    fn ev_summary(events: &[ParserEvent]) -> Vec<String> {
        events
            .iter()
            .map(|e| match e {
                ParserEvent::Bytes(b) => format!("B({})", String::from_utf8_lossy(b)),
                ParserEvent::PromptStart => "PS".to_string(),
                ParserEvent::PromptEnd => "PE".to_string(),
                ParserEvent::CommandStart(_) => "CS".to_string(),
                ParserEvent::CommandEnd { exit, .. } => format!("CE({exit:?})"),
                ParserEvent::AltScreenEnter(mode) => format!("ALT+({mode})"),
                ParserEvent::AltScreenLeave(mode) => format!("ALT-({mode})"),
                _ => "?".to_string(),
            })
            .collect()
    }

    #[test]
    fn coalesce_merges_consecutive_bytes() {
        let mut events = vec![
            ParserEvent::Bytes(b"hello ".to_vec()),
            ParserEvent::Bytes(b"world".to_vec()),
            ParserEvent::Bytes(b"!".to_vec()),
        ];
        coalesce_bytes_events(&mut events);
        assert_eq!(ev_summary(&events), vec!["B(hello world!)"]);
    }

    #[test]
    fn selected_block_range_is_inclusive_in_both_directions() {
        let ids = [10, 20, 30, 40];
        assert_eq!(selected_id_range(&ids, 20, 40), vec![20, 30, 40]);
        assert_eq!(selected_id_range(&ids, 40, 20), vec![20, 30, 40]);
        assert_eq!(selected_id_range(&ids, 99, 30), vec![30]);
    }

    #[test]
    fn post_command_metadata_detection_matches_shell_osc_updates() {
        assert!(is_post_command_metadata(
            b"\x1b]7;file://host/home/tester\x1b\\"
        ));
        assert!(is_post_command_metadata(b"\x1b]0;title\x1b\\"));
        assert!(!is_post_command_metadata(b"/home/tester\r\n"));
        assert!(!is_post_command_metadata(b"daily.txt  Documents\r\n"));
    }

    #[test]
    fn notification_permitted_when_first_or_interval_elapsed() {
        let now = Instant::now();
        assert!(notification_permitted(None, now));
        assert!(notification_permitted(
            Some(now - NOTIFICATION_MIN_INTERVAL),
            now
        ));
    }

    #[test]
    fn notification_dropped_inside_min_interval() {
        let now = Instant::now();
        // A notification just shown (same batch) blocks the next one.
        assert!(!notification_permitted(Some(now), now));
        assert!(!notification_permitted(
            Some(now - NOTIFICATION_MIN_INTERVAL + std::time::Duration::from_millis(1)),
            now
        ));
    }

    #[test]
    fn command_finalize_falls_back_when_vte_feed_has_not_rendered_yet() {
        assert_eq!(finished_command("", "git status"), "git status");
        assert_eq!(finished_command("  ", "git status"), "git status");
    }

    #[test]
    fn command_finalize_prefers_vte_capture_for_edited_or_recalled_input() {
        assert_eq!(
            finished_command("git status --short", "git status"),
            "git status --short"
        );
    }

    #[test]
    fn multiline_recall_uses_bracketed_paste_when_available() {
        let recall = build_command_recall("printf a\r\nprintf b", true);
        assert_eq!(recall.echo_text, "printf a\nprintf b");
        // The leading 0x15 is new: the line kill now rides in the same payload
        // and is unconditional, because typed text the user has not submitted is
        // not represented by any flag this app owns.
        assert_eq!(recall.bytes, b"\x15\x1b[200~printf a\nprintf b\x1b[201~");
    }

    #[test]
    fn multiline_recall_falls_back_to_first_line_without_bracketed_paste() {
        let recall = build_command_recall("printf a\nprintf b", false);
        assert_eq!(recall.echo_text, "printf a");
        assert_eq!(recall.bytes, b"\x15printf a");
        assert!(recall.risk.truncated_to_first_line);
    }

    /// Was `single_line_recall_does_not_add_paste_markers`. The shared encoder
    /// frames every payload the shell can unframe itself, single line included,
    /// which is what keeps one code path for "put this text on the prompt" — and
    /// a framed insertion is also inert to history expansion. Without DECSET 2004
    /// the bytes are still bare.
    #[test]
    fn single_line_recall_is_framed_only_when_the_shell_advertises_it() {
        let bracketed = build_command_recall("git status", true);
        assert_eq!(bracketed.echo_text, "git status");
        assert_eq!(bracketed.bytes, b"\x15\x1b[200~git status\x1b[201~");

        let bare = build_command_recall("git status", false);
        assert_eq!(bare.bytes, b"\x15git status");
    }

    /// A recalled command is text this app captured; a block whose output (or
    /// whose scraped command line) carried a paste terminator must not be able to
    /// close the frame early and have the rest of itself executed.
    #[test]
    fn recall_strips_a_paste_terminator_out_of_the_command() {
        let recall = build_command_recall("echo ok\x1b[201~\rrm -rf ~", true);
        assert!(recall.risk.had_embedded_paste_marker);
        assert!(!recall
            .bytes
            .windows(6)
            .any(|window| window == b"\x1b[201" as &[u8] || window == b"[201~\r"));
        assert_eq!(recall.echo_text, "echo ok\nrm -rf ~");
    }

    #[test]
    fn recall_strips_terminal_controls_from_captured_history() {
        let recall = build_command_recall("echo \x1b[31mred", true);
        assert_eq!(recall.echo_text, "echo [31mred");
        assert!(recall.risk.had_controls);
        assert!(!recall.bytes.contains(&0x1b) || recall.bytes.starts_with(b"\x15\x1b[200~"));
    }

    #[test]
    fn recall_rejects_visual_spoofing_and_oversize_without_clearing_the_prompt() {
        assert!(build_command_recall("echo safe\u{202e}txt", true).is_empty());
        assert!(build_command_recall("echo safe\u{00ad}txt", true).is_empty());
        assert!(build_command_recall("echo safe\u{e0020}txt", true).is_empty());
        assert!(build_command_recall(&"x".repeat(MAX_RECALLED_COMMAND_BYTES + 1), true).is_empty());
    }

    #[test]
    fn agent_commands_are_single_line_bounded_and_visually_unambiguous() {
        assert!(agent_command_is_safe("cargo test"));
        assert!(!agent_command_is_safe("cargo test\nrm -rf ~"));
        assert!(!agent_command_is_safe("echo safe\u{2066}hidden"));
        assert!(!agent_command_is_safe("echo safe\u{fe0f}hidden"));
        assert!(!agent_command_is_safe(
            &"x".repeat(MAX_RECALLED_COMMAND_BYTES + 1)
        ));
    }

    #[test]
    fn recall_of_an_empty_command_writes_nothing_at_all() {
        // Not even the kill-line: the recall paths bail on this instead of
        // wiping a line the user was editing.
        assert!(build_command_recall("", true).bytes.is_empty());
    }

    /// The clipboard injection this repo was vulnerable to: the frame used to be
    /// three separate PTY writes, so the body arrived with a frame already open
    /// and reached the shell verbatim.
    #[test]
    fn clipboard_paste_neutralizes_an_embedded_paste_terminator() {
        let paste = build_clipboard_paste("docs\x1b[201~\rrm -rf ~\r", true);
        assert!(paste.risk.had_embedded_paste_marker);
        assert_eq!(
            paste.bytes,
            b"\x1b[200~docs\nrm -rf ~\n\x1b[201~",
            "{:?}",
            String::from_utf8_lossy(&paste.bytes)
        );
        // No trailing CR outside the frame either: a paste never submits.
        assert!(!paste.bytes.ends_with(b"\r"));
    }

    #[test]
    fn clipboard_paste_keeps_anvils_first_line_fallback_and_strips_controls() {
        let paste = build_clipboard_paste("echo one\necho two", false);
        assert_eq!(paste.echo_text, "echo one");
        assert!(paste.risk.truncated_to_first_line);

        // A clipboard is untrusted text; an escape sequence in it would drive the
        // terminal rather than land in the shell's line buffer.
        let colored = build_clipboard_paste("echo \x1b[31mred", true);
        assert_eq!(colored.echo_text, "echo [31mred");
        assert!(colored.risk.had_controls);
    }

    #[test]
    fn pasted_text_is_mirrored_into_the_editor_shadow_only_at_the_prompt() {
        let typed = RefCell::new(String::from("git "));
        let synced = Cell::new(false);
        let dirty = Cell::new(false);

        record_external_input(
            BlockState::AwaitingCommand,
            "status",
            &typed,
            &synced,
            &dirty,
        );
        assert_eq!(typed.borrow().as_str(), "git status");
        assert!(synced.get(), "the shell's line buffer now holds our text");
        assert!(dirty.get());

        // While a command runs the same bytes are that program's stdin, not an
        // edited command line.
        let running = Cell::new(false);
        let running_dirty = Cell::new(false);
        let running_typed = RefCell::new(String::new());
        record_external_input(
            BlockState::CollectingOutput,
            "y",
            &running_typed,
            &running,
            &running_dirty,
        );
        assert!(running_typed.borrow().is_empty());
        assert!(!running.get());
    }

    #[test]
    fn shell_reported_command_beats_the_screen_scrape() {
        let (command, source) = resolve_command_text(Some("git commit -m 'x'"), false, "git c…");
        assert_eq!(command, "git commit -m 'x'");
        assert_eq!(source, CommandTextSource::ShellReported);
    }

    #[test]
    fn a_bare_mark_falls_back_to_the_screen_scrape() {
        let (command, source) = resolve_command_text(None, false, "ls -la");
        assert_eq!(command, "ls -la");
        assert_eq!(source, CommandTextSource::Screen);
    }

    /// A shell that dropped an oversized command line is not a shell without
    /// integration: a command really ran, so the block must survive even when the
    /// screen capture came back empty, instead of being filed as commandless
    /// background output.
    #[test]
    fn a_truncated_command_line_is_its_own_case() {
        let (command, source) = resolve_command_text(None, true, "for f in *; do");
        assert_eq!(command, "for f in *; do");
        assert_eq!(source, CommandTextSource::ScreenAfterTruncation);

        let (placeholder, source) = resolve_command_text(None, true, "   ");
        assert_eq!(placeholder, TRUNCATED_COMMAND_PLACEHOLDER);
        assert_eq!(source, CommandTextSource::ScreenAfterTruncation);
        // Contrast: no metadata at all and nothing on screen stays empty, which
        // the finalize path reads as "nothing meaningful to record".
        assert_eq!(resolve_command_text(None, false, "").0, "");
    }

    #[test]
    fn the_shells_own_duration_beats_the_local_timer() {
        let started = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let ended = started + std::time::Duration::from_millis(4_000);

        assert_eq!(block_duration_ms(Some(37), Some(started), ended), Some(37));
        // No shell figure: the local timer, which also contains the shell's
        // post-command work and this process's dispatch latency.
        assert_eq!(block_duration_ms(None, Some(started), ended), Some(4_000));
        assert_eq!(block_duration_ms(None, None, ended), None);
        // A clock that went backwards is not a duration.
        assert_eq!(block_duration_ms(None, Some(ended), started), None);
    }

    fn test_block(id: u64, cmd: &str, exit_code: Option<i32>) -> BlockData {
        BlockData {
            id,
            prompt: String::new(),
            cmd: cmd.to_string(),
            cmd_markup: None,
            output: format!("output-{id}"),
            exit_code,
            estimated_height: 1,
            line_count: 1,
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: None,
            cwd: None,
            cols: 80,
        }
    }

    fn block_with_height(height: i32) -> BlockData {
        let mut block = test_block(height as u64, "true", Some(0));
        block.estimated_height = height;
        block
    }

    #[test]
    fn transient_zero_viewport_keeps_the_last_visibility_set() {
        let blocks = VecDeque::from([block_with_height(20), block_with_height(20)]);
        assert!(viewport_state_for_scroll(&blocks, 0.0, 0.0, 1).is_none());
        assert!(viewport_state_for_scroll(&blocks, 0.0, 0.5, 1).is_none());
        assert!(viewport_state_for_scroll(&blocks, f64::NAN, 40.0, 1).is_none());
    }

    #[test]
    fn upper_only_adjustment_changes_do_not_recompute_visibility() {
        let last_page_size = Cell::new(None);
        assert!(viewport_page_size_changed(&last_page_size, 300.0));
        assert!(!viewport_page_size_changed(&last_page_size, 300.0));
        assert!(!viewport_page_size_changed(&last_page_size, 300.4));
        assert!(viewport_page_size_changed(&last_page_size, 301.0));
        assert!(!viewport_page_size_changed(&last_page_size, f64::NAN));
    }

    #[test]
    fn font_metric_changes_invalidate_finished_block_layout() {
        let before = finished_layout_key(800, 600, 16);
        assert_eq!(before, finished_layout_key(800, 600, 16));
        assert_ne!(before, finished_layout_key(800, 600, 18));
    }

    /// Pane resizes are the only thing that may re-fit the history. A command
    /// starting or ending grows and shrinks the live input cell, and a finished
    /// command appends a block — neither may move this key, because a moved key
    /// re-feeds every finished block's VTE and the whole history visibly blinks.
    #[test]
    fn running_a_command_does_not_invalidate_finished_block_layout() {
        let idle = finished_layout_key(800, 600, 16);
        // Same pane, same font: identical whatever the live cell or history is
        // doing. The key has no input-height or block-count component to move.
        assert_eq!(idle, finished_layout_key(800, 600, 16));
        // Real geometry still invalidates.
        assert_ne!(idle, finished_layout_key(800, 540, 16));
        assert_ne!(idle, finished_layout_key(780, 600, 16));
    }

    #[test]
    fn boundary_jitter_cannot_toggle_a_rendered_block() {
        let blocks: VecDeque<BlockData> =
            std::iter::repeat_n(20, 10).map(block_with_height).collect();
        let strict = viewport_state_for_scroll(&blocks, 120.0, 40.0, 1).unwrap();
        let loose = viewport_state_for_scroll(&blocks, 120.0, 40.0, 2).unwrap();
        assert_eq!(
            visible_indices_for_viewport(&strict),
            HashSet::from_iter(4..=9)
        );

        let current = HashSet::from_iter(2..=9);
        assert_eq!(
            stable_visible_indices(&strict, Some(&loose), &current),
            HashSet::from_iter(2..=9)
        );
        assert_eq!(
            stable_visible_indices(&strict, Some(&loose), &HashSet::from([0])),
            HashSet::from_iter(4..=9)
        );
        assert_eq!(
            stable_visible_indices(&strict, Some(&loose), &HashSet::new()),
            HashSet::from_iter(4..=9)
        );
    }

    #[test]
    fn combined_viewport_scan_matches_two_independent_scans_at_boundaries() {
        let blocks: VecDeque<BlockData> = (0..257)
            .map(|index| block_with_height(1 + index % 37))
            .collect();
        for scroll_top in [-50.0, 0.0, 1.0, 777.0, 4_800.0, f64::from(i32::MAX)] {
            for viewport_height in [1.0, 40.0, 800.0, f64::from(i32::MAX)] {
                for margin in [0, 1, 3, u32::MAX] {
                    let expected =
                        viewport_state_for_scroll(&blocks, scroll_top, viewport_height, margin)
                            .zip(viewport_state_for_scroll(
                                &blocks,
                                scroll_top,
                                viewport_height,
                                margin.saturating_add(1),
                            ));
                    let actual =
                        viewport_states_for_scroll(&blocks, scroll_top, viewport_height, margin);
                    match (actual, expected) {
                        (Some((actual_strict, actual_loose)), Some((strict, loose))) => {
                            assert_eq!(
                                (actual_strict.first_visible, actual_strict.last_visible),
                                (strict.first_visible, strict.last_visible)
                            );
                            assert_eq!(
                                (actual_loose.first_visible, actual_loose.last_visible),
                                (loose.first_visible, loose.last_visible)
                            );
                        }
                        (None, None) => {}
                        _ => panic!(
                            "combined/independent validity differed for top={scroll_top}, height={viewport_height}, margin={margin}"
                        ),
                    }
                }
            }
        }

        assert!(viewport_states_for_scroll(&blocks, f64::NAN, 40.0, 1).is_none());
        assert!(viewport_states_for_scroll(&blocks, 0.0, 0.5, 1).is_none());
        let empty = VecDeque::new();
        let (strict, loose) = viewport_states_for_scroll(&empty, 0.0, 40.0, 1).unwrap();
        assert_eq!((strict.first_visible, strict.last_visible), (0, 0));
        assert_eq!((loose.first_visible, loose.last_visible), (0, 0));
    }

    #[test]
    #[ignore = "micro-benchmark; run explicitly with --ignored --nocapture"]
    fn viewport_pair_scan_micro_benchmark() {
        let blocks: VecDeque<BlockData> = std::iter::repeat_n(20, 100_000)
            .map(block_with_height)
            .collect();
        let legacy_started = std::time::Instant::now();
        for _ in 0..256 {
            std::hint::black_box((
                viewport_state_for_scroll(&blocks, 1_999_000.0, 800.0, 1).unwrap(),
                viewport_state_for_scroll(&blocks, 1_999_000.0, 800.0, 2).unwrap(),
            ));
        }
        let legacy_elapsed = legacy_started.elapsed();

        let combined_started = std::time::Instant::now();
        for _ in 0..256 {
            std::hint::black_box(
                viewport_states_for_scroll(&blocks, 1_999_000.0, 800.0, 1).unwrap(),
            );
        }
        eprintln!(
            "viewport pair scan: legacy={legacy_elapsed:?}, combined={:?}",
            combined_started.elapsed()
        );
    }

    #[test]
    fn mounted_jumpability_intersects_both_surfaces_in_one_document_pass() {
        let candidates = HashSet::from([(2, false), (2, true), (3, true), (9, false)]);
        let visited = Cell::new(0);
        let jumpable = mounted_jumpable_records(
            [1, 2, 3, 4]
                .into_iter()
                .inspect(|_| visited.set(visited.get() + 1)),
            &candidates,
        );

        assert_eq!(jumpable, HashSet::from([(2, false), (2, true), (3, true)]));
        assert_eq!(visited.get(), 4, "each mounted block is visited once");
    }

    #[test]
    #[ignore = "micro-benchmark; run explicitly with --ignored --nocapture"]
    fn cross_block_jumpability_micro_benchmark() {
        const MOUNTED: u64 = 1_000;
        const MATCHING_RECORDS: u64 = 250;
        const REPETITIONS: usize = 250;

        let mounted: Vec<u64> = (0..MOUNTED).collect();
        let candidates: HashSet<(u64, bool)> = (MOUNTED - MATCHING_RECORDS..MOUNTED)
            .flat_map(|id| [(id, false), (id, true)])
            .collect();

        let legacy_started = Instant::now();
        for _ in 0..REPETITIONS {
            let found: HashSet<_> = candidates
                .iter()
                .copied()
                .filter(|(id, _)| mounted.iter().any(|mounted_id| mounted_id == id))
                .collect();
            std::hint::black_box(found);
        }
        let legacy = legacy_started.elapsed();

        let batched_started = Instant::now();
        for _ in 0..REPETITIONS {
            std::hint::black_box(mounted_jumpable_records(
                mounted.iter().copied(),
                &candidates,
            ));
        }
        let batched = batched_started.elapsed();

        eprintln!(
            "cross-block jumpability: legacy={legacy:?}, batched={batched:?}, speedup={:.1}x",
            legacy.as_secs_f64() / batched.as_secs_f64()
        );
    }

    #[test]
    fn one_pass_failure_markers_match_legacy_order_and_fraction_semantics() {
        let mut blocks = VecDeque::new();
        for id in 0..5_000_u64 {
            let failed = id % 7 == 0;
            let mut block = test_block(
                id,
                if failed { "false" } else { "true" },
                Some(if failed { 1 } else { 0 }),
            );
            block.estimated_height = match id % 5 {
                0 => -7,
                1 => 0,
                2 => 1,
                3 => 37,
                _ => i32::MAX,
            };
            blocks.push_back(block);
        }

        assert_eq!(
            failed_block_marker_fractions(&blocks),
            failed_block_marker_fractions_legacy(&blocks)
        );
        assert_eq!(
            failed_block_marker_fractions(&VecDeque::new()),
            failed_block_marker_fractions_legacy(&VecDeque::new())
        );
    }

    #[test]
    fn one_pass_failure_markers_keep_saturating_positions_and_newest_tail() {
        let saturated =
            failed_block_marker_fractions_from_entries([(u64::MAX, false), (1, true), (1, true)]);
        assert_eq!(saturated, [1.0, 1.0]);

        let newest = failed_block_marker_fractions_from_entries((0..1_025).map(|_| (1, true)));
        assert_eq!(newest.len(), 1_024);
        assert!((newest[0] - 1.0 / 1_025.0).abs() < f64::EPSILON);
        assert!((newest[1_023] - 1_024.0 / 1_025.0).abs() < f64::EPSILON);
    }

    #[test]
    #[ignore = "micro-benchmark; run explicitly with --ignored --nocapture"]
    fn failure_marker_single_pass_micro_benchmark() {
        const BLOCKS: u64 = 100_000;
        const REPETITIONS: usize = 64;
        let mut blocks: VecDeque<BlockData> = (0..BLOCKS)
            .map(|id| {
                let failed = id % 10 == 0;
                let mut block = test_block(
                    id,
                    if failed { "false" } else { "true" },
                    Some(if failed { 1 } else { 0 }),
                );
                block.estimated_height = 1 + (id % 97) as i32;
                block
            })
            .collect();
        // Exercise the height clamp without changing the retained population.
        blocks[0].estimated_height = i32::MIN;

        let legacy_started = Instant::now();
        for _ in 0..REPETITIONS {
            std::hint::black_box(failed_block_marker_fractions_legacy(&blocks));
        }
        let legacy = legacy_started.elapsed();

        let single_started = Instant::now();
        for _ in 0..REPETITIONS {
            std::hint::black_box(failed_block_marker_fractions(&blocks));
        }
        let single = single_started.elapsed();

        eprintln!(
            "failure markers ({BLOCKS} blocks, {REPETITIONS}x): legacy={legacy:?}, single={single:?}, speedup={:.2}x",
            legacy.as_secs_f64() / single.as_secs_f64()
        );
    }

    #[test]
    fn failure_markers_follow_weighted_history_positions() {
        let mut blocks = VecDeque::from([
            test_block(1, "true", Some(0)),
            test_block(2, "cargo test", Some(101)),
            test_block(3, "false", Some(1)),
        ]);
        blocks[0].estimated_height = 10;
        blocks[1].estimated_height = 30;
        blocks[2].estimated_height = 60;

        assert_eq!(failed_block_marker_fractions(&blocks), vec![0.1, 0.4]);
    }

    #[test]
    fn failure_markers_share_block_status_rules_and_keep_a_bounded_tail() {
        let non_failures = VecDeque::from([
            // Background output is never a failed command, even if a legacy or
            // synthetic record happens to carry a non-zero status.
            test_block(1, "", Some(1)),
            test_block(2, "status-unreported", None),
        ]);
        assert!(failed_block_marker_fractions(&non_failures).is_empty());

        let failures: VecDeque<_> = (0..1025)
            .map(|id| test_block(id, "false", Some(1)))
            .collect();
        let markers = failed_block_marker_fractions(&failures);
        assert_eq!(markers.len(), 1024);
        assert!((markers[0] - 1.0 / 1025.0).abs() < f64::EPSILON);
        assert!((markers[1023] - 1024.0 / 1025.0).abs() < f64::EPSILON);
    }

    #[test]
    fn block_data_mutation_queues_marker_redraw_after_releasing_the_borrow() {
        let blocks = RefCell::new(VecDeque::new());
        let observed_len = Cell::new(0);
        let redraw = || observed_len.set(blocks.borrow().len());

        mutate_block_data_and_redraw(&blocks, &redraw, |blocks| {
            blocks.push_back(test_block(1, "false", Some(1)));
        });

        assert_eq!(observed_len.get(), 1);
    }

    #[test]
    fn marked_index_stepping_wraps_in_both_directions() {
        let marked = [2usize, 5, 9];
        // No current selection: latest-first semantics per direction.
        assert_eq!(step_marked_indices(&marked, None, 1), Some(2));
        assert_eq!(step_marked_indices(&marked, None, -1), Some(9));
        // Strictly next/previous relative to the current block.
        assert_eq!(step_marked_indices(&marked, Some(5), 1), Some(9));
        assert_eq!(step_marked_indices(&marked, Some(5), -1), Some(2));
        // Wrap at the ends.
        assert_eq!(step_marked_indices(&marked, Some(9), 1), Some(2));
        assert_eq!(step_marked_indices(&marked, Some(2), -1), Some(9));
        // Nothing marked -> nowhere to go.
        assert_eq!(step_marked_indices(&[], Some(3), 1), None);
    }

    #[test]
    fn marked_record_stepping_uses_document_order_not_numeric_identity() {
        let records = [900, 3, 450, 17];
        let marked = [450, 900];
        assert_eq!(
            step_marked_record_ids(&records, &marked, Some(900), 1),
            Some(450)
        );
        assert_eq!(
            step_marked_record_ids(&records, &marked, Some(450), -1),
            Some(900)
        );
        assert_eq!(
            step_marked_record_ids(&records, &marked, Some(3), 1),
            Some(450)
        );
    }

    #[test]
    fn selection_markdown_keeps_terminal_order_and_falls_back_to_clicked_block() {
        let blocks = [
            test_block(1, "git status", Some(0)),
            test_block(2, "cargo test", Some(101)),
            test_block(3, "git push", Some(0)),
        ];
        let selected = HashSet::from([3, 1]);
        let markdown = selected_blocks_markdown(blocks.iter(), &selected, 1);
        let first = markdown.find("git status").expect("older block present");
        let second = markdown.find("git push").expect("newer block present");
        assert!(first < second, "blocks must keep terminal order");
        assert!(!markdown.contains("cargo test"));
        assert!(markdown.contains("---"));

        // Right-click without a registered selection still copies that block.
        let fallback = selected_blocks_markdown(blocks.iter(), &HashSet::new(), 2);
        assert!(fallback.contains("cargo test"));
        assert!(fallback.contains("**Exit Code:** 101"));
        assert!(!fallback.contains("---"));
    }

    #[test]
    fn selected_commands_keep_terminal_order_and_skip_background_blocks() {
        let selected = HashSet::from([30, 10, 20]);
        let blocks = [
            (10, "git status"),
            (20, ""),
            (30, "cargo test"),
            (40, "git push"),
        ];
        assert_eq!(
            selected_command_text(blocks, &selected),
            "git status\ncargo test"
        );
    }

    #[test]
    fn selected_commands_preserve_multiline_commands() {
        let selected = HashSet::from([7, 8]);
        let blocks = [(7, "printf 'a\\n'\nprintf 'b\\n'"), (8, "echo done")];
        assert_eq!(
            selected_command_text(blocks, &selected),
            "printf 'a\\n'\nprintf 'b\\n'\necho done"
        );
    }

    #[test]
    fn selected_command_aggregation_rejects_the_whole_oversized_selection() {
        let selected = HashSet::from([7, 8]);
        let first = "x".repeat(MAX_RECALLED_COMMAND_BYTES / 2);
        let second = "y".repeat(MAX_RECALLED_COMMAND_BYTES / 2 + 1);
        assert!(
            selected_command_text([(7, first.as_str()), (8, second.as_str())], &selected)
                .is_empty()
        );
    }

    #[test]
    fn bounded_text_tail_preserves_utf8_and_newest_prompt_content() {
        let mut buffer = "old-界".to_string();
        append_bounded_text_tail(&mut buffer, "-new-终", 9);
        assert!(buffer.len() <= 9);
        assert!(buffer.ends_with("new-终"));

        append_bounded_text_tail(&mut buffer, "abcdef界", 4);
        assert_eq!(buffer, "f界");
    }

    #[test]
    fn oversized_typed_shadow_stays_nonempty_through_backspaces() {
        let mut shadow = "x".repeat(MAX_TYPED_COMMAND_SHADOW_BYTES);
        append_typed_command_shadow(&mut shadow, "y");
        assert_eq!(shadow, TRUNCATED_COMMAND_PLACEHOLDER);
        for _ in 0..TRUNCATED_COMMAND_PLACEHOLDER.chars().count() + 1 {
            pop_typed_command_shadow(&mut shadow);
        }
        assert_eq!(shadow, TRUNCATED_COMMAND_PLACEHOLDER);
    }

    #[test]
    fn bounded_section_is_atomic_at_the_aggregate_limit() {
        let mut output = "first".to_string();
        assert!(append_bounded_section(&mut output, "|", "two", 9));
        assert_eq!(output, "first|two");
        assert!(!append_bounded_section(&mut output, "|", "x", 9));
        assert_eq!(output, "first|two");
    }

    #[test]
    fn coalesce_preserves_boundary_events_in_order() {
        let mut events = vec![
            ParserEvent::Bytes(b"$ ".to_vec()),
            ParserEvent::PromptEnd,
            ParserEvent::Bytes(b"ls".to_vec()),
            ParserEvent::Bytes(b" -la".to_vec()),
            ParserEvent::CommandStart(CommandMeta::default()),
            ParserEvent::Bytes(b"file1\n".to_vec()),
            ParserEvent::Bytes(b"file2\n".to_vec()),
            ParserEvent::CommandEnd {
                exit: Some(0),
                meta: CommandMeta::default(),
            },
            ParserEvent::PromptStart,
        ];
        coalesce_bytes_events(&mut events);
        assert_eq!(
            ev_summary(&events),
            vec![
                "B($ )",
                "PE",
                "B(ls -la)",
                "CS",
                "B(file1\nfile2\n)",
                "CE(Some(0))",
                "PS",
            ]
        );
    }

    #[test]
    fn coalesce_noop_on_empty_or_single() {
        let mut empty: Vec<ParserEvent> = Vec::new();
        coalesce_bytes_events(&mut empty);
        assert!(empty.is_empty());

        let mut one = vec![ParserEvent::Bytes(b"x".to_vec())];
        coalesce_bytes_events(&mut one);
        assert_eq!(ev_summary(&one), vec!["B(x)"]);

        let mut one_boundary = vec![ParserEvent::PromptStart];
        coalesce_bytes_events(&mut one_boundary);
        assert_eq!(ev_summary(&one_boundary), vec!["PS"]);
    }

    #[test]
    fn coalesce_handles_only_boundary_events() {
        let mut events = vec![
            ParserEvent::PromptStart,
            ParserEvent::PromptEnd,
            ParserEvent::CommandStart(CommandMeta::default()),
            ParserEvent::CommandEnd {
                exit: None,
                meta: CommandMeta::default(),
            },
        ];
        coalesce_bytes_events(&mut events);
        // A shell that sent no status renders as `None`, not as a zero.
        assert_eq!(ev_summary(&events), vec!["PS", "PE", "CS", "CE(None)"]);
    }

    #[test]
    fn strips_charset_designation_from_output() {
        assert_eq!(strip_ansi("\u{1b}(Btop"), "top");
    }

    #[test]
    fn cursor_home_and_partial_erase_do_not_clear_block_output() {
        assert_eq!(
            strip_ansi_with_clear_detect("\u{1b}[Hgit output"),
            ("git output".to_string(), false)
        );
        assert_eq!(
            strip_ansi_with_clear_detect("\u{1b}[Jgit output"),
            ("git output".to_string(), false)
        );
        assert_eq!(
            strip_ansi_with_clear_detect("\u{1b}[2Jfresh"),
            ("fresh".to_string(), true)
        );
    }

    // ── strip_ansi_with_clear_detect: cursor model tests ────────────────

    #[test]
    fn carriage_return_overwrites_line() {
        // \r moves cursor to col 0, shorter text overwrites prefix but leaves tail
        assert_eq!(
            strip_ansi_with_clear_detect("Loading...\rDone!"),
            ("Done!ng...".to_string(), false)
        );
    }

    #[test]
    fn carriage_return_full_overwrite() {
        // Full overwrite of same-length text
        assert_eq!(
            strip_ansi_with_clear_detect("AAAA\rBBBB"),
            ("BBBB".to_string(), false)
        );
    }

    #[test]
    fn spinner_animation_shows_final_frame() {
        // Simulates spinner: multiple frames separated by \r
        assert_eq!(
            strip_ansi_with_clear_detect("| working\r/ working\r- working\r\\ working"),
            ("\\ working".to_string(), false)
        );
    }

    #[test]
    fn csi_erase_line_to_end() {
        // CSI 0K: erase from cursor to end of line
        assert_eq!(
            strip_ansi_with_clear_detect("hello world\r\u{1b}[0Kdone"),
            ("done".to_string(), false)
        );
    }

    #[test]
    fn csi_erase_line_implicit_zero() {
        // CSI K (no param) is same as CSI 0K
        assert_eq!(
            strip_ansi_with_clear_detect("old text\r\u{1b}[Knew"),
            ("new".to_string(), false)
        );
    }

    #[test]
    fn csi_erase_line_from_start() {
        // CSI 1K: erase from start to cursor (fills with spaces)
        assert_eq!(
            strip_ansi_with_clear_detect("abcdef\r\u{1b}[3C\u{1b}[1K"),
            ("   def".to_string(), false)
        );
    }

    #[test]
    fn csi_erase_entire_line() {
        // CSI 2K: erase entire line
        assert_eq!(
            strip_ansi_with_clear_detect("something\r\u{1b}[2Kresult"),
            ("result".to_string(), false)
        );
    }

    #[test]
    fn csi_cursor_forward() {
        // CSI C: move cursor forward
        assert_eq!(
            strip_ansi_with_clear_detect("abcdef\r\u{1b}[3CX"),
            ("abcXef".to_string(), false)
        );
    }

    #[test]
    fn csi_cursor_backward() {
        // CSI D: move cursor backward
        assert_eq!(
            strip_ansi_with_clear_detect("abcdef\u{1b}[2DXY"),
            ("abcdXY".to_string(), false)
        );
    }

    #[test]
    fn csi_cursor_absolute_column() {
        // CSI G: absolute column positioning (1-based)
        assert_eq!(
            strip_ansi_with_clear_detect("abcdef\u{1b}[2GX"),
            ("aXcdef".to_string(), false)
        );
    }

    #[test]
    fn backspace_moves_cursor_back() {
        assert_eq!(
            strip_ansi_with_clear_detect("abc\x08X"),
            ("abX".to_string(), false)
        );
    }

    #[test]
    fn backspace_at_start_does_not_underflow() {
        assert_eq!(
            strip_ansi_with_clear_detect("\x08\x08hello"),
            ("hello".to_string(), false)
        );
    }

    #[test]
    fn claude_code_progress_pattern() {
        // Claude Code CLI pattern: write progress, \r, erase line, write new status
        let input = "⠋ Thinking...\r\u{1b}[K⠙ Analyzing...\r\u{1b}[K✓ Done";
        assert_eq!(
            strip_ansi_with_clear_detect(input),
            ("✓ Done".to_string(), false)
        );
    }

    #[test]
    fn unicode_overwrite_preserves_chars() {
        // CJK characters with cursor moves
        assert_eq!(
            strip_ansi_with_clear_detect("你好世界\r\u{1b}[2C再"),
            ("你好再界".to_string(), false)
        );
    }

    #[test]
    fn mixed_ansi_colors_stripped_correctly() {
        // Colored text with cursor movement should strip colors and handle cursor
        assert_eq!(
            strip_ansi_with_clear_detect("\u{1b}[32mhello\u{1b}[0m\rbye"),
            ("byelo".to_string(), false)
        );
    }

    #[test]
    fn clear_screen_still_detected() {
        // CSI 2J and 3J still trigger clear
        assert_eq!(
            strip_ansi_with_clear_detect("\u{1b}[2J"),
            ("".to_string(), true)
        );
        assert_eq!(
            strip_ansi_with_clear_detect("\u{1b}[3J"),
            ("".to_string(), true)
        );
        // CSI 0J / CSI 1J do not trigger clear
        assert_eq!(
            strip_ansi_with_clear_detect("\u{1b}[0J"),
            ("".to_string(), false)
        );
    }

    // ── IME / Chinese input support tests ────────────────────────────────

    /// Simulate the logic from connect_commit: insert text at cursor position
    fn simulate_ime_commit(cmd: &str, cursor_pos: usize, committed: &str) -> (String, usize) {
        let mut buf = cmd.to_string();
        let byte_pos = buf
            .char_indices()
            .nth(cursor_pos)
            .map(|(i, _)| i)
            .unwrap_or(buf.len());
        buf.insert_str(byte_pos, committed);
        let new_pos = cursor_pos + committed.chars().count();
        (buf, new_pos)
    }

    #[test]
    fn ime_commit_chinese_at_end() {
        let (buf, pos) = simulate_ime_commit("ls ", 3, "你好");
        assert_eq!(buf, "ls 你好");
        assert_eq!(pos, 5);
    }

    #[test]
    fn ime_commit_chinese_at_beginning() {
        let (buf, pos) = simulate_ime_commit("hello", 0, "世界");
        assert_eq!(buf, "世界hello");
        assert_eq!(pos, 2);
    }

    #[test]
    fn ime_commit_chinese_in_middle() {
        let (buf, pos) = simulate_ime_commit("echo test", 5, "中文");
        assert_eq!(buf, "echo 中文test");
        assert_eq!(pos, 7);
    }

    #[test]
    fn ime_commit_after_existing_chinese() {
        let (buf, pos) = simulate_ime_commit("你好", 2, "世界");
        assert_eq!(buf, "你好世界");
        assert_eq!(pos, 4);
    }

    #[test]
    fn ime_commit_mixed_cjk_ascii() {
        let (buf, pos) = simulate_ime_commit("git commit -m \"", 15, "修复bug");
        assert_eq!(buf, "git commit -m \"修复bug");
        // 修复bug = 5 chars (修,复,b,u,g), so pos = 15 + 5 = 20
        assert_eq!(pos, 20);
    }

    #[test]
    fn ime_preedit_cursor_position() {
        // During composition, cursor should be after cmd + preedit
        let cmd = "echo ";
        let preedit = "niha"; // pinyin input not yet committed
        let cursor_pos = cmd.chars().count() + preedit.chars().count();
        assert_eq!(cursor_pos, 9);
    }

    #[test]
    fn ime_preedit_buffer_format() {
        // The display buffer format: "{cmd}{preedit} {suggestion}"
        let cmd = "echo ";
        let preedit = "你好";
        let suggestion = "";
        let text = format!("{}{} {}", cmd, preedit, suggestion);
        assert_eq!(text, "echo 你好 ");
        // Preedit tag range: cmd.chars().count() .. cmd.chars().count() + preedit.chars().count()
        let preedit_start = cmd.chars().count();
        let preedit_end = preedit_start + preedit.chars().count();
        assert_eq!(preedit_start, 5);
        assert_eq!(preedit_end, 7);
    }

    #[test]
    fn ime_commit_clears_preedit_state() {
        // After commit, preedit should be empty and cursor advances
        let cmd = "ls ";
        let _preedit = "zhong"; // composing
                                // Simulate commit of "中"
        let (buf, pos) = simulate_ime_commit(cmd, cmd.chars().count(), "中");
        assert_eq!(buf, "ls 中");
        assert_eq!(pos, 4);
        // preedit should be cleared (tested by set_preedit("") after commit)
        let final_preedit = "";
        let display = format!("{} {}", buf, final_preedit);
        assert_eq!(display, "ls 中 ");
    }

    #[test]
    fn ime_backspace_chinese_char() {
        // Backspace should delete one full CJK character
        let cmd = "你好世界";
        let pos = 4; // cursor at end
        let mut buf = cmd.to_string();
        let byte_pos = buf.char_indices().nth(pos - 1).map(|(i, _)| i).unwrap_or(0);
        let next_byte = buf
            .char_indices()
            .nth(pos)
            .map(|(i, _)| i)
            .unwrap_or(buf.len());
        buf.drain(byte_pos..next_byte);
        assert_eq!(buf, "你好世");
        assert_eq!(buf.chars().count(), 3);
    }

    #[test]
    fn ime_cursor_movement_with_chinese() {
        // Left/right should move by one char (not byte)
        let cmd = "你好world";
        let chars: Vec<char> = cmd.chars().collect();
        assert_eq!(chars.len(), 7); // 你好 = 2 chars, world = 5 chars
                                    // At pos 2, cursor is between '好' and 'w'
        let pos = 2;
        assert_eq!(chars[pos - 1], '好');
        assert_eq!(chars[pos], 'w');
    }

    #[test]
    fn agent_execution_requires_same_prompt_and_surfaces_command_mismatch() {
        let execution = AgentExecutionRef {
            epoch: AgentSession::new(1, 2, 1).epoch(),
            generation: 41,
        };
        let armed = || {
            Some(ArmedAgentExecution {
                execution,
                prompt_generation: 7,
            })
        };

        let mut exact = armed();
        assert_eq!(take_armed_agent_execution(&mut exact, 7), Some(execution));
        assert!(exact.is_none(), "a matching execution is one-shot");

        let mut suffix_collision = armed();
        assert_eq!(
            take_armed_agent_execution(&mut suffix_collision, 7),
            Some(execution)
        );
        assert!(
            suffix_collision.is_none(),
            "the command-text check happens at BlockFinished, but the arm is one-shot"
        );

        let mut stale_prompt = armed();
        assert_eq!(take_armed_agent_execution(&mut stale_prompt, 8), None);
        assert!(stale_prompt.is_none());

        assert!(command_end_matches_started_id(
            Some("secret-1"),
            Some("secret-1")
        ));
        assert!(!command_end_matches_started_id(
            Some("secret-1"),
            Some("nested-2")
        ));
        assert!(!agent_prompt_boundary_is_trusted(
            Some(execution),
            Some(false)
        ));
        assert!(agent_prompt_boundary_is_trusted(
            Some(execution),
            Some(true)
        ));
        assert!(!agent_prompt_boundary_is_trusted(Some(execution), None));
        assert!(agent_prompt_boundary_is_trusted(None, Some(false)));
    }
}
