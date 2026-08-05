//! blocks — finished-block widgets (VTE-backed) and the live ActiveBlock.
use super::*;
use crate::config::Config;
use crate::terminal::open_uri;
use gtk::Orientation;
use relm4::gtk;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::Terminal;
use vte4::TerminalExt;

// ─── FinishedBlock ────────────────────────────────────────────────────────────

/// Data for a finished command block (decoupled from widget representation)
#[derive(Clone, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub(crate) struct BlockData {
    pub(crate) id: u64,
    pub(crate) prompt: String,
    pub(crate) cmd: String,
    pub(crate) cmd_markup: Option<String>,
    pub(crate) output: String,
    /// Status the shell reported for the command. `None` means it reported none
    /// — a distinct outcome from `Some(0)`, which older snapshots also used for
    /// "unknown". Legacy JSON exports that stored a bare number still load,
    /// since serde reads any present value as `Some`.
    pub(crate) exit_code: Option<i32>,
    pub(crate) estimated_height: i32,
    pub(crate) line_count: usize,
    #[serde(default)]
    pub(crate) start_time_ms: Option<u64>,
    #[serde(default)]
    pub(crate) end_time_ms: Option<u64>,
    #[serde(default)]
    pub(crate) duration_ms: Option<u64>,
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    /// Live-VTE column count at the time this block was finalized. Restored
    /// blocks render at the same cols so their byte stream (which was formatted
    /// for this width, e.g. by `ls`) reproduces the original line breaks
    /// instead of being reflowed at the current window's width. 0 = unknown
    /// (old saves before this field existed) — caller should fall back.
    #[serde(default)]
    pub(crate) cols: u16,
}

fn markdown_fence(text: &str) -> String {
    let mut longest = 0usize;
    let mut current = 0usize;
    for ch in text.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat(longest.saturating_add(1).max(3))
}

impl BlockData {
    pub(crate) fn is_background(&self) -> bool {
        self.cmd.trim().is_empty()
    }

    /// Export block to JSON format
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Export block to Markdown format
    pub fn to_markdown(&self) -> String {
        let mut md = String::new();

        if self.is_background() {
            md.push_str("## Background Output\n\n");
        } else {
            md.push_str("## Command Block\n\n");

            if !self.prompt.is_empty() {
                let fence = markdown_fence(&self.prompt);
                md.push_str("**Prompt:**\n");
                md.push_str(&format!("{fence}text\n{}\n{fence}\n\n", self.prompt));
            }

            let fence = markdown_fence(&self.cmd);
            md.push_str(&format!("**Command:**\n{fence}bash\n"));
            md.push_str(&self.cmd);
            md.push_str(&format!("\n{fence}\n\n"));
        }

        if !self.output.is_empty() {
            let fence = markdown_fence(&self.output);
            md.push_str(&format!("**Output:**\n{fence}\n"));
            md.push_str(&self.output);
            md.push_str(&format!("\n{fence}\n\n"));
        }

        if !self.is_background() {
            match self.exit_code {
                Some(code) => md.push_str(&format!("**Exit Code:** {code}\n\n")),
                // Do not print `0` here: an export is the copy someone reads
                // later, and "the shell never said" is the fact we have.
                None => md.push_str("**Exit Code:** not reported\n\n"),
            }
        }

        if let Some(dur) = self.duration_ms {
            let dur_sec = dur as f64 / 1000.0;
            md.push_str(&format!("**Duration:** {:.3}s\n\n", dur_sec));
        }

        md
    }
}

/// How a finished block's outcome is presented in its header.
///
/// [`Self::Unreported`] is its own state on purpose: a shell that emits a bare
/// `OSC 133;D` tells us a command ended and nothing about how. That used to be
/// stored as `0` and drawn as a green check with no badge, i.e. as a success
/// this terminal never observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockStatus {
    /// Output emitted while the prompt was idle: there was no command at all.
    Background,
    Succeeded,
    Failed(i32),
    Unreported,
}

pub(crate) fn block_status(is_background: bool, exit_code: Option<i32>) -> BlockStatus {
    match (is_background, exit_code) {
        (true, _) => BlockStatus::Background,
        (false, Some(0)) => BlockStatus::Succeeded,
        (false, Some(code)) => BlockStatus::Failed(code),
        (false, None) => BlockStatus::Unreported,
    }
}

impl BlockStatus {
    /// Left-edge stripe on the block frame.
    fn stripe_class(self) -> &'static str {
        match self {
            Self::Background => "block-background",
            Self::Succeeded => "block-success",
            Self::Failed(_) => "block-failed",
            Self::Unreported => "block-unknown",
        }
    }

    /// Header status glyph (Nerd Font) and the CSS class that colours it.
    fn icon(self) -> (&'static str, &'static str) {
        match self {
            // nf-fa-spinner, nf-fa-check, nf-fa-close, nf-fa-question
            Self::Background => ("\u{f110}", "block-status-background"),
            Self::Succeeded => ("\u{f00c}", "block-status-ok"),
            Self::Failed(_) => ("\u{f00d}", "block-status-bad"),
            Self::Unreported => ("\u{f128}", "block-status-unknown"),
        }
    }

    /// Why the glyph looks the way it does. Only the state a user cannot read
    /// off a check or a cross needs explaining.
    fn icon_tooltip(self) -> Option<&'static str> {
        match self {
            Self::Unreported => Some("The shell reported no exit status for this command"),
            _ => None,
        }
    }

    /// Right-hand badge text. A status nobody reported has no number to show,
    /// so the badge is absent rather than showing a made-up one.
    fn exit_badge(self) -> Option<String> {
        match self {
            Self::Failed(code) => Some(format!("exit:{code}")),
            _ => None,
        }
    }
}

/// Text copied for a whole block. Background blocks have no command, so copying
/// them must not introduce the blank first line that a naive `cmd + "\\n" + output`
/// join would create.
pub(crate) fn block_clipboard_text(cmd: &str, output: &str, output_only: bool) -> String {
    if output_only || cmd.trim().is_empty() {
        output.to_string()
    } else if output.trim().is_empty() {
        cmd.to_string()
    } else {
        format!("{}\n{}", cmd, output)
    }
}

/// Filters for searching/filtering blocks
#[derive(Clone, Default)]
pub struct BlockFilters {
    pub exit_code: Option<i32>,
    pub min_duration_ms: Option<u64>,
    pub max_duration_ms: Option<u64>,
    pub failed_only: bool,
    pub slow_only: bool,
    pub slow_threshold_ms: u64,
    pub use_regex: bool,
}

pub(crate) struct FinishedBlock {
    pub(crate) id: u64,
    /// Commandless output emitted while the shell prompt was idle.
    pub(crate) is_background: bool,
    pub(crate) widget: gtk::Box,
    pub(crate) prompt_text: String,
    /// Read-only VTE displaying the executed command line (single-row typically).
    pub(crate) command_vte: vte4::Terminal,
    /// Read-only VTE displaying the captured output. Finished output takes its
    /// full natural height; scrolling belongs to the outer block canvas.
    pub(crate) output_vte: vte4::Terminal,
    /// Visible per-block scrollbar for long output, bound to `output_vte`'s
    /// private adjustment.
    output_scrollbar: gtk::Scrollbar,
    /// Raw ANSI-bearing output bytes — the source for filter re-feed and the
    /// copy-output action. Mutable so filter can swap the displayed slice
    /// without losing the original.
    pub(crate) full_output: Rc<RefCell<String>>,
    /// Filtered output override. `None` means render `full_output` directly, so
    /// every finished block does not retain a second copy of a potentially huge
    /// log. Allocated only while a filter changes what is displayed.
    displayed_output: Rc<RefCell<Option<String>>>,
    /// Lazy-populated ANSI-stripped view of `full_output`, used as the haystack
    /// for find-within-blocks. Avoids re-stripping on every keystroke. Cleared
    /// when `full_output` is rewritten by a filter action; otherwise kept for
    /// the lifetime of the block (finished blocks are append-once in practice).
    pub(crate) stripped_output: Rc<RefCell<Option<String>>>,
    pub(crate) cmd_text: String,
    pub(crate) copy_cmd_btn: gtk::Button,
    pub(crate) copy_output_btn: gtk::Button,
    pub(crate) rerun_btn: gtk::Button,
    pub(crate) header_row: gtk::Box,
    pub(crate) action_box: gtk::Box,
    /// Toggle the per-block output filter without discarding its query. Exposed
    /// so the Warp-compatible keyboard action can target the selected/latest block.
    pub(crate) toggle_filter: Rc<dyn Fn()>,
    /// Explicit Warp-style navigation affordance for oversized output.
    pub(crate) jump_bottom_btn: gtk::Button,
    pub(crate) bookmark_star: gtk::Label,
    pub(crate) status_icon: gtk::Label,
    /// Column count the output VTE is sized to — needed for re-feed (filter).
    pub(crate) cols: i64,
    /// Number of rows allocated to this finished output. Kept with the widget
    /// so filter re-renders use the same full-height canvas allocation.
    pub(crate) viewport_cap: i64,
    /// Current non-expanded row target, recomputed from the pane height minus
    /// the live input block height.
    dynamic_viewport_rows: Rc<Cell<i64>>,
    output_rows: i64,
    expanded: Rc<Cell<bool>>,
    expand_btn: gtk::Button,
    /// True only when this block has more output rows than can be shown at once.
    pub(crate) output_scrollable: bool,
    /// Whether this block is tall enough to expose long-block navigation.
    pub(crate) long_output: bool,
}

impl Clone for FinishedBlock {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            is_background: self.is_background,
            widget: self.widget.clone(),
            prompt_text: self.prompt_text.clone(),
            command_vte: self.command_vte.clone(),
            output_vte: self.output_vte.clone(),
            output_scrollbar: self.output_scrollbar.clone(),
            cmd_text: self.cmd_text.clone(),
            full_output: self.full_output.clone(),
            displayed_output: self.displayed_output.clone(),
            stripped_output: self.stripped_output.clone(),
            copy_cmd_btn: self.copy_cmd_btn.clone(),
            copy_output_btn: self.copy_output_btn.clone(),
            rerun_btn: self.rerun_btn.clone(),
            header_row: self.header_row.clone(),
            action_box: self.action_box.clone(),
            toggle_filter: self.toggle_filter.clone(),
            jump_bottom_btn: self.jump_bottom_btn.clone(),
            bookmark_star: self.bookmark_star.clone(),
            status_icon: self.status_icon.clone(),
            cols: self.cols,
            viewport_cap: self.viewport_cap,
            dynamic_viewport_rows: self.dynamic_viewport_rows.clone(),
            output_rows: self.output_rows,
            expanded: self.expanded.clone(),
            expand_btn: self.expand_btn.clone(),
            output_scrollable: self.output_scrollable,
            long_output: self.long_output,
        }
    }
}

/// Lightweight shell-command syntax highlighter (Warp-style). Emits an ANSI
/// (SGR) string so it can flow through the same `set_active_output_buffer`
/// rendering path as real shell output. Best-effort, dependency-free:
///   - command name (first word, and first word after a pipe/operator): bold cyan
///   - flags (`-x`, `--long`): dim/gray
///   - quoted strings: green
///   - operators (`| & ; > <`): magenta
///   - `$VAR` references: cyan
///
/// Whitespace and all other text are emitted verbatim in the default color, so
/// the reconstructed buffer text matches the command exactly.
pub(crate) fn highlight_command_to_ansi(cmd: &str) -> String {
    const RESET: &str = "\x1b[0m";
    let mut out = String::with_capacity(cmd.len() + 32);
    let mut i = 0usize;
    let mut expect_command = true;
    while i < cmd.len() {
        let c = cmd[i..].chars().next().unwrap();
        if c.is_whitespace() {
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        if c == '"' || c == '\'' {
            let quote = c;
            let start = i;
            i += c.len_utf8();
            while i < cmd.len() {
                let ch = cmd[i..].chars().next().unwrap();
                if quote == '"' && ch == '\\' {
                    i += ch.len_utf8();
                    if i < cmd.len() {
                        let escaped = cmd[i..].chars().next().unwrap();
                        i += escaped.len_utf8();
                    }
                    continue;
                }
                let done = ch == quote;
                i += ch.len_utf8();
                if done {
                    break;
                }
            }
            out.push_str("\x1b[32m");
            out.push_str(&cmd[start..i]);
            out.push_str(RESET);
            expect_command = false;
            continue;
        }
        if matches!(c, '|' | '&' | ';' | '>' | '<') {
            let start = i;
            while i < cmd.len() {
                let ch = cmd[i..].chars().next().unwrap();
                if !matches!(ch, '|' | '&' | ';' | '>' | '<') {
                    break;
                }
                i += ch.len_utf8();
            }
            out.push_str("\x1b[35m");
            out.push_str(&cmd[start..i]);
            out.push_str(RESET);
            expect_command = true;
            continue;
        }
        let start = i;
        while i < cmd.len() {
            let cc = cmd[i..].chars().next().unwrap();
            if cc.is_whitespace() || matches!(cc, '|' | '&' | ';' | '>' | '<' | '"' | '\'') {
                break;
            }
            i += cc.len_utf8();
        }
        let word = &cmd[start..i];
        if word.starts_with('-') {
            out.push_str("\x1b[90m");
            out.push_str(word);
            out.push_str(RESET);
        } else if word.starts_with('$') {
            out.push_str("\x1b[36m");
            out.push_str(word);
            out.push_str(RESET);
        } else if expect_command {
            out.push_str("\x1b[1;36m");
            out.push_str(word);
            out.push_str(RESET);
            expect_command = false;
        } else {
            out.push_str(word);
        }
    }
    out
}

/// Filter raw output (ANSI preserved) to the lines matching `query`, honoring
/// regex / case / invert and `context` lines of surroundings (Warp's
/// BlockFilterQuery). Empty query, or an invalid regex, returns `full` verbatim.
fn filter_output_lines(
    full: &str,
    query: &str,
    use_regex: bool,
    case_sensitive: bool,
    invert: bool,
    context: usize,
) -> String {
    if query.is_empty() {
        return full.to_string();
    }
    let re = if use_regex {
        match regex::RegexBuilder::new(query)
            .case_insensitive(!case_sensitive)
            .build()
        {
            Ok(re) => Some(re),
            Err(_) => return full.to_string(),
        }
    } else {
        None
    };
    let lines: Vec<&str> = full.lines().collect();
    let matches_line = |line: &str| -> bool {
        let hit = if let Some(ref re) = re {
            re.is_match(line)
        } else if case_sensitive {
            line.contains(query)
        } else {
            contains_case_insensitive(line.as_bytes(), query.as_bytes())
        };
        hit ^ invert
    };
    let mut keep = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        if matches_line(line) {
            let lo = i.saturating_sub(context);
            let hi = (i + context + 1).min(lines.len());
            for slot in keep.iter_mut().take(hi).skip(lo) {
                *slot = true;
            }
        }
    }
    lines
        .iter()
        .zip(keep.iter())
        .filter_map(|(l, k)| if *k { Some(*l) } else { None })
        .collect::<Vec<_>>()
        .join("\n")
}

fn output_row_count(text: &str) -> i64 {
    let text = output_display_text(text);
    if text.is_empty() {
        1
    } else {
        let trailing_blank_row =
            text.ends_with('\n') || (text.ends_with('\r') && !text.ends_with("\r\n"));
        let rows = text.lines().count().max(1) as i64;
        if trailing_blank_row {
            rows + 1
        } else {
            rows
        }
    }
}

/// Rows occupied after VTE wraps the snapshot at `cols`. Finished cards need
/// this rather than the logical line count, otherwise a stack trace containing
/// very long type names is still pushed into the VTE's private scrollback.
pub(crate) fn output_visual_row_count(text: &str, cols: i64) -> i64 {
    use unicode_width::UnicodeWidthChar;

    let cols = cols.max(1) as usize;
    // Count what the terminal leaves on screen, not the byte stream used to
    // produce it.  Programs such as apt repeatedly repaint a progress row with
    // CR + EL and wrap ordinary text in SGR/OSC sequences.  Counting those
    // control bytes (and every overwritten progress update) can turn a short
    // result into a false "long output" block.  Long blocks are fitted to the
    // pane height, so that misclassification shows up as a large blank tail.
    // `strip_ansi` applies the horizontal cursor/erase semantics as well as
    // removing escape sequences, which makes this estimate match the VTE
    // snapshot closely enough for the short/long decision.
    let rendered = strip_ansi(text);
    let text = output_display_text(&rendered);
    if text.is_empty() {
        return 1;
    }

    text.split('\n')
        .map(|line| {
            let mut width = 0usize;
            for ch in line.trim_end_matches('\r').chars() {
                width += match ch {
                    '\t' => 8 - (width % 8),
                    _ => UnicodeWidthChar::width(ch).unwrap_or(0),
                };
            }
            width.max(1).div_ceil(cols) as i64
        })
        .sum::<i64>()
        .max(1)
}

fn output_display_text(text: &str) -> &str {
    let text = if let Some(stripped) = text.strip_prefix("\r\n") {
        stripped
    } else if let Some(stripped) = text.strip_prefix('\n') {
        stripped
    } else if let Some(stripped) = text.strip_prefix('\r') {
        stripped
    } else {
        text
    };

    if let Some(stripped) = text.strip_suffix("\r\n") {
        stripped
    } else if let Some(stripped) = text.strip_suffix('\n') {
        stripped
    } else if let Some(stripped) = text.strip_suffix('\r') {
        stripped
    } else {
        text
    }
}

fn line_count_text(rows: i64) -> String {
    if rows == 1 {
        "1 line".to_string()
    } else {
        format!("{rows} lines")
    }
}

fn collapsed_output_summary(rows: i64) -> String {
    format!("▸ {} hidden — click to show", line_count_text(rows))
}

fn fitted_output_rows(available_px: i32, cell_height: i32, output_rows: i64) -> i64 {
    let cell_height = cell_height.max(1);
    ((available_px.max(cell_height * 3) / cell_height) as i64).clamp(3, output_rows.max(3))
}

fn pin_vte_to_top(vte: &vte4::Terminal) {
    if let Some(adj) = vte.vadjustment() {
        adj.set_value(adj.lower());
    }
}

fn settle_vte_to_top(vte: &vte4::Terminal) {
    pin_vte_to_top(vte);
    // `feed()` updates VTE's scrollback adjustment asynchronously. The
    // immediate pin above can therefore be overwritten when a large batch
    // finishes parsing. Re-pin across two idle/layout passes so a completed
    // block consistently opens at its first line instead of at a sparse tail.
    let vte = vte.clone();
    glib::idle_add_local_once(move || {
        pin_vte_to_top(&vte);
        let vte = vte.clone();
        glib::idle_add_local_once(move || pin_vte_to_top(&vte));
    });
}

fn forward_outer_scroll(outer: &gtk::ScrolledWindow, dy: f64) {
    let outer_adj = outer.vadjustment();
    let step = outer_adj.step_increment().max(outer_adj.page_size() * 0.1);
    let max_value = (outer_adj.upper() - outer_adj.page_size()).max(outer_adj.lower());
    let target = (outer_adj.value() + dy * step).clamp(outer_adj.lower(), max_value);
    outer_adj.set_value(target);
}

/// Convert a block's viewport-relative geometry into an absolute outer-scroll
/// target. All entry points (button, shortcut, context menu, sticky header) use
/// this one calculation so long-block navigation lands identically.
fn block_edge_scroll_target(
    current: f64,
    relative_top: f64,
    block_height: f64,
    page_size: f64,
    lower: f64,
    upper: f64,
    bottom: bool,
) -> f64 {
    let max_value = (upper - page_size).max(lower);
    let absolute_top = current + relative_top;
    let target = if bottom {
        absolute_top + block_height - page_size
    } else {
        absolute_top
    };
    target.clamp(lower, max_value)
}

/// Move one adjustment for a wheel/trackpad delta. Returns true when that
/// adjustment consumed movement, allowing nested scroll surfaces to hand off
/// only at their actual top/bottom boundary.
fn scroll_adjustment(adj: &gtk::Adjustment, dy: f64) -> bool {
    let Some(target) = scroll_target(
        adj.value(),
        adj.lower(),
        adj.upper(),
        adj.page_size(),
        adj.step_increment(),
        dy,
    ) else {
        return false;
    };
    adj.set_value(target);
    true
}

fn scroll_target(
    value: f64,
    lower: f64,
    upper: f64,
    page_size: f64,
    step_increment: f64,
    dy: f64,
) -> Option<f64> {
    if dy == 0.0 {
        return None;
    }
    let step = step_increment.max(page_size * 0.1).max(1.0);
    let max_value = (upper - page_size).max(lower);
    let target = (value + dy * step).clamp(lower, max_value);
    ((target - value).abs() > f64::EPSILON).then_some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finished_block(exit_code: Option<i32>) -> BlockData {
        BlockData {
            id: 1,
            prompt: "$ ".to_string(),
            cmd: "make".to_string(),
            cmd_markup: None,
            output: "built".to_string(),
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

    #[test]
    fn an_unreported_status_never_becomes_a_zero() {
        assert_eq!(block_status(false, None), BlockStatus::Unreported);
        assert_eq!(block_status(false, Some(0)), BlockStatus::Succeeded);
        assert_eq!(block_status(false, Some(130)), BlockStatus::Failed(130));
        assert_eq!(block_status(true, None), BlockStatus::Background);
        // Background output never was a command, so its absent status is not a
        // "the shell said nothing" case.
        assert_eq!(block_status(true, Some(0)), BlockStatus::Background);

        // A number nobody reported cannot be shown, so no badge is rendered.
        assert_eq!(block_status(false, None).exit_badge(), None);
        assert_eq!(
            block_status(false, Some(130)).exit_badge().as_deref(),
            Some("exit:130")
        );
        assert_eq!(block_status(false, Some(0)).exit_badge(), None);
        // The one state a check or a cross cannot explain gets a tooltip.
        assert!(block_status(false, None).icon_tooltip().is_some());
        assert!(block_status(false, Some(0)).icon_tooltip().is_none());
        // And it is not drawn as either a success or a failure.
        for reported in [Some(0), Some(1)] {
            assert_ne!(
                block_status(false, None).stripe_class(),
                block_status(false, reported).stripe_class()
            );
            assert_ne!(
                block_status(false, None).icon(),
                block_status(false, reported).icon()
            );
        }
    }

    #[test]
    fn markdown_export_says_a_status_was_not_reported() {
        assert!(finished_block(None)
            .to_markdown()
            .contains("**Exit Code:** not reported"));
        assert!(finished_block(Some(2))
            .to_markdown()
            .contains("**Exit Code:** 2"));
    }

    #[test]
    fn markdown_export_uses_fences_longer_than_untrusted_content() {
        let mut block = finished_block(Some(0));
        block.prompt = "prompt ``` still prompt".to_owned();
        block.cmd = "printf '```'".to_owned();
        block.output = "ok\n```\n# not document markdown".to_owned();
        let markdown = block.to_markdown();
        assert!(markdown.contains("````text\nprompt ``` still prompt\n````"));
        assert!(markdown.contains("````bash\nprintf '```'\n````"));
        assert!(markdown.contains("````\nok\n```\n# not document markdown\n````"));
    }

    #[test]
    fn bounded_output_retains_exact_tail_under_a_long_stream() {
        const CHUNK: usize = 32 * 1024;
        const LIMIT: usize = 8 * 1024 * 1024;
        const CHUNKS: usize = 3_200; // 100 MiB total
        let mut output = VecDeque::new();
        for index in 0..CHUNKS {
            append_bounded_output(&mut output, &vec![(index % 251) as u8; CHUNK], LIMIT);
        }
        assert_eq!(output.len(), LIMIT);
        let retained_chunks = LIMIT / CHUNK;
        let contiguous = output.make_contiguous();
        for offset in 0..retained_chunks {
            let expected = ((CHUNKS - retained_chunks + offset) % 251) as u8;
            assert!(contiguous[offset * CHUNK..(offset + 1) * CHUNK]
                .iter()
                .all(|byte| *byte == expected));
        }

        append_bounded_output(&mut output, &vec![0x5a; LIMIT + CHUNK], LIMIT);
        assert_eq!(output.len(), LIMIT);
        assert!(output.iter().all(|byte| *byte == 0x5a));
    }

    #[test]
    fn output_row_count_ignores_one_final_line_ending() {
        assert_eq!(output_row_count("/home/tester\r\n"), 1);
        assert_eq!(output_row_count("a\nb\nc\nd\n"), 4);
    }

    #[test]
    fn output_row_count_ignores_command_enter_line_ending() {
        assert_eq!(output_row_count("\r\na\nb\nc\nd\r\n"), 4);
    }

    #[test]
    fn collapsed_summary_uses_singular_and_plural_line_counts() {
        assert_eq!(
            collapsed_output_summary(1),
            "▸ 1 line hidden — click to show"
        );
        assert_eq!(
            collapsed_output_summary(42),
            "▸ 42 lines hidden — click to show"
        );
    }

    #[test]
    fn output_row_count_preserves_intentional_blank_lines_before_final_ending() {
        assert_eq!(output_row_count("a\n\n"), 2);
    }

    #[test]
    fn visual_row_count_includes_terminal_wrapping() {
        assert_eq!(output_visual_row_count("123456789\nabc", 4), 4);
        assert_eq!(output_visual_row_count("界界界", 4), 2);
        assert_eq!(output_visual_row_count(&"x".repeat(1000), 80), 13);
    }

    #[test]
    fn visual_row_count_ignores_ansi_and_overwritten_progress_rows() {
        let apt_like = concat!(
            "\r0% [Working]",
            "\r\x1b[K\x1b[32mHit:1 repo\x1b[0m\r\n",
            "\r50% [Working]",
            "\r\x1b[KDone\r\n",
        );
        assert_eq!(output_visual_row_count(apt_like, 20), 2);
    }

    #[test]
    fn long_output_rows_fill_space_left_above_input() {
        assert_eq!(fitted_output_rows(600, 20, 500), 30);
        assert_eq!(fitted_output_rows(25, 20, 500), 3);
        assert_eq!(fitted_output_rows(600, 20, 12), 12);
    }

    #[test]
    fn inner_scroll_consumes_until_its_boundary() {
        assert_eq!(scroll_target(50.0, 0.0, 100.0, 20.0, 1.0, 1.0), Some(52.0));
        assert_eq!(scroll_target(80.0, 0.0, 100.0, 20.0, 1.0, 1.0), None);
        assert_eq!(scroll_target(80.0, 0.0, 100.0, 20.0, 1.0, -1.0), Some(78.0));
    }

    #[test]
    fn command_highlight_preserves_original_text() {
        let cmd = "git commit -m \"hello 世界\" && echo $HOME | wc -l";
        assert_eq!(strip_ansi(&highlight_command_to_ansi(cmd)), cmd);
    }

    #[test]
    fn terminalize_command_line_breaks_return_to_the_command_column() {
        assert_eq!(
            terminalize_line_breaks(b"cd /tmp\npython3 demo.py"),
            b"cd /tmp\r\npython3 demo.py"
        );
        // Preserve already-terminalized streams; ANSI formatting must not
        // disturb the newline conversion either.
        assert_eq!(
            terminalize_line_breaks(b"\x1b[36mrun\x1b[0m\r\nnext"),
            b"\x1b[36mrun\x1b[0m\r\nnext"
        );
    }

    #[test]
    fn filter_output_lines_matches_case_insensitively_without_regex() {
        assert_eq!(
            filter_output_lines("alpha\nBeta\ngamma", "BETA", false, false, false, 0),
            "Beta"
        );
    }

    #[test]
    fn block_edge_target_uses_absolute_canvas_coordinates() {
        assert_eq!(
            block_edge_scroll_target(300.0, 50.0, 800.0, 200.0, 0.0, 2000.0, false),
            350.0
        );
        assert_eq!(
            block_edge_scroll_target(300.0, 50.0, 800.0, 200.0, 0.0, 2000.0, true),
            950.0
        );
        assert_eq!(
            block_edge_scroll_target(1500.0, 100.0, 800.0, 200.0, 0.0, 1800.0, true),
            1600.0
        );
    }
}

pub(crate) fn estimated_cell_height_px(config: &Config) -> i32 {
    let parts: Vec<&str> = config.font_desc.split_whitespace().collect();
    let base_size = parts
        .last()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(14.0);
    (base_size
        * config.default_font_scale
        * (96.0 / 72.0)
        * 1.2
        * super::alt_screen::BLOCK_CELL_HEIGHT_SCALE)
        .ceil()
        .max(1.0) as i32
}

pub(crate) fn estimated_finished_block_height(config: &Config, output_rows: i64) -> i32 {
    let cell = estimated_cell_height_px(config);
    // Header + command row + output rows + margins/borders/filter slack.
    let rows = output_rows.clamp(1, i32::MAX as i64) as i32;
    rows.saturating_add(2)
        .saturating_mul(cell)
        .saturating_add(34)
}

/// Height metadata used by block virtualization must use visual terminal rows,
/// not logical newlines. A stack trace can contain one type-name line that wraps
/// thousands of times; underestimating it makes the virtualizer hide the block
/// while a large part of it is still on screen, exposing a black empty canvas.
pub(crate) fn estimated_finished_block_height_for_text(
    config: &Config,
    output: &str,
    cols: i64,
) -> i32 {
    let visible_rows = output_visual_row_count(output, cols)
        .min(config.finished_block_viewport_rows as i64)
        .max(1);
    estimated_finished_block_height(config, visible_rows)
}

fn flash_button_label(btn: &gtk::Button, label: &'static str, tooltip: &'static str) {
    let old_label = btn.label().map(|s| s.to_string()).unwrap_or_default();
    let old_tooltip = btn.tooltip_text().map(|s| s.to_string());
    btn.set_label(label);
    btn.set_tooltip_text(Some(tooltip));
    let btn_for_restore = btn.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(900), move || {
        btn_for_restore.set_label(&old_label);
        btn_for_restore.set_tooltip_text(old_tooltip.as_deref());
    });
}

/// Render `bytes` into a read-only finished VTE. Keep a generous temporary
/// scrollback while feeding: the logical/visual row estimate can still be smaller
/// than VTE's real result for cursor movement, CR redraws, combining glyphs and
/// other terminal semantics. Short blocks expand to their exact buffer height;
/// long blocks keep that scrollback inside their configured viewport.
pub(crate) fn render_bytes_into_finished_vte(
    vte: &vte4::Terminal,
    text: &str,
    cols: i64,
    output_rows: i64,
    viewport_cap: i64,
    capture_rows: i64,
    expand_to_buffer: bool,
) {
    let display_text = output_display_text(text);
    // Start from a small measuring grid. If we allocate the estimated full
    // height before feeding, VTE's adjustment cannot distinguish rows that
    // contain output from unused rows in that grid; the settling pass then
    // preserves those unused rows as a large blank tail. Overflow is retained
    // in scrollback and `settle_finished_terminal_after_feed` expands the widget
    // to the exact buffer span after VTE has processed the bytes.
    let visible_rows = output_rows.min(viewport_cap).clamp(1, 32);
    let overflow_rows = output_rows.saturating_sub(visible_rows).saturating_add(64);
    let scrollback = capture_rows.max(overflow_rows).max(64);
    vte.set_scroll_on_output(false);
    // Size and arm scrollback BEFORE reset/feed. Reset may clamp the grid on some
    // VTE builds, so both are reasserted before processing the snapshot bytes.
    vte.set_size(cols.max(1), visible_rows);
    vte.set_scrollback_lines(scrollback);
    vte.reset(true, true);
    vte.set_size(cols.max(1), visible_rows);
    vte.set_scrollback_lines(scrollback);
    vte.feed(display_text.as_bytes());
    if expand_to_buffer {
        settle_finished_terminal_after_feed(vte);
    }
    if let Some(adj) = vte.vadjustment() {
        adj.set_value(adj.lower());
    }
    settle_vte_to_top(vte);
}

/// Convert logical line breaks to terminal line breaks before feeding a VTE.
///
/// `Terminal::feed` follows terminal semantics: a bare LF moves down but keeps
/// the current column. Captured command text, however, uses ordinary Rust
/// newlines between pasted/continued input lines. Feeding those bytes directly
/// made every continuation start underneath the end of the preceding line.
fn terminalize_line_breaks(bytes: &[u8]) -> Vec<u8> {
    let extra_crs = bytes
        .iter()
        .enumerate()
        .filter(|&(i, &b)| b == b'\n' && (i == 0 || bytes[i - 1] != b'\r'))
        .count();
    if extra_crs == 0 {
        return bytes.to_vec();
    }

    let mut terminal_bytes = Vec::with_capacity(bytes.len() + extra_crs);
    for (i, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
            terminal_bytes.push(b'\r');
        }
        terminal_bytes.push(byte);
    }
    terminal_bytes
}

impl FinishedBlock {
    /// Returns the ANSI-stripped view of `full_output`, populating the cache on
    /// first call. Caller passes a closure to handle the cached string by ref to
    /// avoid an extra clone — `stripped_output` lives in a `RefCell` so we can't
    /// hand out a `Ref` that outlives the borrow.
    pub(crate) fn with_stripped_output<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        if self.stripped_output.borrow().is_none() {
            let s = strip_ansi(&self.full_output.borrow());
            *self.stripped_output.borrow_mut() = Some(s);
        }
        let guard = self.stripped_output.borrow();
        f(guard.as_deref().unwrap_or(""))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: u64,
        prompt: &str,
        cmd: &str,
        cmd_ansi: Option<&str>,
        output: &str,
        exit_code: Option<i32>,
        config: &Config,
        duration_ms: Option<u64>,
        end_time_ms: Option<u64>,
        cwd: Option<&str>,
        cols: i64,
    ) -> Self {
        Self::new_with_pool(
            id,
            prompt,
            cmd,
            cmd_ansi,
            output,
            exit_code,
            config,
            duration_ms,
            end_time_ms,
            cwd,
            cols,
            &[],
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_pool(
        id: u64,
        prompt: &str,
        cmd: &str,
        cmd_ansi: Option<&str>,
        output: &str,
        exit_code: Option<i32>,
        config: &Config,
        duration_ms: Option<u64>,
        end_time_ms: Option<u64>,
        cwd: Option<&str>,
        cols: i64,
        // Kitty-graphics textures decoded while this command ran, mounted under
        // the text output. Session history stays text-only, so restored blocks
        // pass an empty slice.
        images: &[gtk::gdk::Texture],
        recycled: Option<gtk::Box>,
    ) -> Self {
        let is_background = cmd.trim().is_empty();

        // Keep ordinary output on the outer continuous canvas, but cap very long
        // snapshots. GTK cannot allocate an arbitrarily tall single widget, so
        // long blocks retain VTE's private scrollback inside this viewport.
        let output_rows = output_visual_row_count(output, cols);
        let viewport_cap = (config.finished_block_viewport_rows as i64).max(1);
        let dynamic_viewport_rows = Rc::new(Cell::new(viewport_cap));
        let max_expanded_cap = (config.finished_block_max_expanded_rows as i64)
            .min(1000)
            .max(viewport_cap);
        let long_output = output_rows > viewport_cap;
        // Mirrors create_finished_terminal's temporary capture budget. It is a
        // limit, not an eagerly allocated grid, and is removed after each feed.
        let capture_rows = (config.truncation_threshold_lines as i64).max(4096);

        let outer = if let Some(reused) = recycled {
            while let Some(child) = reused.first_child() {
                reused.remove(&child);
            }
            reused.remove_css_class("block-hovered");
            reused.remove_css_class("block-selected");
            reused.remove_css_class("block-selection-active");
            reused.remove_css_class("block-success");
            reused.remove_css_class("block-failed");
            reused.remove_css_class("block-background");
            // A pooled widget keeps every class it was last given, so the new
            // block's status stripe would sit under the recycled one.
            reused.remove_css_class("block-unknown");
            reused
        } else {
            let b = gtk::Box::new(Orientation::Vertical, 0);
            b.add_css_class("block-finished");
            b
        };
        if config.block_compact {
            outer.add_css_class("block-compact");
            outer.set_margin_top(1);
            outer.set_margin_bottom(1);
            outer.set_margin_start(4);
            outer.set_margin_end(4);
        } else {
            outer.remove_css_class("block-compact");
            outer.set_margin_top(4);
            outer.set_margin_bottom(4);
            outer.set_margin_start(8);
            outer.set_margin_end(8);
        }

        // Status stripe: green on success, red on failure, cyan for output
        // emitted while the shell prompt was idle (Warp background blocks),
        // neutral when the shell never reported a status.
        let status = block_status(is_background, exit_code);
        outer.add_css_class(status.stripe_class());

        // Add hover highlighting to show block is interactive (and reveal the
        // quick-action buttons). The action box is created below; it's wired into
        // these handlers after construction.
        let hover_ctrl = gtk::EventControllerMotion::new();

        // ── Header row ──────────────────────────────────────────────────────
        let header_row = gtk::Box::new(Orientation::Horizontal, 8);
        header_row.add_css_class("block-header");
        // The output surface keeps VTE's native text selection, so selection
        // lives on this header strip. Make the otherwise subtle interaction
        // discoverable without adding permanent visual chrome to every block.
        header_row.set_tooltip_text(Some(if is_background {
            "Click to select · Shift-click range · Ctrl+Shift-click toggle"
        } else {
            "Click to select · Shift-click range · Ctrl+Shift-click toggle · Enter recalls"
        }));
        if config.block_compact {
            header_row.set_margin_start(8);
            header_row.set_margin_end(6);
            header_row.set_margin_top(3);
            header_row.set_margin_bottom(1);
        } else {
            header_row.set_margin_start(12);
            header_row.set_margin_end(8);
            header_row.set_margin_top(6);
            header_row.set_margin_bottom(2);
        }

        // Bookmark star (gutter marker), hidden until the block is bookmarked.
        let bookmark_star = gtk::Label::new(Some("\u{f02e}")); // nf-fa-bookmark
        bookmark_star.add_css_class("block-bookmark-star");
        bookmark_star.set_halign(gtk::Align::Start);
        bookmark_star.set_visible(false);
        header_row.append(&bookmark_star);

        // Status icon: ✓ for success, ✗ for failure, spinner for an
        // asynchronous/background block, ? when the shell reported nothing.
        let (status_glyph, status_class) = status.icon();
        let status_icon = gtk::Label::new(Some(status_glyph));
        status_icon.add_css_class(status_class);
        status_icon.set_tooltip_text(status.icon_tooltip());
        status_icon.set_halign(gtk::Align::Start);
        header_row.append(&status_icon);

        if is_background {
            let background_chip = gtk::Label::new(Some("Background output"));
            background_chip.add_css_class("block-background-chip");
            background_chip.set_halign(gtk::Align::Start);
            header_row.append(&background_chip);
        }

        // Context chips (Warp-style): cwd pill + git-branch pill.
        if let Some(cwd_path) = cwd {
            let shortened = shorten_path(cwd_path);
            // nf-fa-folder () prefix
            let cwd_chip = gtk::Label::new(Some(&format!("\u{f07b} {}", shortened)));
            cwd_chip.add_css_class("block-chip");
            cwd_chip.set_halign(gtk::Align::Start);
            cwd_chip.set_ellipsize(gtk::pango::EllipsizeMode::Start);
            cwd_chip.set_max_width_chars(40);
            header_row.append(&cwd_chip);

            // git-branch chip (nf-dev-git-branch )
            if let Some(branch) = git_branch_for(cwd_path) {
                let git_chip = gtk::Label::new(Some(&format!("\u{e725} {}", branch)));
                git_chip.add_css_class("block-chip-git");
                git_chip.set_halign(gtk::Align::Start);
                git_chip.set_ellipsize(gtk::pango::EllipsizeMode::End);
                git_chip.set_max_width_chars(28);
                header_row.append(&git_chip);
            }
        }

        // Spacer
        let spacer = gtk::Box::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        header_row.append(&spacer);

        // Timestamp label
        if let Some(et_ms) = end_time_ms {
            let secs = et_ms / 1000;
            let local_offset = chrono_local_offset_secs();
            let local_secs = (secs as i64 + local_offset).rem_euclid(86400) as u64;
            let h = local_secs / 3600;
            let m = (local_secs % 3600) / 60;
            let sec = local_secs % 60;
            let ts_label = gtk::Label::new(Some(&format!("{:02}:{:02}:{:02}", h, m, sec)));
            ts_label.add_css_class("block-header-label");
            header_row.append(&ts_label);
        }

        // Duration badge
        if let Some(dur_ms) = duration_ms {
            let dur_sec = dur_ms as f64 / 1000.0;
            let duration_text = if dur_sec < 1.0 {
                format!("{:.0}ms", dur_ms)
            } else if dur_sec < 60.0 {
                format!("{:.1}s", dur_sec)
            } else {
                let min = dur_sec / 60.0;
                format!("{:.0}m", min)
            };
            let dur_label = gtk::Label::new(Some(&duration_text));
            dur_label.add_css_class("block-meta-badge");
            header_row.append(&dur_label);
        }

        // Exit code badge
        if let Some(text) = status.exit_badge() {
            let badge = gtk::Label::new(Some(&text));
            badge.add_css_class("block-exit-bad");
            header_row.append(&badge);
        }

        // Quick-action buttons (hidden until the block is hovered). Handlers are
        // wired by the caller, which has access to the clipboard + active block.
        let action_box = gtk::Box::new(Orientation::Horizontal, 2);
        action_box.set_visible(false);
        // Small gap between the meta badges (timestamp/duration/exit) on the
        // right and the action button group, so they read as separate units
        // rather than one undifferentiated cluster.
        action_box.set_margin_start(6);
        let copy_cmd_btn = gtk::Button::with_label("\u{f0c5}"); // nf-fa-copy  copy command
        copy_cmd_btn.set_tooltip_text(Some("Copy command"));
        let copy_output_btn = gtk::Button::with_label("\u{f0ea}"); // nf-fa-clipboard  copy output
        copy_output_btn.set_tooltip_text(Some("Copy output"));
        let rerun_btn = gtk::Button::with_label("\u{f021}"); // nf-fa-refresh  recall command
        rerun_btn.set_tooltip_text(Some("Insert command at prompt"));
        // Commandless background blocks retain output actions, find/filter,
        // bookmarks and selection, but cannot copy or recall a command.
        copy_cmd_btn.set_visible(!is_background);
        rerun_btn.set_visible(!is_background);
        let filter_btn = gtk::Button::with_label("\u{f0b0}"); // nf-fa-filter  filter output
        filter_btn.set_tooltip_text(Some("Filter output"));
        let jump_bottom_btn = gtk::Button::with_label("\u{f103}"); // nf-fa-angle-double-down
        jump_bottom_btn.set_tooltip_text(Some("Jump to bottom of this block"));
        jump_bottom_btn.set_visible(long_output);
        // Expand button: appears only when output_rows > viewport_cap; toggles
        // the output VTE between the capped height and a roomier expanded height
        // (`finished_block_max_expanded_rows`). Wired below once output_rows and
        // the output VTE exist.
        let expand_btn = gtk::Button::with_label("\u{f065}"); // nf-fa-expand
        expand_btn.set_tooltip_text(Some("Expand block"));
        for btn in [
            &copy_cmd_btn,
            &copy_output_btn,
            &rerun_btn,
            &filter_btn,
            &jump_bottom_btn,
            &expand_btn,
        ] {
            btn.add_css_class("block-action-btn");
            btn.add_css_class("flat");
            action_box.append(btn);
        }
        header_row.append(&action_box);

        let outer_for_enter = outer.clone();
        let action_box_for_enter = action_box.clone();
        hover_ctrl.connect_enter(move |_, _, _| {
            outer_for_enter.add_css_class("block-hovered");
            action_box_for_enter.set_visible(true);
        });
        let outer_for_leave = outer.clone();
        let action_box_for_leave = action_box.clone();
        hover_ctrl.connect_leave(move |_| {
            outer_for_leave.remove_css_class("block-hovered");
            // Only the active edge of a multi-selection owns persistent actions.
            if !outer_for_leave.has_css_class("block-selection-active") {
                action_box_for_leave.set_visible(false);
            }
        });
        outer.add_controller(hover_ctrl);

        // Collapse toggle button
        let collapse_btn = gtk::Button::with_label("\u{f078}"); // nf-fa-chevron_down
        collapse_btn.add_css_class("block-collapse-btn");
        collapse_btn.add_css_class("flat");
        header_row.append(&collapse_btn);

        outer.append(&header_row);

        // ── VTE-rendered command + output ─────────────────────────────────
        // Command VTE: single-row read-only renderer for the executed command.
        let cmd_bytes: Vec<u8> = match cmd_ansi {
            Some(ansi) if !ansi.is_empty() && !cmd.is_empty() => ansi.as_bytes().to_vec(),
            _ if cmd.is_empty() => b"(empty)".to_vec(),
            _ => highlight_command_to_ansi(cmd).into_bytes(),
        };
        // Captured command strings use logical newlines. Convert them to CRLF
        // for VTE so every pasted/continued line begins at the command column.
        let cmd_bytes = terminalize_line_breaks(&cmd_bytes);
        // Allocate every logical command row up front; VTE's post-feed pass
        // adds any further rows caused by soft wrapping or control sequences.
        let cmd_rows = cmd_bytes.iter().filter(|&&b| b == b'\n').count() as i64 + 1;
        let command_vte =
            create_finished_terminal(config, cols, cmd_rows.max(1), cmd_rows.max(1), true);
        // Defer feeds until the widget is actually mapped — VTE's internal
        // grid resize from set_size() doesn't take effect until the widget is
        // realized, so feeding immediately wraps content at a smaller default
        // width (the ls-output misalignment bug). connect_map fires once the
        // widget has been allocated, when the grid actually matches set_size.
        // One-shot: re-mapping during scroll must not re-feed.
        {
            let cmd_bytes_for_map = cmd_bytes.clone();
            let cols_for_map = cols.max(1);
            let cmd_rows_for_map = cmd_rows.max(1);
            let fed = Cell::new(false);
            command_vte.connect_map(move |w| {
                if fed.get() {
                    return;
                }
                fed.set(true);
                w.set_size(cols_for_map, cmd_rows_for_map);
                w.feed(&cmd_bytes_for_map);
                // Gtk may otherwise allocate this VTE at one row, leaving the
                // continuation lines in its internal scrollback.
                let ch = w.char_height() as i32;
                if ch > 0 {
                    w.set_height_request(cmd_rows_for_map as i32 * ch);
                }
            });
        }

        // Output VTE: full output fed once on first map and allocated at its
        // complete wrapped height. The outer ScrolledWindow is the sole
        // vertical canvas; `displayed_output` keeps filter re-renders in sync.
        let full_output: Rc<RefCell<String>> = Rc::new(RefCell::new(output.to_string()));
        let displayed_output: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let output_scrollable = output_rows > viewport_cap;
        let initial_visible_rows = output_rows.min(viewport_cap).clamp(1, 32);
        let output_vte = create_finished_terminal(
            config,
            cols,
            initial_visible_rows,
            initial_visible_rows,
            !output_scrollable,
        );
        output_vte
            .set_height_request(initial_visible_rows as i32 * estimated_cell_height_px(config));
        // Tracks whether the user has toggled this block to its expanded
        // height. Survives unmap/remap so re-feeding picks the right cap.
        let expanded: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        {
            let cols_for_map = cols.max(1);
            let cap_for_map = dynamic_viewport_rows.clone();
            let max_for_map = max_expanded_cap;
            let full_for_map = full_output.clone();
            let displayed_for_map = displayed_output.clone();
            let expanded_for_map = expanded.clone();
            let fed = Cell::new(false);
            output_vte.connect_map(move |w| {
                if fed.get() {
                    if !output_scrollable {
                        pin_vte_to_top(w);
                        let w = w.clone();
                        glib::idle_add_local_once(move || pin_vte_to_top(&w));
                    }
                    return;
                }
                fed.set(true);
                let full = full_for_map.borrow();
                let displayed = displayed_for_map.borrow();
                let text = displayed.as_deref().unwrap_or(full.as_str());
                let rows = output_visual_row_count(text, cols_for_map);
                let cap = if expanded_for_map.get() {
                    max_for_map
                } else {
                    cap_for_map.get()
                };
                let visible_rows = rows.min(cap).clamp(1, 32);
                render_bytes_into_finished_vte(
                    w,
                    text,
                    cols_for_map,
                    rows,
                    cap,
                    capture_rows,
                    !output_scrollable,
                );
                // Pin a minimum pixel height so GTK's vertical Box layout cannot
                // shrink this VTE below what set_size requested. Without this,
                // finished VTEs can be allocated at ~1 row and VTE scrolls their
                // content into internal scrollback. Do not clear on unmap: GTK
                // virtual scrolling and ordinary layout churn can unmap visible
                // blocks transiently, and clearing there loses output if a later
                // remap is skipped or coalesced.
                let ch = w.char_height() as i32;
                if ch > 0 {
                    w.set_height_request((visible_rows as i32) * ch);
                }
                if !output_scrollable {
                    pin_vte_to_top(w);
                    let w = w.clone();
                    glib::idle_add_local_once(move || pin_vte_to_top(&w));
                }
            });
        }

        // Show the expand toggle only when there's content beyond the cap.
        // Click swaps the output VTE between capped and expanded heights and
        // updates the icon (expand ↔ compress). The map handler reads the
        // shared `expanded` flag so a re-feed after scroll-off/on respects it.
        if output_rows > viewport_cap {
            let expand_for_btn = expanded.clone();
            let viewport_for_btn = dynamic_viewport_rows.clone();
            let output_vte_for_btn = output_vte.clone();
            let full_for_btn = full_output.clone();
            let displayed_for_btn = displayed_output.clone();
            let cols_for_btn = cols.max(1);
            expand_btn.connect_clicked(move |btn| {
                let now_expanded = !expand_for_btn.get();
                expand_for_btn.set(now_expanded);
                let cap = if now_expanded {
                    max_expanded_cap
                } else {
                    viewport_for_btn.get()
                };
                let full = full_for_btn.borrow();
                let displayed = displayed_for_btn.borrow();
                let text = displayed.as_deref().unwrap_or(full.as_str());
                let rows = output_visual_row_count(text, cols_for_btn);
                let visible_rows = rows.min(cap).max(1);
                output_vte_for_btn.set_size(cols_for_btn, visible_rows);
                let ch = output_vte_for_btn.char_height() as i32;
                if ch > 0 {
                    output_vte_for_btn.set_height_request((visible_rows as i32) * ch);
                }
                btn.set_label(if now_expanded { "\u{f066}" } else { "\u{f065}" });
                btn.set_tooltip_text(Some(if now_expanded {
                    "Collapse to default height"
                } else {
                    "Expand block"
                }));
            });
        } else {
            expand_btn.set_visible(false);
        }

        // Command row: Warp-style accent prompt chevron + the command VTE.
        let cmd_row = gtk::Box::new(Orientation::Horizontal, 0);
        let chevron = gtk::Label::new(Some("\u{276f}")); // ❯
        chevron.add_css_class("block-prompt-chevron");
        chevron.set_valign(gtk::Align::Start);
        cmd_row.append(&chevron);
        cmd_row.append(&command_vte);

        outer.append(&cmd_row);
        cmd_row.set_visible(!is_background);
        let output_box = gtk::Box::new(Orientation::Horizontal, 0);
        output_box.set_hexpand(true);
        output_box.append(&output_vte);
        let output_scrollbar =
            gtk::Scrollbar::new(Orientation::Vertical, output_vte.vadjustment().as_ref());
        output_scrollbar.add_css_class("block-output-scrollbar");
        output_scrollbar.set_visible(output_scrollable);
        output_scrollbar.set_tooltip_text(Some("Scroll within this block"));
        output_box.append(&output_scrollbar);
        let output_widget: gtk::Widget = output_box.clone().upcast::<gtk::Widget>();
        outer.append(&output_box);

        // Kitty graphics: append each decoded texture as a Picture under the
        // text output. Pictures preserve aspect ratio inside a max-height bound
        // so a tall plot doesn't push the next block off-screen; one shared box
        // lets the collapse chevron hide them together with the text output.
        let images_box: Option<gtk::Box> = if images.is_empty() {
            None
        } else {
            let ib = gtk::Box::new(Orientation::Vertical, 4);
            ib.add_css_class("block-images");
            ib.set_margin_start(18);
            ib.set_margin_end(8);
            ib.set_margin_bottom(4);
            for tex in images {
                let pic = gtk::Picture::for_paintable(tex);
                pic.set_can_shrink(true);
                pic.set_content_fit(gtk::ContentFit::Contain);
                pic.set_halign(gtk::Align::Start);
                // Cap displayed height so plots/screenshots stay within ~25
                // rows of block real estate; the outer history scrolls past.
                pic.set_size_request(-1, tex.height().clamp(64, 600));
                ib.append(&pic);
            }
            outer.append(&ib);
            Some(ib)
        };

        let collapsed_summary = gtk::Button::with_label(&collapsed_output_summary(output_rows));
        collapsed_summary.add_css_class("block-output-summary");
        collapsed_summary.add_css_class("flat");
        collapsed_summary.set_halign(gtk::Align::Start);
        collapsed_summary.set_margin_start(18);
        collapsed_summary.set_margin_end(8);
        collapsed_summary.set_margin_bottom(4);
        collapsed_summary.set_tooltip_text(Some("Show block output"));
        collapsed_summary.set_visible(false);
        outer.append(&collapsed_summary);

        // Ctrl+click on a URL inside the output VTE → open in browser.
        // VTE's `match_add_regex` (registered in create_finished_terminal) makes
        // `check_match_at` return the matching URL at the pointer position;
        // VTE handles word/line double/triple-click selection natively.
        {
            let click = gtk::GestureClick::new();
            click.set_button(1);
            let vte_for_click = output_vte.downgrade();
            click.connect_pressed(move |controller, n_press, x, y| {
                if n_press != 1 {
                    return;
                }
                let state = controller.current_event_state();
                if !state.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                    return;
                }
                let Some(vte_for_click) = vte_for_click.upgrade() else {
                    return;
                };
                let (uri, _tag) = vte_for_click.check_match_at(x, y);
                if let Some(uri) = uri {
                    let s = uri.to_string();
                    if !s.is_empty() {
                        open_uri(&s);
                        controller.set_state(gtk::EventSequenceState::Claimed);
                    }
                }
            });
            output_vte.add_controller(click);
        }

        let has_output = !output.trim().is_empty();
        let has_images = images_box.is_some();
        if !has_output {
            output_widget.set_visible(false);
        }
        // Image-only commands (`kitten icat` with no text output) still keep a
        // working chevron so their Pictures can be folded away.
        if !has_output && !has_images {
            collapse_btn.set_sensitive(false);
            collapse_btn.set_tooltip_text(Some("No output"));
        } else if has_output {
            collapse_btn.set_tooltip_text(Some(&format!(
                "Toggle output ({})",
                line_count_text(output_rows)
            )));
        }
        let set_collapsed: Rc<dyn Fn(bool)> = {
            let output_widget = output_widget.downgrade();
            let collapsed_summary = collapsed_summary.downgrade();
            let collapse_btn = collapse_btn.downgrade();
            let images_box = images_box.as_ref().map(|ib| ib.downgrade());
            Rc::new(move |collapsed| {
                let (Some(output_widget), Some(collapsed_summary), Some(collapse_btn)) = (
                    output_widget.upgrade(),
                    collapsed_summary.upgrade(),
                    collapse_btn.upgrade(),
                ) else {
                    return;
                };
                // Image-only blocks keep their empty output VTE hidden even
                // while expanded; only the Pictures fold and unfold.
                output_widget.set_visible(!collapsed && has_output);
                if let Some(ib) = images_box.as_ref().and_then(|ib| ib.upgrade()) {
                    ib.set_visible(!collapsed);
                }
                collapsed_summary.set_visible(collapsed);
                collapse_btn.set_label(if collapsed { "\u{f054}" } else { "\u{f078}" });
                collapse_btn.set_tooltip_text(Some(if collapsed {
                    "Show output"
                } else {
                    "Hide output"
                }));
            })
        };
        {
            let set_collapsed = set_collapsed.clone();
            // The summary's visibility is the one folded-state signal that also
            // works for image-only blocks, whose output VTE stays hidden even
            // while expanded.
            let collapsed_summary = collapsed_summary.downgrade();
            collapse_btn.connect_clicked(move |_| {
                if let Some(collapsed_summary) = collapsed_summary.upgrade() {
                    set_collapsed(!collapsed_summary.is_visible());
                }
            });
        }
        {
            let set_collapsed = set_collapsed.clone();
            collapsed_summary.connect_clicked(move |_| set_collapsed(false));
        }
        if !has_output && !has_images {
            collapse_btn.set_label("\u{f054}"); // nf-fa-chevron_right
            collapsed_summary.set_visible(false);
        }

        // Per-block output filter (Warp's BlockFilterQuery). Closing the
        // editor disables filtering but deliberately preserves the query/options;
        // reopening it reapplies the same filter, matching Warp's toggle behavior.
        let filter_enabled = Rc::new(Cell::new(false));
        let toggle_filter: Rc<dyn Fn()> = {
            let filter_row = gtk::Box::new(Orientation::Horizontal, 4);
            filter_row.add_css_class("block-filter-row");
            filter_row.set_visible(false);
            filter_row.set_margin_start(12);
            filter_row.set_margin_end(8);
            filter_row.set_margin_top(2);
            filter_row.set_margin_bottom(2);

            let filter_entry = gtk::SearchEntry::new();
            filter_entry.set_placeholder_text(Some("Filter output…"));
            filter_entry.set_hexpand(true);
            let regex_tg = gtk::ToggleButton::with_label(".*");
            regex_tg.set_tooltip_text(Some("Regular expression"));
            let case_tg = gtk::ToggleButton::with_label("Aa");
            case_tg.set_tooltip_text(Some("Case sensitive"));
            let invert_tg = gtk::ToggleButton::with_label("!");
            invert_tg.set_tooltip_text(Some("Invert match (hide matching lines)"));
            let ctx_spin = gtk::SpinButton::with_range(0.0, 9.0, 1.0);
            ctx_spin.set_tooltip_text(Some("Lines of context around each match"));
            ctx_spin.set_value(0.0);
            let filter_status = gtk::Label::new(None);
            filter_status.add_css_class("block-filter-status");
            filter_status.set_halign(gtk::Align::Start);
            for w in [&regex_tg, &case_tg, &invert_tg] {
                w.add_css_class("flat");
                w.add_css_class("block-filter-toggle");
            }
            filter_row.append(&filter_entry);
            filter_row.append(&regex_tg);
            filter_row.append(&case_tg);
            filter_row.append(&invert_tg);
            filter_row.append(&ctx_spin);
            filter_row.append(&filter_status);

            outer.append(&filter_row);
            outer.reorder_child_after(&filter_row, Some(&cmd_row));

            let apply = {
                let output_vte = output_vte.downgrade();
                let full_output = full_output.clone();
                let displayed_output = displayed_output.clone();
                let filter_entry = filter_entry.downgrade();
                let regex_tg = regex_tg.downgrade();
                let case_tg = case_tg.downgrade();
                let invert_tg = invert_tg.downgrade();
                let ctx_spin = ctx_spin.downgrade();
                let filter_status = filter_status.downgrade();
                let expand_btn = expand_btn.downgrade();
                let output_scrollbar = output_scrollbar.downgrade();
                let expanded = expanded.clone();
                let dynamic_viewport_rows = dynamic_viewport_rows.clone();
                let collapsed_summary = collapsed_summary.downgrade();
                let filter_enabled = filter_enabled.clone();
                move || {
                    let (
                        Some(output_vte),
                        Some(filter_entry),
                        Some(regex_tg),
                        Some(case_tg),
                        Some(invert_tg),
                        Some(ctx_spin),
                        Some(filter_status),
                        Some(expand_btn),
                        Some(output_scrollbar),
                        Some(collapsed_summary),
                    ) = (
                        output_vte.upgrade(),
                        filter_entry.upgrade(),
                        regex_tg.upgrade(),
                        case_tg.upgrade(),
                        invert_tg.upgrade(),
                        ctx_spin.upgrade(),
                        filter_status.upgrade(),
                        expand_btn.upgrade(),
                        output_scrollbar.upgrade(),
                        collapsed_summary.upgrade(),
                    )
                    else {
                        return;
                    };
                    let q = filter_entry.text().to_string();
                    let full = full_output.borrow();
                    let full_rows = output_row_count(&full);
                    let shown = if !filter_enabled.get() || q.is_empty() {
                        full.to_string()
                    } else {
                        filter_output_lines(
                            full.as_str(),
                            &q,
                            regex_tg.is_active(),
                            case_tg.is_active(),
                            invert_tg.is_active(),
                            ctx_spin.value() as usize,
                        )
                    };
                    let shown_rows = output_row_count(&shown);
                    let shown_visual_rows = output_visual_row_count(&shown, cols);
                    let active_cap = if expanded.get() {
                        max_expanded_cap
                    } else {
                        dynamic_viewport_rows.get()
                    };
                    render_bytes_into_finished_vte(
                        &output_vte,
                        &shown,
                        cols,
                        shown_visual_rows,
                        active_cap,
                        capture_rows,
                        shown_visual_rows <= active_cap,
                    );
                    let ch = output_vte.char_height() as i32;
                    if ch > 0 {
                        let probe_rows = shown_visual_rows.min(active_cap).clamp(1, 32);
                        output_vte.set_height_request((probe_rows as i32) * ch);
                    }
                    let has_query = filter_enabled.get() && !q.trim().is_empty();
                    if has_query {
                        filter_status.set_visible(true);
                        let hidden = full_rows.saturating_sub(shown_rows);
                        if shown.trim().is_empty() {
                            filter_status.set_text("No matches");
                            filter_status.add_css_class("block-filter-empty");
                        } else {
                            filter_status.remove_css_class("block-filter-empty");
                            filter_status.set_text(&format!(
                                "{} shown, {} hidden",
                                line_count_text(shown_rows),
                                hidden
                            ));
                        }
                    } else {
                        filter_status.remove_css_class("block-filter-empty");
                        filter_status.set_visible(false);
                    }
                    expand_btn.set_visible(shown_visual_rows > dynamic_viewport_rows.get());
                    output_scrollbar.set_visible(shown_visual_rows > active_cap);
                    collapsed_summary.set_label(&collapsed_output_summary(shown_rows));
                    let display_override = if shown == full.as_str() {
                        None
                    } else {
                        Some(shown)
                    };
                    *displayed_output.borrow_mut() = display_override;
                }
            };
            let apply = Rc::new(apply);
            {
                let a = apply.clone();
                filter_entry.connect_search_changed(move |_| a());
            }
            for tg in [&regex_tg, &case_tg, &invert_tg] {
                let a = apply.clone();
                tg.connect_toggled(move |_| a());
            }
            {
                let a = apply.clone();
                ctx_spin.connect_value_changed(move |_| a());
            }

            let filter_row_for_toggle = filter_row.downgrade();
            let entry_for_toggle = filter_entry.downgrade();
            let apply_for_toggle = apply.clone();
            let filter_btn_for_toggle = filter_btn.downgrade();
            let filter_enabled_for_toggle = filter_enabled.clone();
            let toggle: Rc<dyn Fn()> = Rc::new(move || {
                let (
                    Some(filter_row_for_toggle),
                    Some(entry_for_toggle),
                    Some(filter_btn_for_toggle),
                ) = (
                    filter_row_for_toggle.upgrade(),
                    entry_for_toggle.upgrade(),
                    filter_btn_for_toggle.upgrade(),
                )
                else {
                    return;
                };
                let show = !filter_row_for_toggle.is_visible();
                filter_row_for_toggle.set_visible(show);
                filter_enabled_for_toggle.set(show);
                if show {
                    filter_btn_for_toggle.add_css_class("block-action-active");
                    entry_for_toggle.grab_focus();
                } else {
                    filter_btn_for_toggle.remove_css_class("block-action-active");
                }
                apply_for_toggle();
            });
            let toggle_for_button = toggle.clone();
            filter_btn.connect_clicked(move |_| toggle_for_button());
            toggle
        };

        FinishedBlock {
            id,
            is_background,
            widget: outer,
            prompt_text: prompt.to_string(),
            command_vte,
            output_vte,
            output_scrollbar,
            full_output,
            displayed_output,
            stripped_output: Rc::new(RefCell::new(None)),
            cmd_text: cmd.to_string(),
            copy_cmd_btn,
            copy_output_btn,
            rerun_btn,
            header_row,
            action_box,
            toggle_filter,
            jump_bottom_btn,
            bookmark_star,
            status_icon,
            cols,
            viewport_cap,
            dynamic_viewport_rows,
            output_rows,
            expanded,
            expand_btn,
            output_scrollable,
            long_output,
        }
    }

    pub(crate) fn widget(&self) -> &gtk::Box {
        &self.widget
    }

    /// Scroll this block's command edge or output edge into the outer history
    /// canvas. The child VTEs never own this navigation.
    pub(crate) fn scroll_to_edge(&self, outer: &gtk::ScrolledWindow, bottom: bool) {
        let widget = self.widget.clone();
        let outer = outer.clone();
        glib::idle_add_local_once(move || {
            let Some(bounds) = widget.compute_bounds(&outer) else {
                return;
            };
            let adj = outer.vadjustment();
            let target = block_edge_scroll_target(
                adj.value(),
                bounds.y() as f64,
                bounds.height() as f64,
                adj.page_size(),
                adj.lower(),
                adj.upper(),
                bottom,
            );
            adj.set_value(target);
        });
    }

    /// Size a long output surface to the pane space left above the live input
    /// block. Returns the visible row count when this is a dynamically-sized
    /// block so the virtualization metadata can stay in lockstep with GTK.
    pub(crate) fn fit_output_to_height(&self, available_px: i32) -> Option<i64> {
        if !self.output_scrollable {
            return None;
        }
        let cell_height = (self.output_vte.char_height() as i32).max(1);
        let rows = fitted_output_rows(available_px, cell_height, self.output_rows);
        if self.dynamic_viewport_rows.replace(rows) == rows {
            return Some(rows);
        }

        // Dynamic pane sizing is authoritative; leave the manual expanded
        // state when the window or input block changes size.
        self.expanded.set(false);
        self.expand_btn.set_label("\u{f065}");
        self.expand_btn.set_tooltip_text(Some("Expand block"));
        self.output_vte.set_size(self.cols.max(1), rows);
        self.output_vte
            .set_height_request((rows as i32).saturating_mul(cell_height));
        self.output_scrollbar.set_visible(self.output_rows > rows);
        self.output_vte.queue_allocate();
        Some(rows)
    }

    /// Give a long block first refusal on wheel events while the pointer is over
    /// either its output or its scrollbar. Only hand off to the outer history
    /// canvas after the inner adjustment reaches the requested boundary.
    pub(crate) fn connect_scroll_forwarding(&self, outer: &gtk::ScrolledWindow) {
        let block_for_jump = self.clone();
        let outer_for_jump = outer.clone();
        self.jump_bottom_btn.connect_clicked(move |_| {
            block_for_jump.scroll_to_edge(&outer_for_jump, true);
        });

        let targets: [gtk::Widget; 2] = [
            self.output_vte.clone().upcast(),
            self.output_scrollbar.clone().upcast(),
        ];
        for target in targets {
            let scroll_ctrl =
                gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
            // Capture before VTE and the ancestor ScrolledWindow can both react
            // to the same event. Slider dragging is a pointer gesture and stays
            // native to gtk::Scrollbar.
            scroll_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
            let vte = self.output_vte.downgrade();
            let outer_for_vte = outer.downgrade();
            let output_scrollable = self.output_scrollable;
            scroll_ctrl.connect_scroll(move |_, _dx, dy| {
                let (Some(vte), Some(outer_for_vte)) = (vte.upgrade(), outer_for_vte.upgrade())
                else {
                    return glib::Propagation::Proceed;
                };
                if output_scrollable {
                    if let Some(inner_adj) = vte.vadjustment() {
                        if scroll_adjustment(&inner_adj, dy) {
                            return glib::Propagation::Stop;
                        }
                    }
                } else {
                    pin_vte_to_top(&vte);
                }
                forward_outer_scroll(&outer_for_vte, dy);
                glib::Propagation::Stop
            });
            target.add_controller(scroll_ctrl);
        }
    }

    /// Wire the hover quick-action buttons (copy command, copy output, recall).
    /// Kept separate from construction because handlers need the clipboard, PTY,
    /// shell input state, and active block, which only the owning `TermView` has.
    // These arguments are the distinct shared state cells captured by GTK
    // callbacks; grouping them would only hide the ownership contract.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn connect_actions(
        &self,
        vte: &Terminal,
        pty: &Rc<crate::pty::OwnedPty>,
        pty_synced: &Rc<Cell<bool>>,
        bracketed_paste: &Rc<Cell<bool>>,
        typed_cmd: &Rc<RefCell<String>>,
        armed_agent_execution: &Rc<RefCell<Option<super::ArmedAgentExecution>>>,
        bstate: &Rc<Cell<BlockState>>,
        active: &Rc<RefCell<ActiveBlock>>,
    ) {
        let vte_for_cmd = vte.clone();
        let cmd_for_copy = self.cmd_text.clone();
        self.copy_cmd_btn.connect_clicked(move |btn| {
            vte_for_cmd.clipboard().set_text(&cmd_for_copy);
            flash_button_label(btn, "\u{f00c}", "Command copied");
        });

        let vte_for_out = vte.clone();
        // Copy the FULL output (ANSI stripped), not just the collapsed first-N
        // lines shown in output_buffer before "Show more" is clicked.
        let full_output_for_copy = self.full_output.clone();
        self.copy_output_btn.connect_clicked(move |btn| {
            let text = strip_ansi(&full_output_for_copy.borrow());
            vte_for_out.clipboard().set_text(&text);
            flash_button_label(btn, "\u{f00c}", "Output copied");
        });

        let pty_for_rerun = Rc::clone(pty);
        let pty_synced_for_rerun = pty_synced.clone();
        let bracketed_paste_for_rerun = bracketed_paste.clone();
        let typed_cmd_for_rerun = typed_cmd.clone();
        let armed_agent_for_rerun = armed_agent_execution.clone();
        let bstate_for_rerun = bstate.clone();
        let active_for_rerun = active.clone();
        let cmd_for_rerun = self.cmd_text.clone();
        self.rerun_btn.connect_clicked(move |btn| {
            if recall_command_at_prompt(
                &pty_for_rerun,
                &pty_synced_for_rerun,
                &typed_cmd_for_rerun,
                bstate_for_rerun.get(),
                armed_agent_for_rerun.borrow().is_some(),
                &cmd_for_rerun,
                bracketed_paste_for_rerun.get(),
            ) {
                active_for_rerun.borrow().grab_focus();
                flash_button_label(btn, "\u{f00c}", "Command inserted");
            } else {
                flash_button_label(btn, "\u{f071}", "Wait for an editable prompt");
            }
        });
    }
}

// ─── ActiveBlock ──────────────────────────────────────────────────────────────

/// The live area: a single persistent input-enabled VTE pinned to the viewport
/// height (anvil model). The shell's prompt, the user's typing, and command
/// output all render natively in this one VTE. When a command finishes, its
/// accumulated output (`raw_output`) is snapshotted into a styled FinishedBlock
/// stacked above this card.
pub(crate) struct ActiveBlock {
    pub(crate) widget: gtk::Box,
    pub(crate) active_vte: Terminal,
    /// Raw output bytes accumulated during CollectingOutput, consumed by the
    /// finalize path to build the styled finished block (anvil's `out_buf`).
    pub(crate) raw_output: Rc<RefCell<VecDeque<u8>>>,
}

/// Retain only the newest `limit` bytes without repeatedly shifting the whole
/// retained buffer. A `VecDeque` makes long-running output proportional to the
/// new bytes discarded instead of copying the full multi-megabyte tail for
/// every PTY chunk.
pub(super) fn append_bounded_output(buffer: &mut VecDeque<u8>, bytes: &[u8], limit: usize) {
    if limit == 0 {
        buffer.clear();
        return;
    }
    if bytes.len() >= limit {
        buffer.clear();
        buffer.extend(bytes[bytes.len() - limit..].iter().copied());
        return;
    }
    let overflow = buffer
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(limit);
    if overflow > 0 {
        buffer.drain(..overflow);
    }
    buffer.extend(bytes.iter().copied());
}

impl ActiveBlock {
    pub(crate) fn new(config: &Config) -> Self {
        let widget = gtk::Box::new(Orientation::Vertical, 0);
        widget.add_css_class("block-active");
        if config.block_compact {
            widget.add_css_class("block-compact");
        }
        // focusable(false) keeps the holder Box from being a focus target, but we
        // must NOT set can_focus(false): in GTK4 that blocks all descendants
        // (including active_vte) from ever receiving focus.
        widget.set_focusable(false);
        widget.set_hexpand(true);
        // NOT vexpand: the input cell hugs its content (warp model). Its exact
        // height is driven by `update_input_height` in block_view/mod.rs via
        // height_request. With vexpand the cell would fill the whole viewport
        // regardless of the requested height.
        widget.set_vexpand(false);

        let active_vte = create_active_terminal(config);
        active_vte.set_hexpand(true);
        active_vte.set_vexpand(false);
        widget.append(&active_vte);

        ActiveBlock {
            widget,
            active_vte,
            raw_output: Rc::new(RefCell::new(VecDeque::new())),
        }
    }

    /// Append raw command-output bytes to the snapshot buffer (bounded). The bytes
    /// are also fed to the live VTE separately by the reader; this buffer is only
    /// the source the finalize path styles into a finished block.
    pub(crate) fn accumulate_output(&self, raw_bytes: &[u8]) {
        let mut buf = self.raw_output.borrow_mut();
        append_bounded_output(&mut buf, raw_bytes, super::MAX_RAW_OUTPUT_BYTES);
    }

    pub(crate) fn output_text(&self) -> String {
        let mut raw = self.raw_output.borrow_mut();
        if raw.is_empty() {
            return String::new();
        }
        String::from_utf8_lossy(raw.make_contiguous()).into_owned()
    }

    /// Clear the accumulated output buffer (without touching the VTE).
    pub(crate) fn reset_output_buffer(&self) {
        self.raw_output.borrow_mut().clear();
    }

    /// The column count the live VTE is wrapping at — the single source of truth
    /// for pre-wrapping finished blocks so they align with what the user watched.
    pub(crate) fn grid_cols(&self) -> usize {
        (self.active_vte.column_count().max(20)) as usize
    }

    /// Reset the live VTE for the next prompt (anvil block.rs:1028-1044). `reset`
    /// acts immediately, but already-queued feed() bytes are processed async, so the
    /// in-stream clear (fed after them) wipes stale output in the correct order.
    ///
    /// `preserve_scrollback`: when true, keep the VTE's buffer + scrollback intact
    /// (only the accumulated raw_output snapshot for the *next* block is cleared,
    /// and SGR state is soft-reset). This mirrors a traditional VTE where PageUp
    /// at a prompt reveals the previous command's output tail. The default (false)
    /// wipes the live VTE on every PromptStart, since the finished blocks above
    /// already hold the authoritative scrollback.
    pub(crate) fn reset_active(&self, preserve_scrollback: bool) {
        if preserve_scrollback {
            self.active_vte.feed(b"\x1b[0m");
        } else {
            self.active_vte.reset(true, true);
            self.active_vte.feed(b"\x1b[H\x1b[2J\x1b[3J");
        }
        self.raw_output.borrow_mut().clear();
    }

    pub(crate) fn widget(&self) -> &gtk::Box {
        &self.widget
    }

    pub(crate) fn grab_focus(&self) {
        self.active_vte.grab_focus();
    }
}

// ─── TermView state machine ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum BlockState {
    /// Waiting for first PromptStart or any bytes
    Idle,
    /// Between PromptStart and PromptEnd — collecting prompt text
    CollectingPrompt,
    /// Between PromptEnd and CommandStart — user is typing
    AwaitingCommand,
    /// Between CommandStart and CommandEnd — collecting output
    CollectingOutput,
    /// Inside full-screen app (vim/less/etc.)
    AltScreen,
    /// Between CommandEnd and next PromptStart — still collecting late output
    PostCommand,
    /// Shell has no OSC-133 integration: route all bytes to the raw VTE so output
    /// is never dropped. Entered from Idle when output arrives but no FTCS event
    /// has been seen within the startup grace window. Recovered to block mode if a
    /// PromptStart ever arrives (late-loading integration).
    RawFallback,
}
