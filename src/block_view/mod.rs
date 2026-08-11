use gtk::gdk::RGBA;
use gtk::pango::FontDescription;
use gtk::prelude::*;
use gtk::{glib, Orientation, ScrolledWindow};
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};
use vte4::Terminal;
use vte4::TerminalExt;

use crate::config::Config;
use crate::parser::{ColorKind, KeyboardProtocolQuery, Parser, ParserConfig, ParserEvent};
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

// Global block ID counter
static BLOCK_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

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

fn next_block_id() -> u64 {
    BLOCK_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Approximate the vertical positions of failed finished blocks within the
/// complete scrollback history. The bounded tail caps the number of Cairo marks
/// for very long sessions while preserving the newest failures, which are
/// usually the most useful navigation hints. Computing positions still scans
/// the retained block metadata once.
fn failed_block_marker_fractions(blocks: &VecDeque<BlockData>) -> Vec<f64> {
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

#[derive(Default)]
struct ShellCapabilityObserver {
    state: CapabilityOscState,
    collecting_prompt: bool,
}

#[derive(Default)]
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
    /// This exact-pinned jterm_core predates OSC 7771, so Anvil consumes only
    /// this hidden capability packet locally while the normal parser continues
    /// to own every display/lifecycle event.
    fn feed(&mut self, bytes: &[u8], expected: &str, ready: &Cell<bool>) {
        for &byte in bytes {
            let state = std::mem::take(&mut self.state);
            self.state = match state {
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
        && !command
            .chars()
            .any(|ch| !ch.is_control() && crate::text_safety::is_visual_spoof(ch))
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

/// `(command, exit status, output sample, Agent execution, duration ms)`. The
/// status is `None` when the shell reported none, so a consumer that styles
/// failures can tell "failed" apart from "outcome unknown".
type BlockFinishedCallbacks = Rc<
    RefCell<
        Vec<
            Box<
                dyn Fn(
                    String,
                    Option<i32>,
                    String,
                    Option<crate::agent::AgentExecutionRef>,
                    Option<u64>,
                ),
            >,
        >,
    >,
>;
type BlockContextCallbacks = Rc<RefCell<Vec<Box<dyn Fn(crate::ai::BlockContext)>>>>;
type CwdCallbacks = Rc<RefCell<Vec<Box<dyn Fn(&str, bool)>>>>;
type AgentExecutionLostCallbacks =
    Rc<RefCell<Vec<Box<dyn Fn(crate::agent::AgentExecutionRef, &'static str)>>>>;
type CommandStartedCallbacks = Rc<RefCell<Vec<Box<dyn Fn(CommandStartedEvent)>>>>;
type CommandFinishedCallbacks = Rc<RefCell<Vec<Box<dyn Fn(CommandFinishedEvent)>>>>;
type HumanInputCallbacks = Rc<RefCell<Vec<Box<dyn Fn(HumanInputKind)>>>>;

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
    active_vte: Terminal,
    bstate: Rc<Cell<BlockState>>,
    pty: Rc<OwnedPty>,
    typed_cmd: Rc<RefCell<String>>,
    idle_input_dirty: Rc<Cell<bool>>,
    pty_synced: Rc<Cell<bool>>,
    prompt_end_pos: Rc<Cell<(i64, i64)>>,
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
        let anchor = self.prompt_end_pos.get();
        if self.active_vte.cursor_position() != anchor
            || crate::terminal::click_cursor::verified_suffix_is_empty(&self.active_vte)
                != Some(true)
        {
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
            let (col, row) = ctx.active_vte.cursor_position();
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

            let rendered = visible_editor_text(&ctx.active_vte, ctx.prompt_end_pos.get());
            let suffix_empty =
                crate::terminal::click_cursor::verified_suffix_is_empty(&ctx.active_vte);
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
    /// The single persistent live VTE (anvil model): prompt + typing + output all
    /// render here natively; finished commands snapshot into styled blocks above.
    active_vte: Terminal,
    active: Rc<RefCell<ActiveBlock>>,
    bstate: Rc<Cell<BlockState>>,
    prompt_buf: Rc<RefCell<String>>,
    /// Keystroke shadow used only to size the idle input cell (line count). The
    /// authoritative finished-command text is read off the live VTE at
    /// CommandStart, so this never has to round-trip to display.
    typed_cmd: Rc<RefCell<String>>,
    /// Immutable PromptEnd cursor anchor used for empty-editor verification.
    prompt_end_pos: Rc<Cell<(i64, i64)>>,
    /// VTE feed is asynchronous; approval remains unavailable until a short
    /// post-PromptEnd fence confirms that no input raced the captured anchor.
    prompt_anchor_ready: Rc<Cell<bool>>,
    /// One-shot Agent execution identity armed atomically with its PTY write.
    /// It follows only the next command at the same prompt generation; the
    /// Agent session performs the secondary command-text check at completion.
    armed_agent_execution: Rc<RefCell<Option<ArmedAgentExecution>>>,
    agent_prompt_generation: Rc<Cell<u64>>,
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
    /// Queue a repaint whenever block metadata changes, even when GTK's scroll
    /// adjustment geometry happens to remain numerically identical.
    failure_marker_redraw: FailureMarkerRedraw,
    finished_blocks: Rc<RefCell<Vec<FinishedBlock>>>,
    viewport: Rc<RefCell<ViewportState>>,
    widget_pool: Rc<RefCell<WidgetPool>>,
    visible_indices: Rc<RefCell<std::collections::HashSet<usize>>>,
    selected_block_ids: SelectedBlockIds,
    selected_block_id: Rc<Cell<Option<u64>>>,
    selection_anchor_id: Rc<Cell<Option<u64>>>,
    bookmarks: Rc<RefCell<std::collections::HashSet<u64>>>,
    /// Blocks removed by the most recent Clear Blocks, kept as data so an
    /// explicit undo can rebuild their widgets. Single-level: a later clear
    /// with content replaces it; cleared again only when consumed by undo.
    cleared_stash: RefCell<Vec<BlockData>>,
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
}

impl Drop for TermView {
    fn drop(&mut self) {
        if let Err(err) = self.save_history() {
            log::warn!("save block history on close: {err}");
        }
        self.forget_history_revision();
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

/// Captures the shared handles the PTY reader/exit callbacks need, so
/// `TermView::new` does not carry the reader closure inline.
struct ReaderCtx {
    active_rc: Rc<RefCell<ActiveBlock>>,
    /// The live VTE — every byte is fed here; alt-screen toggles feed it 1049h/l.
    active_vte: Terminal,
    bstate_rc: Rc<Cell<BlockState>>,
    /// State to restore when an alt-screen app exits (anvil model).
    prev_state_rc: Rc<Cell<BlockState>>,
    osc133_depth_rc: Rc<Cell<u32>>,
    prompt_buf_rc: Rc<RefCell<String>>,
    /// Keystroke-shadow input line, used only as a fallback if the VTE-text
    /// capture at CommandStart returns empty.
    typed_cmd_rc: Rc<RefCell<String>>,
    /// Bytes emitted asynchronously after PromptEnd and before the next PromptStart.
    /// Empty-command blocks are inferred from this separate buffer, so no history
    /// schema change is needed.
    background_output_rc: Rc<RefCell<VecDeque<u8>>>,
    /// Once the user starts editing at an idle prompt, output is intentionally left
    /// inline: shell echo/completion and true background output are ambiguous then.
    idle_input_dirty_rc: Rc<Cell<bool>>,
    /// Command text read from the live VTE at CommandStart; primary source
    /// for the finished block.
    vte_typed_cmd_rc: Rc<RefCell<String>>,
    /// VTE cursor position (col, row) captured at PromptEnd; the start anchor
    /// for the text-range read that produces `vte_typed_cmd_rc`.
    prompt_end_pos_rc: Rc<Cell<(i64, i64)>>,
    prompt_anchor_ready_rc: Rc<Cell<bool>>,
    /// Rendered prompt (last non-empty line) captured at PromptEnd, used by the
    /// finalize path since prompt_buf is cleared once the prompt ends.
    prompt_display_rc: Rc<RefCell<String>>,
    block_list_rc: gtk::Box,
    block_scroll_rc: ScrolledWindow,
    remote_session_cbs: StrCallbacks,
    exited_cbs: IntCallbacks,
    activity_cbs: VoidCallbacks,
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
    block_data_for_cb: Rc<RefCell<VecDeque<BlockData>>>,
    failure_marker_redraw: FailureMarkerRedraw,
    finished_blocks_for_cb: Rc<RefCell<Vec<FinishedBlock>>>,
    scroll_debouncer: ScrollDebouncer,
    widget_pool_for_cb: Rc<RefCell<WidgetPool>>,
    pty_synced_rc: Rc<Cell<bool>>,
    visible_indices_rc: Rc<RefCell<std::collections::HashSet<usize>>>,
    fullscreen_rc: Rc<Cell<bool>>,
    ftcs_seen_rc: Rc<Cell<bool>>,
    init_cmds_queue_for_cb: Rc<RefCell<std::collections::VecDeque<String>>>,
    pty_for_init: Rc<OwnedPty>,
    block_start_time_for_cb: Rc<Cell<Option<SystemTime>>>,
    /// Status from the shell's OSC 133 `D` packet. `None` means the shell did
    /// not report one — not that the command succeeded.
    pending_exit_code_rc: Rc<Cell<Option<i32>>>,
    /// Duration the shell measured for the running command, when it sends one.
    /// Beats `block_start_time_for_cb`, which starts when this process noticed
    /// the mark.
    shell_duration_ms_rc: Rc<Cell<Option<u64>>>,
    /// The shell's execution id for the running command (jsh only): the key its
    /// execution journal keeps the record under.
    execution_id_rc: Rc<RefCell<Option<String>>>,
    execution_id_trusted_rc: Rc<Cell<bool>>,
    agent_completion_trusted_rc: Rc<Cell<bool>>,
    /// cwd the running command was started in, as the shell reported it at
    /// CommandStart. The pane's tracked cwd has already moved on after a `cd`.
    command_cwd_rc: Rc<RefCell<Option<String>>>,
    current_cwd_for_cb: Rc<RefCell<String>>,
    event_buf: Rc<RefCell<Vec<ParserEvent>>>,
    unread_count_rc: Rc<Cell<u32>>,
    jump_fab: gtk::Button,
    selected_block_ids_rc: SelectedBlockIds,
    selected_block_id_rc: Rc<Cell<Option<u64>>>,
    selection_anchor_id_rc: Rc<Cell<Option<u64>>>,
    bookmarks_rc: Rc<RefCell<std::collections::HashSet<u64>>>,
    cmd_running_rc: Rc<Cell<bool>>,
    running_cmd_rc: Rc<RefCell<String>>,
    armed_agent_execution_rc: Rc<RefCell<Option<ArmedAgentExecution>>>,
    agent_prompt_generation_rc: Rc<Cell<u64>>,
    active_agent_execution_rc: Rc<Cell<Option<crate::agent::AgentExecutionRef>>>,
    agent_execution_supported_rc: Rc<Cell<bool>>,
    verified_submission: VerifiedSubmissionCtx,
    /// Recomputes the compact/full visual live surface. PTY geometry is kept
    /// separately at the full pane viewport.
    layout_active_surface: Rc<dyn Fn()>,
    block_finished_cbs: BlockFinishedCallbacks,
    ask_ai_about_block_cbs: BlockContextCallbacks,
    selection_feed_hold: Rc<SelectionFeedHold>,
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
    let text = String::from_utf8_lossy(bytes);
    strip_ansi(text.as_ref())
        .chars()
        .any(|ch| !ch.is_whitespace() && !ch.is_control())
}

fn take_background_output(pending: &RefCell<VecDeque<u8>>) -> Option<String> {
    let mut pending = pending.borrow_mut();
    if pending.is_empty() {
        return None;
    }
    let output = {
        let bytes = pending.make_contiguous();
        background_output_has_visible_text(bytes)
            .then(|| String::from_utf8_lossy(bytes).into_owned())
    };
    pending.clear();
    output
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

impl ReaderCtx {
    fn install(self, pty: &Rc<OwnedPty>) -> std::io::Result<()> {
        let ReaderCtx {
            active_rc,
            active_vte,
            bstate_rc,
            prev_state_rc,
            osc133_depth_rc,
            prompt_buf_rc,
            typed_cmd_rc,
            background_output_rc,
            idle_input_dirty_rc,
            vte_typed_cmd_rc,
            prompt_end_pos_rc,
            prompt_anchor_ready_rc,
            prompt_display_rc,
            block_list_rc,
            block_scroll_rc,
            remote_session_cbs,
            exited_cbs,
            activity_cbs,
            command_started_cbs,
            command_finished_cbs,
            mouse_reporting_rc,
            bracketed_paste_rc,
            dynamic_colors_rc,
            config_for_cb,
            parser,
            block_data_for_cb,
            failure_marker_redraw,
            finished_blocks_for_cb,
            scroll_debouncer,
            widget_pool_for_cb,
            pty_synced_rc,
            visible_indices_rc,
            fullscreen_rc,
            ftcs_seen_rc,
            init_cmds_queue_for_cb,
            pty_for_init,
            block_start_time_for_cb,
            pending_exit_code_rc,
            shell_duration_ms_rc,
            execution_id_rc,
            execution_id_trusted_rc,
            agent_completion_trusted_rc,
            command_cwd_rc,
            current_cwd_for_cb,
            event_buf,
            unread_count_rc,
            jump_fab,
            selected_block_ids_rc,
            selected_block_id_rc,
            selection_anchor_id_rc,
            bookmarks_rc,
            cmd_running_rc,
            running_cmd_rc,
            armed_agent_execution_rc,
            agent_prompt_generation_rc,
            active_agent_execution_rc,
            agent_execution_supported_rc,
            verified_submission,
            layout_active_surface,
            block_finished_cbs,
            ask_ai_about_block_cbs,
            selection_feed_hold,
        } = self;
        let active_alt_screen_mode_rc: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));
        // Kitty graphics (APC G) — multi-chunk uploads assemble here; completed
        // textures wait against the running command until its block finishes.
        // The byte counter enforces the shared per-block budget so a runaway
        // shell cannot balloon RSS between prompts.
        let kitty_assembler_rc: Rc<RefCell<kitty_graphics::Assembler>> =
            Rc::new(RefCell::new(kitty_graphics::Assembler::new()));
        let kitty_pending_images_rc: Rc<RefCell<Vec<gtk::gdk::Texture>>> =
            Rc::new(RefCell::new(Vec::new()));
        let kitty_pending_bytes_rc: Rc<Cell<usize>> = Rc::new(Cell::new(0));
        let shell_token = pty
            .shell_integration_token()
            .unwrap_or_default()
            .to_string();
        let mut capability_observer = ShellCapabilityObserver::default();
        selection_feed_hold.install_vte_hooks(&active_vte);

        // Keep the complete security-observer → parser → Block state machine
        // behind one replayable closure. A selection hold intercepts raw chunks
        // before this boundary and flushes them back through this exact path.
        let process_chunk: Rc<RefCell<dyn FnMut(Vec<u8>)>> = Rc::new(RefCell::new(
            move |data: Vec<u8>| {
                capability_observer.feed(&data, &shell_token, &agent_execution_supported_rc);
                let mut events = event_buf.borrow_mut();
                events.clear();
                parser.borrow_mut().feed(&data, &mut events);
                // Fold runs of consecutive `Bytes` events into one so the live
                // VTE feed, autoscroll mark-dirty, and accumulate_output happen
                // once per stretch instead of once per parser chunk. Boundary
                // events (PromptStart/End, AltScreen*, CommandStart/End) still
                // run their synchronous mark_dirty between stretches, keeping
                // the scroll-invariant from [[scroll_synchronous_autoscroll]].
                coalesce_bytes_events(&mut events);

                for event in events.iter() {
                    let state = bstate_rc.get();
                    match event {
                        ParserEvent::RemoteSessionId(id) => {
                            for cb in remote_session_cbs.borrow().iter() {
                                cb(id);
                            }
                        }
                        ParserEvent::DecsetMode { mode, set } => {
                            if *mode == 2004 {
                                bracketed_paste_rc.set(*set);
                            }
                            // VTE handles paste/cursor/etc. natively from its
                            // own bytes; block_view only needs mouse-reporting
                            // state for wheel suppression in alt-screen apps.
                            let new_mode = match (*mode, *set) {
                                (1000, true) => Some(MouseReportingMode::Click),
                                (1002, true) => Some(MouseReportingMode::Button),
                                (1003, true) => Some(MouseReportingMode::Motion),
                                (1006, true) => Some(MouseReportingMode::Sgr),
                                (1000 | 1002 | 1003 | 1006, false) => {
                                    Some(MouseReportingMode::None)
                                }
                                _ => None,
                            };
                            if let Some(m) = new_mode {
                                mouse_reporting_rc.set(m);
                            }
                        }
                        ParserEvent::Bytes(bytes) => {
                            if state == BlockState::AwaitingCommand {
                                if let Some(submission) =
                                    verified_submission.submission.borrow_mut().as_mut()
                                {
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
                                bstate_rc.set(BlockState::RawFallback);
                            }

                            let feed_active_vte = match bstate_rc.get() {
                                BlockState::CollectingPrompt => {
                                    let text = String::from_utf8_lossy(bytes);
                                    append_bounded_text_tail(
                                        &mut prompt_buf_rc.borrow_mut(),
                                        &text,
                                        MAX_PROMPT_CAPTURE_BYTES,
                                    );
                                    scroll_debouncer.mark_dirty(&block_scroll_rc);
                                    true
                                }
                                BlockState::AwaitingCommand => {
                                    // Warp separates asynchronous output only when it
                                    // arrives before the user begins editing. Once input
                                    // is dirty, PTY echo/completion is indistinguishable
                                    // from a background process and remains inline.
                                    if !idle_input_dirty_rc.get() {
                                        let mut pending = background_output_rc.borrow_mut();
                                        append_bounded_output(
                                            &mut pending,
                                            bytes,
                                            MAX_RAW_OUTPUT_BYTES,
                                        );
                                    }
                                    scroll_debouncer.mark_dirty(&block_scroll_rc);
                                    true
                                }
                                BlockState::CollectingOutput | BlockState::PostCommand => {
                                    if bstate_rc.get() != BlockState::PostCommand
                                        || !is_post_command_metadata(bytes)
                                    {
                                        active_rc.borrow().accumulate_output(bytes);
                                    }
                                    for cb in activity_cbs.borrow().iter() {
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
                                active_vte.feed(bytes);
                            }
                        }

                        ParserEvent::PromptStart => {
                            ftcs_seen_rc.set(true);
                            let state = bstate_rc.get();
                            if state == BlockState::CollectingOutput
                                || state == BlockState::AltScreen
                            {
                                continue;
                            }
                            if state == BlockState::PostCommand
                                && !agent_prompt_boundary_is_trusted(
                                    active_agent_execution_rc.get(),
                                    pty_for_init.shell_is_foreground(),
                                )
                            {
                                // A foreground child can print a guessed/known
                                // C/D/A sequence. Its D may have moved us to
                                // PostCommand, but it cannot return foreground
                                // ownership to the shell. Resume capture and
                                // wait for the shell's real D + prompt instead.
                                log::warn!(
                                    "Ignoring an Agent prompt marker while a child process still owns the PTY"
                                );
                                pending_exit_code_rc.set(None);
                                cmd_running_rc.set(true);
                                bstate_rc.set(BlockState::CollectingOutput);
                                continue;
                            }
                            if state == BlockState::PostCommand
                                && !agent_completion_trusted_rc.get()
                            {
                                if let Some(execution) = active_agent_execution_rc.take() {
                                    emit_agent_execution_lost(
                                        &verified_submission.agent_execution_lost_callbacks,
                                        execution,
                                        "the shell prompt arrived without a trusted matching command end",
                                    );
                                }
                            }
                            let background_output = if state == BlockState::AwaitingCommand {
                                take_background_output(&background_output_rc)
                            } else {
                                None
                            };
                            let is_background = background_output.is_some();
                            // Finalize the previous command (deferred from CommandEnd),
                            // or turn commandless async output into a first-class block.
                            if state == BlockState::PostCommand || is_background {
                                // The VTE-text capture taken at CommandStart is
                                // authoritative — it reflects what was on screen
                                // when the user pressed Enter. Fall back to the
                                // keystroke shadow only if the VTE read came back
                                // empty (which would indicate the prompt-end
                                // anchor never captured a valid cursor position).
                                let cmd = if is_background {
                                    String::new()
                                } else {
                                    finished_command(
                                        &vte_typed_cmd_rc.borrow(),
                                        &typed_cmd_rc.borrow(),
                                    )
                                };

                                if cmd.is_empty() && !is_background {
                                    // Nothing meaningful to record; just reset.
                                    let preserve = config_for_cb.borrow().preserve_live_scrollback;
                                    active_rc.borrow().reset_active(preserve);
                                    // No block is created here, so half-uploaded
                                    // kitty chunks and undisplayed images have
                                    // nowhere to land: drop them with the rest of
                                    // the active state instead of leaking into
                                    // the next command.
                                    kitty_assembler_rc.borrow_mut().reset();
                                    kitty_pending_images_rc.borrow_mut().clear();
                                    kitty_pending_bytes_rc.set(0);
                                    bstate_rc.set(BlockState::CollectingPrompt);
                                    prompt_buf_rc.borrow_mut().clear();
                                    scroll_debouncer.mark_dirty(&block_scroll_rc);
                                    continue;
                                }

                                let prompt = if is_background {
                                    String::new()
                                } else {
                                    prompt_display_rc.borrow().clone()
                                };

                                // The raw bytes already carry CRLF — the PTY's
                                // ONLCR turns `\n` into `\r\n` on the master side
                                // before we ever see them — and the finished VTE
                                // handles in-line CR overwrites natively, just
                                // like the live VTE did while the command ran. So
                                // we feed the captured bytes verbatim, with no
                                // reconstruction pass.
                                let output_with_ansi = background_output
                                    .unwrap_or_else(|| active_rc.borrow().output_text());

                                let output_plain = strip_ansi(&output_with_ansi).to_string();

                                let truncation_limit =
                                    config_for_cb.borrow().truncation_threshold_lines as usize;
                                let output_trimmed = {
                                    let trimmed = output_plain.trim();
                                    let lines: Vec<&str> = trimmed.lines().collect();
                                    if lines.len() > truncation_limit {
                                        let kept: String = lines[..truncation_limit].join("\n");
                                        format!("{}\n\n[... truncated: {} lines total, showing first {}]", kept, lines.len(), truncation_limit)
                                    } else {
                                        trimmed.to_string()
                                    }
                                };

                                let line_count = output_trimmed.lines().count();

                                let start_time = if is_background {
                                    None
                                } else {
                                    block_start_time_for_cb.get()
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
                                    shell_duration_ms_rc.take()
                                };
                                let duration_ms =
                                    block_duration_ms(shell_duration_ms, start_time, now);

                                let block_cwd = {
                                    // The cwd the shell said the command ran in wins
                                    // over the pane's tracked cwd: after `cd`, the
                                    // OSC 7 that updated the pane already names the
                                    // directory the *next* command will run in.
                                    let reported = if is_background {
                                        None
                                    } else {
                                        command_cwd_rc.borrow_mut().take()
                                    };
                                    reported.or_else(|| {
                                        let cwd_str = current_cwd_for_cb.borrow().clone();
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
                                    pending_exit_code_rc.get()
                                };

                                // Single id shared by the serializable BlockData and
                                // the GTK FinishedBlock so id-keyed lookups (export,
                                // delete) resolve in both lists.
                                let block_id = next_block_id();
                                // Capture cols now (live VTE is allocated by the time
                                // a command finishes) and store it on BlockData so
                                // session restore can recreate the finished VTE at
                                // the same width — preserving column-formatted output
                                // (ls, git log, etc.) instead of reflowing it.
                                let cols = active_rc.borrow().grid_cols() as i64;
                                let estimated_height = estimated_finished_block_height_for_text(
                                    &config_for_cb.borrow(),
                                    &output_plain,
                                    cols,
                                );
                                let block_data = BlockData {
                                    id: block_id,
                                    prompt: prompt.clone(),
                                    cmd: cmd.clone(),
                                    cmd_markup: None,
                                    output: output_plain.trim().to_string(),
                                    exit_code,
                                    estimated_height,
                                    line_count,
                                    start_time_ms,
                                    end_time_ms,
                                    duration_ms,
                                    cwd: block_cwd.clone(),
                                    cols: cols.clamp(1, u16::MAX as i64) as u16,
                                };

                                mutate_block_data_and_redraw(
                                    &block_data_for_cb,
                                    failure_marker_redraw.as_ref(),
                                    |blocks| blocks.push_back(block_data),
                                );

                                // Drain the kitty-graphics images decoded during
                                // this command so the finished block mounts them
                                // below its text output. Images are display-only:
                                // BlockData/history stay text-only, so a restored
                                // session simply omits them.
                                let kitty_images: Vec<gtk::gdk::Texture> =
                                    kitty_pending_images_rc.borrow_mut().drain(..).collect();
                                kitty_pending_bytes_rc.set(0);

                                let recycled = widget_pool_for_cb.borrow_mut().acquire();
                                // Snapshot VTEs must match what the live view
                                // showed: overlay any dynamic OSC 10/11/12
                                // colors onto the theme for this block.
                                let block_config = finished_block_config(
                                    &dynamic_colors_rc,
                                    &config_for_cb.borrow(),
                                );
                                let finished = FinishedBlock::new_with_pool(
                                    block_id,
                                    &prompt,
                                    &cmd,
                                    None,
                                    &output_with_ansi,
                                    exit_code,
                                    &block_config,
                                    duration_ms,
                                    end_time_ms,
                                    block_cwd.as_deref(),
                                    cols,
                                    &kitty_images,
                                    recycled,
                                );
                                finished.widget().insert_before(
                                    &block_list_rc,
                                    Some(active_rc.borrow().widget()),
                                );

                                let was_user_scrolled = scroll_debouncer.user_scrolled_up.get();

                                // If the user is reading history (scrolled up), this
                                // freshly-finished block is "unread": bump the FAB badge
                                // so they can see work completed below and jump to it.
                                if was_user_scrolled {
                                    unread_count_rc.set(unread_count_rc.get().saturating_add(1));
                                    set_jump_fab_label(&jump_fab, unread_count_rc.get());
                                    jump_fab.set_visible(true);
                                }

                                let max_blocks = config_for_cb.borrow().max_visible_blocks as usize;
                                let finished_clone = finished.clone();
                                let finished_widget = finished_clone.widget().clone();

                                finished_clone.connect_actions(
                                    &active_vte,
                                    &pty_for_init,
                                    &pty_synced_rc,
                                    &bracketed_paste_rc,
                                    &typed_cmd_rc,
                                    &armed_agent_execution_rc,
                                    &bstate_rc,
                                    &active_rc,
                                );
                                finished_clone.connect_scroll_forwarding(&block_scroll_rc);

                                finished_blocks_for_cb.borrow_mut().push(finished);

                                if !is_background {
                                    let output_sample = sample_output_for_event(&output_plain);
                                    let agent_execution = active_agent_execution_rc.take();
                                    for cb in block_finished_cbs.borrow().iter() {
                                        cb(
                                            cmd.clone(),
                                            exit_code,
                                            output_sample.clone(),
                                            agent_execution,
                                            duration_ms,
                                        );
                                    }
                                    // Attach this block's output to the shell's own
                                    // journal record. Only jsh sends an execution id,
                                    // and only it writes the record this completes —
                                    // without the id there is nothing to correlate,
                                    // which is why the capture used to stay stranded
                                    // in this window.
                                    if let Some(id) = execution_id_rc.borrow_mut().take() {
                                        // The journal gets the line-capped text this
                                        // pane kept, with `truncated` set from the same
                                        // cap. `total_bytes` is measured against the
                                        // trimmed full capture, so a record that merely
                                        // lost surrounding blank lines is not reported
                                        // as truncated. (What this layer cannot see is
                                        // the live buffer's byte cap dropping the *head*
                                        // of a very long stream — a gap, not a claim.)
                                        let total_bytes = output_plain.trim().len();
                                        let submitted =
                                            jterm_core::execution_journal::CompletedExecution {
                                                id,
                                                output: output_trimmed.clone(),
                                                output_available: true,
                                                truncated: output_trimmed.len() != total_bytes,
                                                total_bytes,
                                            };
                                        if let Err(error) =
                                            jterm_core::execution_journal::submit(submitted)
                                        {
                                            log::warn!("jsh execution journal rejected a block's output: {error:?}");
                                        }
                                    }
                                }

                                {
                                    let cfg = config_for_cb.borrow();
                                    if !is_background && cfg.notify_long_blocks {
                                        if let Some(ms) = duration_ms {
                                            if ms >= cfg.notify_long_block_threshold_ms {
                                                crate::notify::long_block_finished(
                                                    &cmd,
                                                    exit_code_for_i32_api(exit_code),
                                                    ms,
                                                );
                                            }
                                        }
                                    }
                                }

                                // Right-click context menu.
                                let finished_blocks_for_menu = finished_blocks_for_cb.clone();
                                let block_list_for_menu = block_list_rc.clone();
                                let vte_for_copy = active_vte.clone();
                                let pty_for_rerun_menu = pty_for_init.clone();
                                let pty_synced_for_rerun_menu = pty_synced_rc.clone();
                                let bracketed_paste_for_rerun_menu = bracketed_paste_rc.clone();
                                let typed_cmd_for_rerun_menu = typed_cmd_rc.clone();
                                let armed_agent_for_rerun_menu = armed_agent_execution_rc.clone();
                                let bstate_for_rerun_menu = bstate_rc.clone();
                                let active_for_rerun_menu = active_rc.clone();
                                let selected_ids_for_menu = selected_block_ids_rc.clone();
                                let selected_for_menu = selected_block_id_rc.clone();
                                let anchor_for_menu = selection_anchor_id_rc.clone();
                                let bookmarks_for_menu = bookmarks_rc.clone();
                                let block_scroll_for_menu = block_scroll_rc.clone();
                                let visible_for_menu = visible_indices_rc.clone();
                                let widget_pool_for_menu = widget_pool_for_cb.clone();
                                let ask_ai_cbs_for_menu = ask_ai_about_block_cbs.clone();
                                let failure_marker_redraw_for_menu = failure_marker_redraw.clone();
                                let block_id = finished_clone.id;

                                let right_click = gtk::GestureClick::new();
                                right_click.set_button(3);

                                let finished_menu_clone = finished_clone.clone();
                                let block_data_for_export = block_data_for_cb.clone();
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
                                    let widget: &gtk::Widget = &finished_menu_clone
                                        .widget()
                                        .clone()
                                        .upcast::<gtk::Widget>();
                                    popover.set_parent(widget);
                                    popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
                                        x as i32, y as i32, 1, 1,
                                    )));
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
                                        block_data_for_export.borrow().iter().any(|block| {
                                            selected.contains(&block.id)
                                                && !block.cmd.trim().is_empty()
                                        })
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
                                                blocks
                                                    .iter()
                                                    .map(|block| (block.id, block.cmd.as_str())),
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
                                            let output =
                                                finished_for_ai.with_stripped_output(|text| {
                                                    crate::ai::truncate_for_context(text, 80)
                                                });
                                            let data = block_data_for_ai.borrow();
                                            let record =
                                                data.iter().find(|block| block.id == block_id);
                                            let truncated = output.contains("lines elided")
                                                || output.contains("bytes elided");
                                            let context = crate::ai::BlockContext {
                                                cmd: finished_for_ai.cmd_text.clone(),
                                                output,
                                                cwd: record.and_then(|block| block.cwd.clone()),
                                                exit_code: exit_code_for_i32_api(
                                                    record.and_then(|block| block.exit_code),
                                                ),
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
                                                    block_clipboard_text(
                                                        &block.cmd,
                                                        &strip_ansi(&block.output),
                                                        false,
                                                    )
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
                                            finished_for_scroll
                                                .scroll_to_edge(&scroll_for_action, false);
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
                                            finished_for_scroll
                                                .scroll_to_edge(&scroll_for_action, true);
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
                                        let bookmarked =
                                            bookmarks_for_menu.borrow().contains(&block_id);
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
                                            let text = selected_blocks_markdown(
                                                blocks.iter(),
                                                &selected,
                                                block_id_md,
                                            );
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
                                            if let Some(block) =
                                                blocks.iter().find(|b| b.id == block_id_json)
                                            {
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
                                        let pty_synced_for_action =
                                            pty_synced_for_rerun_menu.clone();
                                        let bracketed_paste_for_action =
                                            bracketed_paste_for_rerun_menu.clone();
                                        let typed_cmd_for_action = typed_cmd_for_rerun_menu.clone();
                                        let armed_agent_for_action =
                                            armed_agent_for_rerun_menu.clone();
                                        let bstate_for_action = bstate_for_rerun_menu.clone();
                                        let active_for_action = active_for_rerun_menu.clone();
                                        item.set_sensitive(
                                            bstate_for_action.get() == BlockState::AwaitingCommand
                                                && armed_agent_for_action.borrow().is_none(),
                                        );
                                        item.connect_clicked(move |_| {
                                            popover_c.popdown();
                                            let finished = finished_for_rerun.borrow();
                                            let recalled =
                                                if armed_agent_for_action.borrow().is_some() {
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
                                            if let Some(block) =
                                                blocks.iter().find(|b| b.id == block_id_md)
                                            {
                                                let markdown = block.to_markdown();
                                                vte_for_md.clipboard().set_text(&markdown);
                                            }
                                        });
                                        vbox.append(&item);
                                    }

                                    {
                                        let item = make_item("Delete Block");
                                        let popover_c = popover.clone();
                                        let finished_blocks_for_delete =
                                            finished_blocks_for_menu.clone();
                                        let block_list_for_delete = block_list_for_menu.clone();
                                        let block_data_for_delete = block_data_for_export.clone();
                                        let selected_ids_for_delete = selected_ids_for_menu.clone();
                                        let selected_for_delete = selected_for_menu.clone();
                                        let anchor_for_delete = anchor_for_menu.clone();
                                        let bookmarks_for_delete = bookmarks_for_menu.clone();
                                        let visible_for_delete = visible_for_menu.clone();
                                        let widget_pool_for_delete = widget_pool_for_menu.clone();
                                        let failure_marker_redraw_for_delete =
                                            failure_marker_redraw_for_menu.clone();
                                        let block_id_del = block_id;
                                        item.connect_clicked(move |_| {
                                            popover_c.popdown();
                                            let mut blocks =
                                                finished_blocks_for_delete.borrow_mut();
                                            if let Some(pos) =
                                                blocks.iter().position(|b| b.id == block_id_del)
                                            {
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
                                                |blocks| blocks.retain(|b| b.id != block_id_del),
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

                                install_finished_block_selection(
                                    &finished_clone,
                                    &active_rc,
                                    &finished_blocks_for_cb,
                                    &selected_block_ids_rc,
                                    &selected_block_id_rc,
                                    &selection_anchor_id_rc,
                                );

                                if finished_blocks_for_cb.borrow().len() > max_blocks {
                                    let oldest = finished_blocks_for_cb.borrow_mut().remove(0);
                                    remove_finished_block_from_selection(
                                        &finished_blocks_for_cb.borrow(),
                                        &selected_block_ids_rc,
                                        &selected_block_id_rc,
                                        &selection_anchor_id_rc,
                                        oldest.id,
                                    );
                                    bookmarks_rc.borrow_mut().remove(&oldest.id);
                                    let widget_to_release = oldest.widget().clone();
                                    block_list_rc.remove(&widget_to_release);
                                    widget_pool_for_cb.borrow_mut().release(widget_to_release);
                                    visible_indices_rc.borrow_mut().clear();
                                }

                                if block_data_for_cb.borrow().len() > max_blocks {
                                    mutate_block_data_and_redraw(
                                        &block_data_for_cb,
                                        failure_marker_redraw.as_ref(),
                                        VecDeque::pop_front,
                                    );
                                }

                                // Keep a small JSONL command index separate from
                                // optional full-output block history. This powers
                                // History, palette search, and opt-in AI context
                                // without persisting terminal output by default.
                                let (history_path, history_limit) = {
                                    let cfg = config_for_cb.borrow();
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
                                        &cmd,
                                        block_cwd.as_deref(),
                                        exit_code_for_i32_api(exit_code),
                                        end_time_ms,
                                    ) {
                                        log::warn!("command history: {err}");
                                    }
                                }

                                let preserve = config_for_cb.borrow().preserve_live_scrollback;
                                active_rc.borrow().reset_active(preserve);
                                // Drop any half-uploaded kitty chunks so they
                                // can't leak into the next command (the drain
                                // above already moved every completed image onto
                                // the finished block).
                                kitty_assembler_rc.borrow_mut().reset();
                                kitty_pending_images_rc.borrow_mut().clear();
                                kitty_pending_bytes_rc.set(0);
                                if !was_user_scrolled {
                                    scroll_debouncer.reset_scroll_lock();
                                    scroll_debouncer.pin_to_bottom_deferred(&block_scroll_rc);
                                }
                            }
                            bstate_rc.set(BlockState::CollectingPrompt);
                            prompt_buf_rc.borrow_mut().clear();
                            // Live VTE collapses back to the compact input cell
                            // now that no command is running. Sync the PTY size
                            // so the shell sees the new winsize before it reads
                            // anything past the prompt.
                            sync_active_to_pty(
                                &layout_active_surface,
                                &active_vte,
                                &block_scroll_rc,
                                &pty_for_init,
                            );
                            scroll_debouncer.mark_dirty(&block_scroll_rc);
                        }

                        ParserEvent::PromptEnd => {
                            if bstate_rc.get() != BlockState::CollectingPrompt {
                                continue;
                            }
                            verified_submission.cancel_if_pending(
                                "a new prompt arrived before the reviewed command start was verified",
                            );
                            // Capture the rendered prompt (last non-empty line) for the
                            // finished block / export.
                            let prompt_line = {
                                let pb = prompt_buf_rc.borrow();
                                strip_ansi(&pb)
                                    .lines()
                                    .rev()
                                    .find(|l| !l.trim().is_empty())
                                    .unwrap_or("")
                                    .trim()
                                    .to_string()
                            };
                            *prompt_display_rc.borrow_mut() = prompt_line;
                            prompt_buf_rc.borrow_mut().clear();
                            typed_cmd_rc.borrow_mut().clear();
                            vte_typed_cmd_rc.borrow_mut().clear();
                            background_output_rc.borrow_mut().clear();
                            idle_input_dirty_rc.set(false);
                            let prompt_generation =
                                agent_prompt_generation_rc.get().wrapping_add(1);
                            agent_prompt_generation_rc.set(prompt_generation);
                            // An armed write belongs to exactly one prompt. A
                            // redraw/new prompt before CommandStart invalidates
                            // it instead of letting same text match later.
                            armed_agent_execution_rc.borrow_mut().take();
                            active_agent_execution_rc.set(None);
                            agent_completion_trusted_rc.set(false);
                            execution_id_trusted_rc.set(false);
                            // Snapshot the live VTE cursor at the moment the
                            // prompt finishes drawing — this is where the user's
                            // command starts. CommandStart will read text from
                            // here to the cursor's then-position to recover the
                            // command as it really appeared on screen.
                            let (col, row) = active_vte.cursor_position();
                            prompt_end_pos_rc.set((col, row));
                            prompt_anchor_ready_rc.set(false);
                            pty_synced_rc.set(false);
                            bstate_rc.set(BlockState::AwaitingCommand);
                            // VTE applies feed asynchronously. Keep the cursor
                            // captured at the authenticated PromptEnd boundary
                            // immutable, then expose it after a short fence only
                            // if no input or new prompt raced it. Moving this
                            // anchor to the later live cursor could absorb text
                            // printed after PromptEnd (for example a line-editor
                            // prefill) into trusted prompt furniture.
                            {
                                let state = bstate_rc.clone();
                                let dirty = idle_input_dirty_rc.clone();
                                let synced = pty_synced_rc.clone();
                                let generation = agent_prompt_generation_rc.clone();
                                let ready = prompt_anchor_ready_rc.clone();
                                glib::timeout_add_local_once(
                                    std::time::Duration::from_millis(32),
                                    move || {
                                        if state.get() == BlockState::AwaitingCommand
                                            && generation.get() == prompt_generation
                                            && !dirty.get()
                                            && !synced.get()
                                        {
                                            ready.set(true);
                                        }
                                    },
                                );
                            }
                            layout_active_surface();
                            if active_vte.has_focus() {
                                let active_for_focus = active_rc.clone();
                                glib::idle_add_local_once(move || {
                                    active_for_focus.borrow().grab_focus();
                                });
                            }

                            // Feed next initial command if any.
                            if let Some(cmd) = init_cmds_queue_for_cb.borrow_mut().pop_front() {
                                let text = format!("{}\r", cmd);
                                idle_input_dirty_rc.set(true);
                                pty_synced_rc.set(true);
                                pty_for_init.write_bytes(text.as_bytes());
                            }

                            scroll_debouncer.reset_scroll_lock();
                            scroll_debouncer.mark_dirty(&block_scroll_rc);
                        }

                        ParserEvent::CommandStart(meta) => {
                            ftcs_seen_rc.set(true);
                            let state = bstate_rc.get();
                            if state == BlockState::CollectingOutput
                                || state == BlockState::AltScreen
                            {
                                osc133_depth_rc.set(osc133_depth_rc.get().saturating_add(1));
                                continue;
                            }
                            if state != BlockState::AwaitingCommand {
                                continue;
                            }
                            osc133_depth_rc.set(0);
                            // A command start without an intervening PromptStart is
                            // an ambiguous shell-integration edge. Keep those bytes
                            // visible in the live VTE but do not merge them into the
                            // command's output block.
                            background_output_rc.borrow_mut().clear();
                            active_rc.borrow().reset_output_buffer();
                            block_start_time_for_cb.set(Some(SystemTime::now()));
                            // The shell may attach its own measurement to either
                            // mark; jsh puts it on D. Reset it here so the previous
                            // command's figure cannot be reused for this one.
                            shell_duration_ms_rc.set(meta.duration_ms);
                            // jsh's execution id: the key its journal is written
                            // under, so the output captured below can be attached
                            // to the record instead of living only in this window.
                            *execution_id_rc.borrow_mut() = meta.id.clone();
                            let trusted_execution_id = meta.id.as_deref().is_some_and(|id| {
                                pty_for_init
                                    .shell_integration_token()
                                    .is_some_and(|token| command_id_uses_shell_token(id, token))
                            });
                            execution_id_trusted_rc.set(trusted_execution_id);
                            agent_completion_trusted_rc.set(false);
                            // The cwd the command runs *in*. The pane's tracked cwd
                            // comes from an OSC 7 the shell emits with its next
                            // prompt, which for `cd`/`pushd` is already the new
                            // directory by the time this block is finalized.
                            *command_cwd_rc.borrow_mut() = meta.cwd.clone();
                            // Scrape the command off the live VTE as the fallback
                            // for shells that send bare marks: the range from the
                            // cursor captured at PromptEnd to the cursor now (right
                            // before the shell echoes a newline) is what the user
                            // saw, including history recalls and jsh autosuggestion
                            // accepts.
                            let (cmd_end_col, cmd_end_row) = active_vte.cursor_position();
                            let (start_col, start_row) = prompt_end_pos_rc.get();
                            let captured = active_vte
                                .text_range_format(
                                    vte4::Format::Text,
                                    start_row,
                                    start_col,
                                    cmd_end_row,
                                    cmd_end_col,
                                )
                                .0
                                .map(|gs| gs.to_string())
                                .unwrap_or_default();
                            let scraped =
                                normalize_captured_command(&captured, &prompt_display_rc.borrow());
                            let (command, source) = resolve_command_text(
                                meta.command.as_deref(),
                                meta.command_truncated,
                                &scraped,
                            );
                            if source == CommandTextSource::ScreenAfterTruncation {
                                log::debug!(
                                    "Shell dropped an oversized command line; falling back to the screen capture ({} bytes)",
                                    command.len()
                                );
                            }
                            let matching_execution = verified_submission.command_start_observed(
                                meta.command.as_deref(),
                                &captured,
                                trusted_execution_id,
                            );
                            active_agent_execution_rc.set(matching_execution);
                            *vte_typed_cmd_rc.borrow_mut() = command.clone();
                            emit_command_started(
                                &command_started_cbs,
                                CommandStartedEvent {
                                    command: command.clone(),
                                    cwd: command_cwd_rc.borrow().clone(),
                                },
                            );
                            *running_cmd_rc.borrow_mut() = command;
                            cmd_running_rc.set(true);
                            bstate_rc.set(BlockState::CollectingOutput);
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
                            sync_active_to_pty(
                                &layout_active_surface,
                                &active_vte,
                                &block_scroll_rc,
                                &pty_for_init,
                            );
                            scroll_debouncer.mark_dirty(&block_scroll_rc);
                        }

                        ParserEvent::CommandEnd { exit, meta } => {
                            let state = bstate_rc.get();
                            if state != BlockState::CollectingOutput
                                && state != BlockState::AltScreen
                            {
                                continue;
                            }
                            let matches_started_id = command_end_matches_started_id(
                                execution_id_rc.borrow().as_deref(),
                                meta.id.as_deref(),
                            );
                            if osc133_depth_rc.get() > 0 && !matches_started_id {
                                osc133_depth_rc.set(osc133_depth_rc.get() - 1);
                                continue;
                            }
                            if matches_started_id {
                                // A command can print an unmatched nested C
                                // marker. The shell's private outer C/D id still
                                // identifies its real completion, so do not let
                                // hostile output wedge this pane indefinitely.
                                osc133_depth_rc.set(0);
                            }
                            let active_agent_execution = active_agent_execution_rc.get();
                            let shell_is_foreground = pty_for_init.shell_is_foreground();
                            let trusted_match = execution_id_trusted_rc.get()
                                && command_end_matches_started_id(
                                    execution_id_rc.borrow().as_deref(),
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
                                    continue;
                                }
                                AgentCommandEndDecision::Accept => {
                                    if active_agent_execution.is_some() {
                                        agent_completion_trusted_rc.set(true);
                                    }
                                }
                                AgentCommandEndDecision::AcceptWithoutAgentCorrelation => {
                                    let execution = active_agent_execution
                                        .expect("decision requires an active Agent execution");
                                    active_agent_execution_rc.set(None);
                                    agent_completion_trusted_rc.set(false);
                                    emit_agent_execution_lost(
                                        &verified_submission.agent_execution_lost_callbacks,
                                        execution,
                                        "the shell command end lacked a trusted matching id or foreground owner",
                                    );
                                }
                            }
                            // Safety net (Warp parity): if the alt-screen app
                            // crashed or exited without rmcup, force the UI back
                            // to the block list so the next prompt is usable.
                            if state == BlockState::AltScreen {
                                let mode = active_alt_screen_mode_rc.replace(None).unwrap_or(1049);
                                let leave = format!("\x1b[?{mode}l");
                                active_vte.feed(leave.as_bytes());
                                exit_fullscreen(
                                    &finished_blocks_for_cb,
                                    &visible_indices_rc,
                                    &fullscreen_rc,
                                );
                                active_rc.borrow().set_live_organism_alt_screen(false);
                                layout_active_surface();
                            }
                            // `None` stays `None`: a shell that reported no status
                            // is not a shell that reported success, and this used
                            // to collapse to a green `exit 0`.
                            pending_exit_code_rc.set(*exit);
                            // jsh measures the command itself; only fall back to
                            // this process's timer when the shell said nothing.
                            if meta.duration_ms.is_some() {
                                shell_duration_ms_rc.set(meta.duration_ms);
                            }
                            // The D packet repeats the execution id, so a shell
                            // that only tags the finish still correlates.
                            if meta.id.is_some() {
                                *execution_id_rc.borrow_mut() = meta.id.clone();
                            }
                            let duration_ms = shell_duration_ms_rc.get().or_else(|| {
                                block_start_time_for_cb.get().and_then(|started| {
                                    SystemTime::now()
                                        .duration_since(started)
                                        .ok()
                                        .map(|elapsed| {
                                            elapsed.as_millis().min(u64::MAX as u128) as u64
                                        })
                                })
                            });
                            emit_command_finished(
                                &command_finished_cbs,
                                CommandFinishedEvent {
                                    command: running_cmd_rc.borrow().clone(),
                                    cwd: command_cwd_rc.borrow().clone(),
                                    exit_code: *exit,
                                    duration_ms,
                                },
                            );
                            cmd_running_rc.set(false);
                            bstate_rc.set(BlockState::PostCommand);
                            scroll_debouncer.mark_dirty(&block_scroll_rc);
                        }

                        ParserEvent::AltScreenEnter(mode) => {
                            let from_state = bstate_rc.get();
                            if from_state != BlockState::CollectingOutput
                                && from_state != BlockState::AwaitingCommand
                            {
                                continue;
                            }
                            prev_state_rc.set(from_state);
                            bstate_rc.set(BlockState::AltScreen);
                            active_alt_screen_mode_rc.set(Some(*mode));
                            {
                                let active = active_rc.borrow();
                                active.set_live_organism_visible(false);
                                active.set_live_organism_alt_screen(true);
                            }
                            // Hand the viewport to the alt-screen app: hide finished
                            // blocks so the live VTE fills the scroll area.
                            enter_fullscreen(
                                &finished_blocks_for_cb,
                                &visible_indices_rc,
                                &fullscreen_rc,
                            );
                            // Grow the live VTE to the full viewport before the
                            // app draws (see sync_active_to_pty doc).
                            sync_active_to_pty(
                                &layout_active_surface,
                                &active_vte,
                                &block_scroll_rc,
                                &pty_for_init,
                            );
                            let enter = format!("\x1b[?{mode}h");
                            active_vte.feed(enter.as_bytes());
                        }

                        ParserEvent::AltScreenLeave(mode) => {
                            if bstate_rc.get() != BlockState::AltScreen {
                                continue;
                            }
                            // Warp parity: alt-screen content is ephemeral and is
                            // NOT merged into the block. The active block keeps
                            // just the command name + exit code.
                            active_alt_screen_mode_rc.set(None);
                            active_rc.borrow().set_live_organism_alt_screen(false);
                            let leave = format!("\x1b[?{mode}l");
                            active_vte.feed(leave.as_bytes());
                            exit_fullscreen(
                                &finished_blocks_for_cb,
                                &visible_indices_rc,
                                &fullscreen_rc,
                            );
                            osc133_depth_rc.set(0);
                            bstate_rc.set(prev_state_rc.get());
                            // Collapse the live VTE back to the compact input cell
                            // now that the alt app has released the viewport.
                            sync_active_to_pty(
                                &layout_active_surface,
                                &active_vte,
                                &block_scroll_rc,
                                &pty_for_init,
                            );
                            if active_vte.has_focus() {
                                let active_for_idle = active_rc.clone();
                                glib::idle_add_local_once(move || {
                                    active_for_idle.borrow().grab_focus();
                                });
                            }
                        }

                        ParserEvent::ClipboardSet(text) => {
                            if config_for_cb.borrow().allow_remote_clipboard_write {
                                if let Some(display) = gtk::gdk::Display::default() {
                                    let clipboard = display.clipboard();
                                    clipboard.set_text(text);
                                }
                            }
                        }

                        ParserEvent::ClipboardQuery => {
                            pty_for_init.write_bytes(b"\x1b]52;c;\x1b\\");
                        }

                        ParserEvent::ColorQuery(kind) => {
                            let reply = build_color_query_reply(
                                &config_for_cb.borrow(),
                                dynamic_colors_rc.get(),
                                *kind,
                            );
                            pty_for_init.write_bytes(reply.as_bytes());
                        }

                        // The original OSC bytes already passed through to the
                        // live VTE (which recolors natively); only the tracked
                        // values change here so queries and new finished blocks
                        // see the dynamic color.
                        ParserEvent::ColorSet { kind, spec } => {
                            let mut colors = dynamic_colors_rc.get();
                            colors.set(*kind, spec);
                            dynamic_colors_rc.set(colors);
                        }

                        ParserEvent::ColorReset(kind) => {
                            let mut colors = dynamic_colors_rc.get();
                            colors.reset(*kind);
                            dynamic_colors_rc.set(colors);
                        }

                        ParserEvent::KeyboardProtocolQuery(query) => {
                            let (col, row) = active_vte.cursor_position();
                            let reply = build_keyboard_query_reply(*query, col, row);
                            pty_for_init.write_bytes(reply.as_bytes());
                        }

                        ParserEvent::ApcSequence(payload) => {
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
                                let outcome = kitty_assembler_rc.borrow_mut().feed(payload);
                                // Answer before consuming the outcome: clients
                                // like `kitten icat` block on the `i=`-keyed
                                // OK/error reply.
                                if let Some(reply) = kitty_graphics::response_for(payload, &outcome)
                                {
                                    pty_for_init.write_bytes(&reply);
                                }
                                if let kitty_graphics::Outcome::Complete(texture) = outcome {
                                    // Rough memory bound: width*height*4 (bytes
                                    // per RGBA pixel). Once the shared per-block
                                    // budget is exhausted, further images drop —
                                    // the transmission was still acknowledged
                                    // above, only the display is skipped.
                                    let approx = (texture.width() as usize)
                                        .saturating_mul(texture.height() as usize)
                                        .saturating_mul(4);
                                    let used = kitty_pending_bytes_rc.get();
                                    if used + approx <= kitty_graphics::MAX_PENDING_BYTES_PER_BLOCK
                                    {
                                        kitty_pending_bytes_rc.set(used + approx);
                                        kitty_pending_images_rc.borrow_mut().push(texture);
                                    } else {
                                        log::warn!(
                                            "kitty graphics: per-block image budget exhausted ({} + {} > {}), dropping",
                                            used,
                                            approx,
                                            kitty_graphics::MAX_PENDING_BYTES_PER_BLOCK
                                        );
                                    }
                                }
                            }
                        }

                        ParserEvent::Notification { title, body } => {
                            // Desktop notification requested via OSC 9 / OSC
                            // 777. The parser already control-stripped and
                            // capped the text; here only launch pacing is
                            // enforced, then extras drop silently.
                            let now = Instant::now();
                            LAST_NOTIFICATION_AT.with(|last| {
                                if notification_permitted(last.get(), now) {
                                    last.set(Some(now));
                                    crate::notify::app_notification(title.as_deref(), body);
                                }
                            });
                        }
                    }
                }
            },
        ));

        selection_feed_hold.set_flush({
            let process_chunk = Rc::downgrade(&process_chunk);
            move |bytes| {
                if let Some(process_chunk) = process_chunk.upgrade() {
                    (process_chunk.borrow_mut())(bytes);
                }
            }
        });

        let hold_for_reader = selection_feed_hold.clone();
        let hold_for_exit = selection_feed_hold.clone();
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

/// Convert mapped GTK scroll geometry into a block range. Notebook/tab
/// transitions temporarily expose zero-sized adjustments; retaining the last
/// valid set avoids virtualizing every card during that transient frame.
fn viewport_state_for_scroll(
    block_data: &VecDeque<BlockData>,
    scroll_top: f64,
    viewport_height: f64,
    margin_pages: u32,
) -> Option<ViewportState> {
    if !scroll_top.is_finite() || !viewport_height.is_finite() || viewport_height < 1.0 {
        return None;
    }
    let scroll_top = scroll_top.max(0.0) as i32;
    let viewport_height = viewport_height as i32;
    if viewport_height <= 0 {
        return None;
    }
    let margin_pages = i32::try_from(margin_pages).unwrap_or(i32::MAX);
    let margin = viewport_height.saturating_mul(margin_pages);
    let visible_top = scroll_top.saturating_sub(margin).max(0);
    let visible_bottom = scroll_top
        .saturating_add(viewport_height)
        .saturating_add(margin);
    (visible_bottom > visible_top)
        .then(|| compute_viewport_state(block_data, visible_top, visible_bottom))
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
    /// Replace the runtime configuration shared by parser and rendering
    /// callbacks. Visual setters are dispatched separately; this updates
    /// behavioral options without requiring Block panes to be recreated.
    pub(crate) fn reload_config(&self, config: &Config) {
        *self.config.borrow_mut() = config.clone();
    }

    pub fn new(
        config: &Config,
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
        let active = Rc::new(RefCell::new(ActiveBlock::new(config)));
        let active_vte = active.borrow().active_vte.clone();
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
        let background_output: Rc<RefCell<VecDeque<u8>>> = Rc::new(RefCell::new(VecDeque::new()));
        let idle_input_dirty: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // Command text snapshot taken at CommandStart from the VTE itself,
        // between `prompt_end_pos` and the current cursor. This is what
        // finalize uses to record the run.
        let vte_typed_cmd: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        // VTE cursor position (col, row) right after the prompt finished
        // drawing — anchor for the text-range read at CommandStart.
        let prompt_end_pos: Rc<Cell<(i64, i64)>> = Rc::new(Cell::new((0, 0)));
        let prompt_anchor_ready: Rc<Cell<bool>> = Rc::new(Cell::new(false));

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
        let finished_blocks_rc: Rc<RefCell<Vec<FinishedBlock>>> = Rc::new(RefCell::new(Vec::new()));

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

        // ── Warp-style input-cell sizing ──────────────────────────────────
        // The live VTE holder hugs its content (prompt + typed command) with a
        // guaranteed minimum height while idle, so finished blocks remain visible
        // above it. It is forced to the full viewport only for alt-screen apps
        // (vim/less/TUIs) which need real terminal rows. During a running command
        // the height is frozen at the idle value (no per-chunk resize / SIGWINCH
        // thrash); the full output is snapshotted into a finished block when done.
        let layout_active_surface: Rc<dyn Fn()> = {
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
            Rc::new(move || {
                let (Some(holder), Some(vte), Some(scroll)) =
                    (holder.upgrade(), vte.upgrade(), scroll.upgrade())
                else {
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
                if matches!(
                    state,
                    BlockState::CollectingOutput | BlockState::PostCommand
                ) {
                    let target = (cols, viewport_rows);
                    if last_size_target.get() != target {
                        vte.set_size(cols, viewport_rows);
                        last_size_target.set(target);
                    }
                    holder.set_visible(true);
                    holder.set_height_request((viewport_rows as i32) * cell_h);
                    fit_finished_outputs();
                    return;
                }
                holder.set_visible(true);
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
                fit_finished_outputs();
            })
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

        // State to restore when an alt-screen app exits (anvil model).
        let prev_state: Rc<Cell<BlockState>> = Rc::new(Cell::new(BlockState::Idle));
        let osc133_depth: Rc<Cell<u32>> = Rc::new(Cell::new(0));
        let prompt_buf: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        // Rendered prompt captured at PromptEnd (prompt_buf is cleared once the
        // prompt ends, so the finalize path reads this instead).
        let prompt_display: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
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

        // Set from the shell's OSC 133 `D` packet. `None` — the initial value and
        // the value for a shell that omits the status — means "not reported",
        // which the block header renders as neutral rather than as `exit 0`.
        let pending_exit_code: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
        // Metadata jsh attaches to the same marks (see ParserEvent::CommandStart).
        let shell_duration_ms: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));
        let execution_id: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let execution_id_trusted: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let agent_completion_trusted: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let command_cwd: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

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
            active_vte: active_vte.clone(),
            bstate: bstate.clone(),
            pty: pty.clone(),
            typed_cmd: typed_cmd.clone(),
            idle_input_dirty: idle_input_dirty.clone(),
            pty_synced: pty_synced.clone(),
            prompt_end_pos: prompt_end_pos.clone(),
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

        // ── Wire PTY → parser → block events ─────────────────────────────
        {
            let active_rc = active.clone();
            let active_vte_rc = active_vte.clone();
            let bstate_rc = bstate.clone();
            let prev_state_rc = prev_state.clone();
            let osc133_depth_rc = osc133_depth.clone();
            let prompt_buf_rc = prompt_buf.clone();
            let typed_cmd_rc = typed_cmd.clone();
            let vte_typed_cmd_rc = vte_typed_cmd.clone();
            let prompt_end_pos_rc = prompt_end_pos.clone();
            let prompt_display_rc = prompt_display.clone();
            let block_list_rc = block_list.clone();
            let block_scroll_rc = block_scroll.clone();
            let exited_cbs = exited_callbacks.clone();
            let activity_cbs = activity_callbacks.clone();
            let mouse_reporting_rc = mouse_reporting_mode.clone();
            let bracketed_paste_rc = bracketed_paste.clone();
            let dynamic_colors_rc = dynamic_colors.clone();
            let config_for_cb = Rc::new(RefCell::new(config.clone()));
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
            let pending_exit_code_rc = pending_exit_code.clone();
            let current_cwd_for_cb = current_cwd.clone();

            let event_buf: Rc<RefCell<Vec<ParserEvent>>> =
                Rc::new(RefCell::new(Vec::with_capacity(32)));
            ReaderCtx {
                active_rc,
                active_vte: active_vte_rc,
                bstate_rc,
                prev_state_rc,
                osc133_depth_rc,
                prompt_buf_rc,
                typed_cmd_rc,
                background_output_rc: background_output.clone(),
                idle_input_dirty_rc: idle_input_dirty.clone(),
                vte_typed_cmd_rc,
                prompt_end_pos_rc,
                prompt_anchor_ready_rc: prompt_anchor_ready.clone(),
                prompt_display_rc,
                block_list_rc,
                block_scroll_rc,
                remote_session_cbs: remote_session_callbacks.clone(),
                exited_cbs,
                activity_cbs,
                command_started_cbs: command_started_callbacks.clone(),
                command_finished_cbs: command_finished_callbacks.clone(),
                mouse_reporting_rc,
                bracketed_paste_rc,
                dynamic_colors_rc,
                config_for_cb,
                parser,
                block_data_for_cb,
                failure_marker_redraw: failure_marker_redraw.clone(),
                finished_blocks_for_cb,
                scroll_debouncer,
                widget_pool_for_cb,
                pty_synced_rc,
                visible_indices_rc,
                fullscreen_rc,
                ftcs_seen_rc,
                init_cmds_queue_for_cb,
                pty_for_init,
                block_start_time_for_cb,
                pending_exit_code_rc,
                shell_duration_ms_rc: shell_duration_ms.clone(),
                execution_id_rc: execution_id.clone(),
                execution_id_trusted_rc: execution_id_trusted.clone(),
                agent_completion_trusted_rc: agent_completion_trusted.clone(),
                command_cwd_rc: command_cwd.clone(),
                current_cwd_for_cb,
                event_buf,
                unread_count_rc: unread_count.clone(),
                jump_fab: jump_fab.clone(),
                selected_block_ids_rc: selected_block_ids.clone(),
                selected_block_id_rc: selected_block_id.clone(),
                selection_anchor_id_rc: selection_anchor_id.clone(),
                bookmarks_rc: block_bookmarks.clone(),
                cmd_running_rc: cmd_running.clone(),
                running_cmd_rc: running_cmd.clone(),
                armed_agent_execution_rc: armed_agent_execution.clone(),
                agent_prompt_generation_rc: agent_prompt_generation.clone(),
                active_agent_execution_rc: active_agent_execution.clone(),
                agent_execution_supported_rc: agent_execution_supported.clone(),
                verified_submission: verified_submission.clone(),
                layout_active_surface: layout_active_surface.clone(),
                block_finished_cbs: block_finished_callbacks.clone(),
                ask_ai_about_block_cbs: ask_ai_about_block_callbacks.clone(),
                selection_feed_hold: selection_feed_hold.clone(),
            }
            .install(&pty)?;

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
                    let cmd_disp =
                        crate::text_safety::bounded_display_text(cmd.trim(), 1024, false);
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
                        let command =
                            crate::text_safety::bounded_display_text(command, 1024, false);
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
                prompt_end_pos: prompt_end_pos.clone(),
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
            let scroll_ctrl = gtk::EventControllerScroll::new(
                gtk::EventControllerScrollFlags::VERTICAL
                    | gtk::EventControllerScrollFlags::HORIZONTAL,
            );
            scroll_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
            scroll_ctrl.connect_scroll(move |_, _dx, dy| {
                let in_mouse_app = fullscreen_for_scroll.get()
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
            active_vte,
            active,
            bstate,
            prompt_buf,
            typed_cmd,
            prompt_end_pos,
            prompt_anchor_ready,
            armed_agent_execution,
            agent_prompt_generation,
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
            command_started_callbacks,
            command_finished_callbacks,
            block_finished_callbacks,
            ask_ai_about_block_callbacks,
            mouse_reporting_mode,
            bracketed_paste,
            dynamic_colors,
            config: Rc::new(RefCell::new(config.clone())),
            block_data: block_data_rc,
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
            selection_anchor_id,
            bookmarks: block_bookmarks,
            cleared_stash: RefCell::new(Vec::new()),
            unread_count,
            jump_fab,
            sticky_organism_slot,
            find_state: Rc::new(RefCell::new(FindState::default())),
            current_cwd: current_cwd.clone(),
            resize_tick_id: RefCell::new(None),
            sticky_timer_id: RefCell::new(Some(sticky_timer_id)),
            cross_selection,
            selection_feed_hold,
            layout_active_surface,
        };

        // Load history if configured
        let _ = term_view.load_history();

        // Restored blocks keep their persisted ids while the global counter
        // restarts at 0 every launch. Seed it past every restored id so a new
        // block can never alias one — selection, bookmarks, undo-clear, and
        // the block context menu all key on id uniqueness.
        if let Some(max_id) = term_view.block_data.borrow().iter().map(|b| b.id).max() {
            BLOCK_ID_COUNTER.fetch_max(max_id + 1, Ordering::Relaxed);
        }

        // Create widgets for loaded blocks. Each block's `cols` is what the live
        // VTE was wrapping at when the command ran; restoring at the same cols
        // reproduces the exact line breaks (so `ls` columns don't get split
        // mid-word). For old saves without a cols field (cols == 0), fall back
        // to the live VTE's current column count.
        {
            let config = term_view.config.borrow();
            let fallback_cols = term_view.active.borrow().grid_cols() as i64;
            mutate_block_data_and_redraw(
                &term_view.block_data,
                term_view.failure_marker_redraw.as_ref(),
                |block_data_ref| {
                    for block in block_data_ref.iter_mut() {
                        let cols = if block.cols > 0 {
                            block.cols as i64
                        } else {
                            fallback_cols
                        };
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
                    let Some(strict) = viewport_state_for_scroll(
                        &block_data_ref,
                        adjustment.value(),
                        adjustment.page_size(),
                        margin,
                    ) else {
                        return;
                    };
                    let loose = viewport_state_for_scroll(
                        &block_data_ref,
                        adjustment.value(),
                        adjustment.page_size(),
                        margin.saturating_add(1),
                    );
                    drop(block_data_ref);

                    let new_visible =
                        stable_visible_indices(&strict, loose.as_ref(), &visible.borrow());
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
        let last: Rc<Cell<(u16, u16)>> = Rc::new(Cell::new((0, 0)));
        let tick_id = self.active_vte.add_tick_callback(move |vte, _clock| {
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
        LiveOrganismSurfaceMetrics {
            // The hidden surface may not be allocated yet. The always-mapped
            // VTE is its measured child and shares the same clipped space.
            width: active.active_vte.width().max(0),
            height: active.active_vte.height().max(0),
            cell_width: (active.active_vte.char_width() as i32).max(1),
            cell_height: (active.active_vte.char_height() as i32).max(1),
            right_gutter: LIVE_ORGANISM_RIGHT_GUTTER,
            alt_screen: active.live_organism_alt_screen(),
            cursor_row: {
                let (_, cursor_row) = active.active_vte.cursor_position();
                let top_row = gtk::prelude::ScrollableExt::vadjustment(&active.active_vte)
                    .map(|adjustment| adjustment.value().floor() as i64)
                    .unwrap_or(0);
                let visible_rows = active.active_vte.row_count().max(1);
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

    /// Insert a transient card directly above the live prompt. Agent UI is
    /// deliberately not a finished block, so it stays out of history,
    /// selection, virtualization, and persistence metadata.
    ///
    /// Calling this for an already-inserted widget re-pins it below any newly
    /// completed command block.
    pub fn insert_inline_notice(&self, widget: &gtk::Widget) {
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
    }

    /// Remove a transient inline card. Safe when the widget was already
    /// detached as part of pane teardown.
    pub fn remove_inline_notice(&self, widget: &gtk::Widget) {
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
                if self.active_vte.cursor_position() == self.prompt_end_pos.get()
                    && crate::terminal::click_cursor::verified_suffix_is_empty(&self.active_vte)
                        == Some(true)
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
        F: Fn(String, Option<i32>, String, Option<crate::agent::AgentExecutionRef>, Option<u64>)
            + 'static,
    {
        self.block_finished_callbacks.borrow_mut().push(Box::new(f));
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
        let stash: Vec<BlockData> = std::mem::take(&mut *self.cleared_stash.borrow_mut());
        if stash.is_empty() {
            return 0;
        }
        let restored_count = stash.len();

        let mut restored: Vec<FinishedBlock> = Vec::with_capacity(restored_count);
        {
            // Rebuild with the same overlay the reader uses for new blocks: if a
            // dynamic OSC 10/11/12 color is active, restored blocks must match
            // the recolored live view instead of reverting to theme colors.
            let config = finished_block_config(&self.dynamic_colors, &self.config.borrow());
            let fallback_cols = self.active.borrow().grid_cols() as i64;
            // Everything restored predates the current finished blocks, so the
            // insertion anchor is the pane's first finished widget — or the
            // live input block when the pane has none.
            let anchor: gtk::Widget = self
                .finished_blocks
                .borrow()
                .first()
                .map(|block| block.widget().clone().upcast())
                .unwrap_or_else(|| self.active.borrow().widget().clone().upcast());
            let mut stash = stash;
            for block in stash.iter_mut() {
                let cols = if block.cols > 0 {
                    block.cols as i64
                } else {
                    fallback_cols
                };
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

    pub fn apply_failed_filter(&self) {
        if let Some(idx) = self.get_failed_blocks().first().copied() {
            self.scroll_to_block(idx);
        }
    }

    pub fn apply_slow_filter(&self) {
        if let Some(idx) = self.get_slow_blocks(1000).first().copied() {
            self.scroll_to_block(idx);
        }
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
    pub fn jump_to_failed(&self, direction: i32) {
        let failed = self.get_failed_blocks();
        self.jump_to_marked_index(&failed, direction);
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
        let Some(strict) = viewport_state_for_scroll(
            &block_data,
            adjustment.value(),
            adjustment.page_size(),
            margin,
        ) else {
            return;
        };
        let loose = viewport_state_for_scroll(
            &block_data,
            adjustment.value(),
            adjustment.page_size(),
            margin.saturating_add(1),
        );
        drop(block_data);
        let new_visible =
            stable_visible_indices(&strict, loose.as_ref(), &self.visible_indices.borrow());
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
                        self.fullscreen.get().to_string(),
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
            |blocks| blocks.retain(|b| b.id != block_id),
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
        let finished = self.finished_blocks.borrow();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut out: Vec<String> = Vec::new();
        for block in finished.iter().rev() {
            let cmd = block.cmd_text.trim();
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
        build_keyboard_query_reply, classify_command_prompt_status, clear_dynamic_colors,
        coalesce_bytes_events, command_end_matches_started_id, command_id_uses_shell_token,
        decide_agent_command_end, failed_block_marker_fractions, finished_block_config,
        finished_command, finished_layout_key, is_post_command_metadata,
        mutate_block_data_and_redraw, normalize_captured_command, notification_permitted,
        parse_color_spec, pop_typed_command_shadow, record_external_input, resolve_command_text,
        reviewed_pre_command_bytes_are_identity_neutral, reviewed_submission_matches,
        selected_blocks_markdown, selected_command_text, selected_id_range,
        shell_argv_supports_agent_ids, stable_visible_indices, step_marked_indices,
        stranded_focus_key_recovers, strip_ansi, strip_ansi_with_clear_detect,
        take_armed_agent_execution, take_background_output, viewport_page_size_changed,
        viewport_state_for_scroll, visible_indices_for_viewport, AgentCommandEndDecision,
        ArmedAgentExecution, BlockData, BlockState, CommandPromptStatus, CommandTextSource,
        DynamicColors, DynamicColorsRc, ShellCapabilityObserver, MAX_RECALLED_COMMAND_BYTES,
        MAX_TYPED_COMMAND_SHADOW_BYTES, NOTIFICATION_MIN_INTERVAL, TRUNCATED_COMMAND_PLACEHOLDER,
    };
    use crate::agent::{AgentExecutionRef, AgentSession};
    use crate::config::Config;
    use crate::parser::{ColorKind, CommandMeta, KeyboardProtocolQuery, ParserEvent};
    use std::cell::{Cell, RefCell};
    use std::collections::{HashSet, VecDeque};
    use std::rc::Rc;
    use std::time::{Instant, SystemTime};

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
    }

    #[test]
    fn taking_background_output_drains_the_pending_buffer() {
        let pending = RefCell::new(VecDeque::from(b"async line\r\n".to_vec()));
        assert_eq!(
            take_background_output(&pending).as_deref(),
            Some("async line\r\n")
        );
        assert!(pending.borrow().is_empty());
        assert!(take_background_output(&pending).is_none());
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
