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

/// Conservative per-pane estimated-memory budget for completed-block objects.
///
/// The configurable count limit is not a memory limit: a few ANSI-heavy cards
/// can otherwise retain large Strings and VTE cell grids. The newest completed
/// block is the sole exception when it cannot fit by itself.
pub(crate) const MAX_COMPLETED_BLOCK_RETAINED_BYTES: usize = 128 * 1024 * 1024;

// A finished card owns two VTEs plus its GTK widget/controller tree. These
// bases cover allocations which cannot be inferred from text lengths.
const RETAINED_BYTES_PER_VTE_BASE: usize = 128 * 1024;
const RETAINED_BYTES_PER_WIDGET_TREE_BASE: usize = 256 * 1024;
const FINISHED_BLOCK_FIXED_RETAINED_BYTES: usize =
    2 * RETAINED_BYTES_PER_VTE_BASE + RETAINED_BYTES_PER_WIDGET_TREE_BASE;

// Anvil normally owns raw ANSI output in `full_output`; a filter may add one
// displayed copy. The plain capture lives in BlockData and the lazy find cache
// may add another. VTE cells are substantially larger than their source bytes.
const RAW_OUTPUT_RETAINED_OWNERS: usize = 2;
const PLAIN_OUTPUT_RETAINED_OWNERS: usize = 2;
const VTE_RETAINED_BYTES_PER_MATERIALIZED_BYTE: usize = 32;
// GDK may retain decoded pixels and encoded/source backing simultaneously.
const IMAGE_RETAINED_OWNERS: usize = 2;
const FINISHED_OUTPUT_FILTER_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

/// Fade a card's quick-action strip without changing the header allocation.
///
/// An invisible strip must also be insensitive: `can_target(false)` only
/// removes pointer targeting, while its child buttons can still receive Tab
/// focus and activate an action from a card that is no longer active.
pub(crate) fn reveal_block_actions(action_box: &gtk::Box, revealed: bool) {
    action_box.set_opacity(if revealed { 1.0 } else { 0.0 });
    action_box.set_can_target(revealed);
    action_box.set_sensitive(revealed);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CompletedBlockRetentionPlan {
    /// Number of oldest entries to remove from every block-indexed collection.
    pub(crate) evict_prefix: usize,
    pub(crate) retained_count: usize,
    pub(crate) retained_estimated_bytes: usize,
    /// Evictions required in addition to the configured count limit.
    pub(crate) byte_budget_evictions: usize,
    /// The explicit newest-wins exception to the hard byte cap.
    pub(crate) newest_exceeds_byte_budget: bool,
}

/// Plan prefix eviction over oldest-to-newest `(block_id, estimated_bytes)`.
///
/// Scanning backwards makes newest-wins explicit and avoids summing entries
/// which the count cap will discard. Overflow is treated as over budget.
pub(crate) fn completed_block_retention_plan(
    blocks: &[(u64, usize)],
    max_blocks: usize,
    max_bytes: usize,
) -> CompletedBlockRetentionPlan {
    let Some(&(_, newest_bytes)) = blocks.last() else {
        return CompletedBlockRetentionPlan::default();
    };

    let count_limit = max_blocks.max(1);
    let count_limited_start = blocks.len().saturating_sub(count_limit);
    let mut retained_start = blocks.len() - 1;
    let mut retained_count = 1;
    let mut retained_estimated_bytes = newest_bytes;

    for index in (count_limited_start..retained_start).rev() {
        let Some(next_bytes) = retained_estimated_bytes.checked_add(blocks[index].1) else {
            break;
        };
        if next_bytes > max_bytes {
            break;
        }
        retained_start = index;
        retained_count += 1;
        retained_estimated_bytes = next_bytes;
    }

    CompletedBlockRetentionPlan {
        evict_prefix: retained_start,
        retained_count,
        retained_estimated_bytes,
        byte_budget_evictions: retained_start.saturating_sub(count_limited_start),
        newest_exceeds_byte_budget: newest_bytes > max_bytes,
    }
}

#[allow(clippy::too_many_arguments)]
fn estimated_completed_block_retained_bytes(
    prompt_bytes: usize,
    command_bytes: usize,
    command_markup_bytes: usize,
    rendered_command_bytes: usize,
    raw_output_bytes: usize,
    materialized_output_bytes: usize,
    plain_output_bytes: usize,
    cwd_bytes: usize,
    image_pixel_bytes: usize,
) -> usize {
    FINISHED_BLOCK_FIXED_RETAINED_BYTES
        .saturating_add(raw_output_bytes.saturating_mul(RAW_OUTPUT_RETAINED_OWNERS))
        .saturating_add(plain_output_bytes.saturating_mul(PLAIN_OUTPUT_RETAINED_OWNERS))
        .saturating_add(
            materialized_output_bytes
                .min(MAX_FINISHED_VTE_GRID_CELLS)
                .saturating_mul(VTE_RETAINED_BYTES_PER_MATERIALIZED_BYTE),
        )
        // BlockData.cmd plus FinishedBlock.cmd_text.
        .saturating_add(command_bytes.saturating_mul(2))
        .saturating_add(rendered_command_bytes)
        .saturating_add(
            rendered_command_bytes
                .min(MAX_FINISHED_VTE_GRID_CELLS)
                .saturating_mul(VTE_RETAINED_BYTES_PER_MATERIALIZED_BYTE),
        )
        // BlockData.prompt plus FinishedBlock.prompt_text.
        .saturating_add(prompt_bytes.saturating_mul(2))
        .saturating_add(command_markup_bytes)
        // BlockData.cwd plus its rendered chip.
        .saturating_add(cwd_bytes.saturating_mul(2))
        .saturating_add(image_pixel_bytes.saturating_mul(IMAGE_RETAINED_OWNERS))
        .saturating_add(std::mem::size_of::<BlockData>())
        .saturating_add(std::mem::size_of::<FinishedBlock>())
}

/// Upper-bound terminal grid units from a UTF-8/control stream without parsing
/// it twice. Tabs receive a full-row allowance because tab stops are mutable.
fn terminal_grid_units_upper_bound(bytes: &[u8], cols: usize) -> usize {
    let tab_extra = cols.max(1).saturating_sub(1);
    bytes.iter().fold(bytes.len(), |units, byte| {
        if *byte == b'\t' {
            units.saturating_add(tab_extra)
        } else {
            units
        }
    })
}

fn rendered_command_bytes(cmd: &str, cmd_ansi: Option<&str>) -> Vec<u8> {
    match cmd_ansi {
        Some(ansi) if !ansi.is_empty() && !cmd.is_empty() => ansi.as_bytes().to_vec(),
        _ if cmd.is_empty() => b"(empty)".to_vec(),
        _ => highlight_command_to_ansi(cmd).into_bytes(),
    }
}

/// Conservative estimate available before a GTK/VTE widget tree exists, so
/// live finalization can evict old cards before constructing a large new one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn estimated_live_finished_block_retained_bytes(
    prompt: &str,
    cmd: &str,
    cmd_ansi: Option<&str>,
    raw_output: &str,
    plain_output_bytes: usize,
    cwd: Option<&str>,
    cols: i64,
    images: &[gtk::gdk::Texture],
) -> usize {
    let cols = cols.clamp(1, MAX_FINISHED_VTE_COLUMNS) as usize;
    let command = rendered_command_bytes(cmd, cmd_ansi);
    let image_pixel_bytes = images.iter().fold(0usize, |total, texture| {
        total.saturating_add(
            (texture.width().max(0) as usize)
                .saturating_mul(texture.height().max(0) as usize)
                .saturating_mul(4),
        )
    });
    estimated_completed_block_retained_bytes(
        prompt.len(),
        cmd.len(),
        cmd_ansi.map_or(0, str::len),
        command
            .len()
            .max(terminal_grid_units_upper_bound(&command, cols)),
        raw_output.len(),
        plain_output_bytes.max(terminal_grid_units_upper_bound(raw_output.as_bytes(), cols)),
        plain_output_bytes,
        cwd.map_or(0, str::len),
        image_pixel_bytes,
    )
    .saturating_add(if images.is_empty() {
        0
    } else {
        // The pending admission ledger includes encoded backing and object
        // overhead which cannot be reconstructed from Texture dimensions.
        crate::terminal::kitty_graphics::MAX_PENDING_BYTES_PER_BLOCK
    })
}

// ─── FinishedBlock ────────────────────────────────────────────────────────────

pub(crate) const BLOCK_LIFECYCLE_SCHEMA: u32 = 0x4a54_4c31;

fn block_lifecycle_schema() -> u32 {
    BLOCK_LIFECYCLE_SCHEMA
}

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
    #[serde(skip, default = "block_lifecycle_schema")]
    pub(crate) lifecycle_schema: u32,
    #[serde(default)]
    pub(crate) completion_provenance: CompletionProvenanceWire,
    #[serde(default)]
    pub(crate) start_mark_seen: bool,
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
    /// The command line came from the shell's own OSC 133 report, not a
    /// screen scrape. Defaults to false so blocks saved before this field
    /// existed can never pose as exact evidence for agent tasks.
    #[serde(default)]
    pub(crate) command_exact: bool,
    /// The shell admitted its command report was truncated; the visible text
    /// must not be treated as the command that actually ran.
    #[serde(default)]
    pub(crate) command_truncated: bool,
}

/// Fence long enough to contain `text`: untrusted output may itself hold a run
/// of backticks, and a shorter fence would end the block there. Shared with the
/// metadata record export so both record kinds fence the same way.
pub(super) fn markdown_fence(text: &str) -> String {
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

    pub(crate) fn lifecycle_health(&self) -> BlockLifecycleHealth {
        assess_lifecycle(self.start_mark_seen, self.completion_provenance.into())
    }

    pub(crate) fn timing_is_authoritative(&self) -> bool {
        let completion_provenance = CompletionProvenance::from(self.completion_provenance);
        self.is_background()
            || completion_provenance == CompletionProvenance::JournalRecovered
            || (completion_provenance == CompletionProvenance::ShellReported
                && self.start_mark_seen)
    }

    pub(crate) fn lifecycle_notice(&self) -> Option<String> {
        if self.is_background() {
            return None;
        }
        match self.lifecycle_health() {
            BlockLifecycleHealth::Healthy => None,
            BlockLifecycleHealth::Recovered => Some(
                "Recovered command record — terminal rows were reconstructed from session history"
                    .to_string(),
            ),
            BlockLifecycleHealth::Degraded => Some(match self.completion_provenance.into() {
                CompletionProvenance::BoundaryInferred =>
                    "Command completion inferred from a trusted prompt boundary; exit status and timing are unavailable".to_string(),
                CompletionProvenance::ShellReported =>
                    "The shell reported an end marker without a matching command-start marker".to_string(),
                _ => "Command lifecycle provenance is degraded".to_string(),
            }),
            BlockLifecycleHealth::Incomplete => Some(
                "Command lifecycle is incomplete; no trusted completion source was retained"
                    .to_string(),
            ),
        }
    }

    /// Conservative cost of rebuilding this text-only persisted record as a
    /// finished card. A legacy zero column count uses the defensive VTE cap.
    pub(crate) fn estimated_restored_retained_bytes(&self) -> usize {
        estimated_live_finished_block_retained_bytes(
            &self.prompt,
            &self.cmd,
            self.cmd_markup.as_deref(),
            &self.output,
            self.output.len(),
            self.cwd.as_deref(),
            if self.cols == 0 {
                MAX_FINISHED_VTE_COLUMNS
            } else {
                i64::from(self.cols)
            },
            &[],
        )
    }

    /// Export block to JSON format
    pub fn to_json(&self) -> String {
        let Ok(mut value) = serde_json::to_value(self) else {
            return "{}".to_string();
        };
        if let Some(object) = value.as_object_mut() {
            if self.is_background() {
                object.remove("completion_provenance");
                object.remove("start_mark_seen");
            } else {
                object.insert(
                    "lifecycle_health".to_string(),
                    serde_json::Value::String(self.lifecycle_health().schema_name().to_string()),
                );
            }
            if !self.timing_is_authoritative() {
                for key in ["start_time_ms", "end_time_ms", "duration_ms"] {
                    object.insert(key.to_string(), serde_json::Value::Null);
                }
            }
        }
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
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
            md.push_str(&format!(
                "**Lifecycle:** {} ({})\n\n",
                self.lifecycle_health().schema_name(),
                self.completion_provenance.as_str(),
            ));
        }

        if let Some(dur) = self
            .timing_is_authoritative()
            .then_some(self.duration_ms)
            .flatten()
        {
            let dur_sec = dur as f64 / 1000.0;
            md.push_str(&format!("**Duration:** {:.3}s\n\n", dur_sec));
        }

        // Where it ran is part of what it means. A pasted block without it is
        // not reproducible, and half the commands worth exporting are
        // directory-relative.
        if let Some(cwd) = self.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
            md.push_str(&format!("**Directory:** {cwd}\n\n"));
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

/// Translate the shared completed-block contract into anvil's renderer-owned
/// status. `resolved_command` is the final block text after OSC metadata and
/// screen-capture fallback; raw [`crate::parser::CommandMeta::command`] must not
/// be passed here on its own.
pub(crate) fn block_status(
    resolved_command: Option<&str>,
    reported_exit_code: Option<i32>,
) -> BlockStatus {
    use jterm_core::block_contract::{classify_completed, CompletedBlockOutcome};

    match classify_completed(resolved_command, reported_exit_code) {
        CompletedBlockOutcome::Background => BlockStatus::Background,
        CompletedBlockOutcome::Success => BlockStatus::Succeeded,
        CompletedBlockOutcome::Failed(code) => BlockStatus::Failed(code),
        CompletedBlockOutcome::Unknown => BlockStatus::Unreported,
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

    /// Header status icon, the CSS class that colours it, and its accessible
    /// name. Symbolic theme icons keep the status portable across font setups.
    fn icon(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Background => (
                "view-refresh-symbolic",
                "block-status-background",
                "Background output",
            ),
            Self::Succeeded => ("emblem-ok-symbolic", "block-status-ok", "Command succeeded"),
            Self::Failed(_) => (
                "dialog-error-symbolic",
                "block-status-bad",
                "Command failed",
            ),
            Self::Unreported => (
                "dialog-question-symbolic",
                "block-status-unknown",
                "Command exit status not reported",
            ),
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
    pub bookmarked_only: bool,
    pub background_only: bool,
    pub slow_threshold_ms: u64,
    pub use_regex: bool,
}

pub(crate) struct FinishedBlock {
    pub(crate) id: u64,
    /// Commandless output emitted while the shell prompt was idle.
    pub(crate) is_background: bool,
    pub(crate) widget: gtk::Box,
    /// Everything the card draws, wrapped one level below `widget` so
    /// virtualization can hide the contents while `widget` stays in the
    /// document at a pinned placeholder height. See
    /// [`FinishedBlock::set_virtualized`].
    content: gtk::Box,
    /// Last real allocation of this card, kept as the placeholder height.
    virtualized_height: Rc<Cell<i32>>,
    virtualized: Rc<Cell<bool>>,
    /// Density which [`Self::virtualized_height`] currently describes. Keeping
    /// the two together lets a live switch translate a parked placeholder
    /// without remeasuring an allocation which still has the old margins.
    compact: Rc<Cell<bool>>,
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
    /// Visible keyboard contract for the active edge of a Block selection.
    /// It sits before the expanding spacer so showing it consumes slack rather
    /// than shifting timestamp/status metadata.
    pub(crate) selection_hint: gtk::Label,
    /// Persistent selection legend, independent of transient refusal text.
    pub(crate) selection_hint_steady: Rc<RefCell<String>>,
    /// Only the newest refusal timeout may restore the steady legend.
    pub(crate) selection_feedback_generation: Rc<Cell<u64>>,
    /// Fold or unfold this card's output, and whether it is folded now. Same
    /// exposure as `toggle_filter`, for the menu item and the keyboard action.
    pub(crate) toggle_collapsed: Rc<dyn Fn()>,
    collapsed_state: Rc<Cell<bool>>,
    /// Toggle the per-block output filter without discarding its query. Exposed
    /// so the Warp-compatible keyboard action can target the selected/latest block.
    pub(crate) toggle_filter: Rc<dyn Fn()>,
    /// Explicit Warp-style navigation affordance for oversized output.
    pub(crate) jump_bottom_btn: gtk::Button,
    pub(crate) bookmark_star: gtk::Image,
    pub(crate) status_icon: gtk::Image,
    /// Header chip naming an untrusted completion; hidden on a healthy record.
    lifecycle_chip: gtk::Label,
    /// Column count the output VTE is sized to — needed for re-feed (filter).
    pub(crate) cols: i64,
    /// Number of rows allocated to this finished output. Kept with the widget
    /// so filter re-renders use the same full-height canvas allocation.
    pub(crate) viewport_cap: i64,
    /// Current non-expanded row target, recomputed from the pane height minus
    /// the live input block height.
    dynamic_viewport_rows: Rc<Cell<i64>>,
    /// Render geometry plus filtered-text generation. A remap at the same
    /// geometry must not re-feed and transiently reset the card height.
    render_stamp: Rc<Cell<RenderStamp>>,
    /// Cost cache for deriving wrapped rows from the displayed transcript.
    /// This is deliberately separate from `render_stamp`: equal row counts do
    /// not prove that VTE already contains the right bytes or geometry.
    visual_rows_cache: Rc<Cell<Option<OutputVisualRowsCacheEntry>>>,
    displayed_generation: Rc<Cell<u64>>,
    command_bytes: Rc<Vec<u8>>,
    command_render_cols: Rc<Cell<i64>>,
    command_base_rows: i64,
    capture_rows: i64,
    max_expanded_cap: i64,
    output_rows: i64,
    expanded: Rc<Cell<bool>>,
    expand_btn: gtk::Button,
    /// True only when this block has more output rows than can be shown at once.
    pub(crate) output_scrollable: bool,
    /// Whether this block is tall enough to expose long-block navigation.
    pub(crate) long_output: bool,
    estimated_retained_bytes: usize,
}

impl Clone for FinishedBlock {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            is_background: self.is_background,
            widget: self.widget.clone(),
            content: self.content.clone(),
            virtualized_height: self.virtualized_height.clone(),
            virtualized: self.virtualized.clone(),
            compact: self.compact.clone(),
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
            selection_hint: self.selection_hint.clone(),
            selection_hint_steady: self.selection_hint_steady.clone(),
            selection_feedback_generation: self.selection_feedback_generation.clone(),
            toggle_collapsed: self.toggle_collapsed.clone(),
            collapsed_state: self.collapsed_state.clone(),
            toggle_filter: self.toggle_filter.clone(),
            jump_bottom_btn: self.jump_bottom_btn.clone(),
            bookmark_star: self.bookmark_star.clone(),
            status_icon: self.status_icon.clone(),
            lifecycle_chip: self.lifecycle_chip.clone(),
            cols: self.cols,
            viewport_cap: self.viewport_cap,
            dynamic_viewport_rows: self.dynamic_viewport_rows.clone(),
            render_stamp: self.render_stamp.clone(),
            visual_rows_cache: self.visual_rows_cache.clone(),
            displayed_generation: self.displayed_generation.clone(),
            command_bytes: self.command_bytes.clone(),
            command_render_cols: self.command_render_cols.clone(),
            command_base_rows: self.command_base_rows,
            capture_rows: self.capture_rows,
            max_expanded_cap: self.max_expanded_cap,
            output_rows: self.output_rows,
            expanded: self.expanded.clone(),
            expand_btn: self.expand_btn.clone(),
            output_scrollable: self.output_scrollable,
            long_output: self.long_output,
            estimated_retained_bytes: self.estimated_retained_bytes,
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
/// BlockFilterQuery). `None` means the caller should borrow `full` directly;
/// this covers an empty query, an invalid regex, and a result identical to the
/// original transcript without cloning it into a temporary display override.
fn filter_output_lines(
    full: &str,
    query: &str,
    use_regex: bool,
    case_sensitive: bool,
    invert: bool,
    context: usize,
) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    let re = if use_regex {
        match regex::RegexBuilder::new(query)
            .case_insensitive(!case_sensitive)
            .build()
        {
            Ok(re) => Some(re),
            Err(_) => return None,
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
            let hi = i.saturating_add(context).saturating_add(1).min(lines.len());
            for slot in keep.iter_mut().take(hi).skip(lo) {
                *slot = true;
            }
        }
    }

    // `str::lines().join("\n")` (the legacy result) normalizes CRLF and
    // removes a final LF. Only borrow `full` when joining every retained line
    // would be byte-identical; otherwise preserve those exact old semantics.
    if keep.iter().all(|kept| *kept) && !full.ends_with('\n') && !full.contains("\r\n") {
        return None;
    }

    let (kept_lines, kept_bytes) = lines
        .iter()
        .zip(&keep)
        .filter(|(_, kept)| **kept)
        .fold((0usize, 0usize), |(count, bytes), (line, _)| {
            (count.saturating_add(1), bytes.saturating_add(line.len()))
        });
    let mut filtered =
        String::with_capacity(kept_bytes.saturating_add(kept_lines.saturating_sub(1)));
    let mut wrote_line = false;
    for (line, kept) in lines.iter().zip(&keep) {
        if !kept {
            continue;
        }
        if wrote_line {
            filtered.push('\n');
        }
        filtered.push_str(line);
        wrote_line = true;
    }
    Some(filtered)
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

#[cfg(test)]
thread_local! {
    static OUTPUT_VISUAL_ROW_COUNT_CALLS: Cell<usize> = const { Cell::new(0) };
    static OUTPUT_VISUAL_ROWS_CACHE_HITS: Cell<usize> = const { Cell::new(0) };
    static OUTPUT_VISUAL_ROWS_CACHE_MISSES: Cell<usize> = const { Cell::new(0) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OutputVisualRowsCacheEntry {
    effective_cols: i64,
    displayed_generation: u64,
    rows: i64,
}

/// Rows occupied after VTE wraps the snapshot at `cols`. Finished cards need
/// this rather than the logical line count, otherwise a stack trace containing
/// very long type names is still pushed into the VTE's private scrollback.
pub(crate) fn output_visual_row_count(text: &str, cols: i64) -> i64 {
    #[cfg(test)]
    OUTPUT_VISUAL_ROW_COUNT_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
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
    // `strip_ansi` must replay cursor motion and erases, but for ordinary text
    // its result is byte-for-byte identical. Borrow the common case directly
    // so a cache miss scans once without first allocating an equally large
    // String. ESC, CR, and BS are exactly the slow-path identity boundary used
    // by `strip_ansi_with_clear_detect`.
    let rendered;
    let text = if memchr::memchr3(0x1b, b'\r', b'\x08', text.as_bytes()).is_none() {
        text
    } else {
        rendered = strip_ansi(text);
        rendered.as_str()
    };
    let text = output_display_text(text);
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

fn cached_output_visual_row_count(
    cache: &Cell<Option<OutputVisualRowsCacheEntry>>,
    text: &str,
    effective_cols: i64,
    displayed_generation: u64,
) -> i64 {
    if let Some(entry) = cache.get().filter(|entry| {
        entry.effective_cols == effective_cols && entry.displayed_generation == displayed_generation
    }) {
        #[cfg(test)]
        OUTPUT_VISUAL_ROWS_CACHE_HITS.with(|hits| hits.set(hits.get().saturating_add(1)));
        return entry.rows;
    }

    #[cfg(test)]
    OUTPUT_VISUAL_ROWS_CACHE_MISSES.with(|misses| misses.set(misses.get().saturating_add(1)));
    let rows = output_visual_row_count(text, effective_cols);
    cache.set(Some(OutputVisualRowsCacheEntry {
        effective_cols,
        displayed_generation,
        rows,
    }));
    rows
}

/// Start a new displayed-text generation and invalidate its derived row count.
/// Clear before wrapping so `u64::MAX -> 0` can never alias a stale generation
/// zero entry, even if a future caller advances without immediately measuring.
fn advance_displayed_generation(
    generation: &Cell<u64>,
    visual_rows_cache: &Cell<Option<OutputVisualRowsCacheEntry>>,
) -> u64 {
    visual_rows_cache.set(None);
    let next = generation.get().wrapping_add(1);
    generation.set(next);
    next
}

/// Finished VTEs follow their pixel allocation once a split becomes narrower
/// than the columns recorded with the block. Row and height math must use that
/// same width or map/refit passes alternate between two geometries.
fn effective_render_cols(vte: &vte4::Terminal, recorded_cols: i64) -> i64 {
    clamp_render_cols(recorded_cols, vte.width() as i64, vte.char_width())
}

fn clamp_render_cols(recorded_cols: i64, width_px: i64, cell_width_px: i64) -> i64 {
    let recorded = recorded_cols.clamp(1, MAX_FINISHED_VTE_COLUMNS);
    if width_px <= 0 || cell_width_px <= 0 {
        return recorded;
    }
    recorded.min((width_px / cell_width_px).max(2))
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

/// Rows a finished block spends on chrome that is not its output: the command
/// row plus the card's own padding. Reserved alongside the live input cell so a
/// capped block still leaves the prompt visible beneath it.
const FINISHED_BLOCK_NON_OUTPUT_ROWS: i64 = 3;

/// Rows of output a finished block may show, derived from the history
/// viewport's own height.
///
/// The reserve is the live input cell's *minimum* height, never its current
/// one. A cap that followed the live cell would change twice per command — the
/// cell grows to the full viewport while a command runs and collapses back at
/// the next prompt — and every change re-feeds every finished block's VTE, so
/// the whole history visibly collapsed to three rows and re-expanded on each
/// Enter. Keeping the reserve constant makes the cap a pure function of pane
/// geometry, so a command run re-fits nothing.
fn fitted_output_rows_for_viewport(
    viewport_rows: Option<i64>,
    fallback_rows: i64,
    output_rows: i64,
) -> i64 {
    let output_rows = output_rows.max(1);
    let reserve = super::MIN_INPUT_ROWS as i64 + FINISHED_BLOCK_NON_OUTPUT_ROWS;
    viewport_rows
        .map(|rows| rows.saturating_sub(reserve))
        .unwrap_or(fallback_rows)
        .max(3)
        .min(output_rows)
}

/// [`fitted_output_rows_for_viewport`] against the pane this block actually
/// hangs in. Falls back to the caller's rows while the block has no
/// `ScrolledWindow` ancestor yet (construction, before the card is inserted).
fn fitted_output_rows_for_widget(
    vte: &vte4::Terminal,
    fallback_rows: i64,
    output_rows: i64,
) -> i64 {
    let viewport_rows = vte
        .ancestor(gtk::ScrolledWindow::static_type())
        .and_then(|widget| widget.downcast::<gtk::ScrolledWindow>().ok())
        .and_then(|scroll| super::viewport_rows_for(vte, &scroll));
    fitted_output_rows_for_viewport(viewport_rows, fallback_rows, output_rows)
}

/// Clamp every dynamic height cap to the same column × row budget used by
/// `set_size`. Otherwise a large GTK height request can grow a VTE back beyond
/// the bounded grid immediately after the renderer clamped it.
fn bounded_finished_viewport_rows(cols: i64, requested_rows: i64) -> i64 {
    bounded_finished_vte_geometry(cols, requested_rows.max(1), 0).1
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

pub(crate) fn forward_outer_scroll(outer: &gtk::ScrolledWindow, dy: f64) {
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
pub(crate) fn scroll_adjustment_by_wheel(adj: &gtk::Adjustment, dy: f64) -> bool {
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

    fn legacy_filter_output_lines(
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
            .filter_map(|(line, kept)| kept.then_some(*line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn reset_visual_row_cache_counters() {
        OUTPUT_VISUAL_ROWS_CACHE_HITS.with(|hits| hits.set(0));
        OUTPUT_VISUAL_ROWS_CACHE_MISSES.with(|misses| misses.set(0));
    }

    fn visual_row_cache_counters() -> (usize, usize) {
        let hits = OUTPUT_VISUAL_ROWS_CACHE_HITS.with(Cell::get);
        let misses = OUTPUT_VISUAL_ROWS_CACHE_MISSES.with(Cell::get);
        (hits, misses)
    }

    fn spin_main_context_until(condition: impl Fn() -> bool) {
        let context = glib::MainContext::default();
        let started = std::time::Instant::now();
        while !condition() && started.elapsed() < std::time::Duration::from_secs(2) {
            while context.iteration(false) {}
        }
        assert!(
            condition(),
            "GTK condition did not settle before the deadline"
        );
    }

    fn spin_main_context_for(duration: std::time::Duration) {
        let done = Rc::new(Cell::new(false));
        let done_for_timeout = done.clone();
        glib::timeout_add_local_once(duration, move || done_for_timeout.set(true));
        spin_main_context_until(|| done.get());
    }

    fn find_search_entry(widget: &gtk::Widget) -> Option<gtk::SearchEntry> {
        if let Ok(entry) = widget.clone().downcast::<gtk::SearchEntry>() {
            return Some(entry);
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            if let Some(entry) = find_search_entry(&current) {
                return Some(entry);
            }
            child = current.next_sibling();
        }
        None
    }

    #[test]
    #[ignore = "requires DISPLAY; run explicitly under Xvfb"]
    fn unmapped_refit_skips_output_row_scan() {
        gtk::init().expect("gtk init");
        let block = FinishedBlock::new(
            1,
            "$ ",
            "cat huge.log",
            None,
            &"wide 界 output\n".repeat(100_000),
            Some(0),
            &Config::safe_defaults(),
            None,
            None,
            None,
            80,
        );
        assert!(!block.output_vte.is_mapped());
        OUTPUT_VISUAL_ROW_COUNT_CALLS.with(|calls| calls.set(0));

        assert_eq!(block.refit_output_to_viewport(), None);
        OUTPUT_VISUAL_ROW_COUNT_CALLS.with(|calls| {
            assert_eq!(
                calls.get(),
                0,
                "an off-screen refit must return before scanning its transcript"
            )
        });

        // The skipped cap is not lost: the card's map handler performs the
        // deferred measurement against its actual ScrolledWindow ancestor.
        let scrolled = gtk::ScrolledWindow::builder()
            .min_content_width(640)
            .min_content_height(320)
            .child(block.widget())
            .build();
        let window = gtk::Window::builder().child(&scrolled).build();
        reset_visual_row_cache_counters();
        window.present();
        spin_main_context_until(|| block.output_vte.is_mapped());
        let (hits, misses) = visual_row_cache_counters();
        assert_eq!(hits + misses, 1, "map resolves output rows exactly once");
        assert!(block.dynamic_viewport_rows.get() > 0);
        window.close();
        while glib::MainContext::default().iteration(false) {}
    }

    #[test]
    #[ignore = "requires DISPLAY; run explicitly under Xvfb"]
    fn only_an_untrusted_record_wears_a_lifecycle_chip() {
        gtk::init().expect("gtk init");
        let config = Config::safe_defaults();
        let block = FinishedBlock::new(
            6,
            "$ ",
            "make",
            None,
            "built\n",
            Some(0),
            &config,
            None,
            None,
            None,
            80,
        );

        block.set_lifecycle(BlockLifecycleHealth::Healthy, None);
        assert!(!block.lifecycle_chip.is_visible());

        block.set_lifecycle(BlockLifecycleHealth::Degraded, Some("no end marker"));
        assert!(block.lifecycle_chip.is_visible());
        assert_eq!(block.lifecycle_chip.text(), "inferred");
        assert_eq!(
            block.lifecycle_chip.tooltip_text().as_deref(),
            Some("no end marker"),
            "the explanation belongs on the chip, where no other tooltip shadows it"
        );

        // Card shells are pooled, so the healthy record that reuses this one
        // has to be able to take the mark back off.
        block.set_lifecycle(BlockLifecycleHealth::Healthy, None);
        assert!(!block.lifecycle_chip.is_visible());
        assert_eq!(block.lifecycle_chip.tooltip_text(), None);

        // Background output never ran a command, so it has no completion to
        // doubt — and its header already says what it is.
        let background = FinishedBlock::new(
            7, "$ ", "", None, "async\n", None, &config, None, None, None, 80,
        );
        background.set_lifecycle(BlockLifecycleHealth::Degraded, Some("no end marker"));
        assert!(!background.lifecycle_chip.is_visible());
    }

    #[test]
    #[ignore = "requires DISPLAY; run explicitly under Xvfb"]
    fn the_selection_hint_sits_on_the_spacers_left() {
        use gtk::prelude::*;

        gtk::init().expect("gtk init");
        let card = FinishedBlock::new(
            1,
            "$ ",
            "cargo test",
            None,
            "ok\r\n",
            Some(0),
            &Config::safe_defaults(),
            Some(5),
            Some(1_700_000_000_000),
            None,
            80,
        );

        let hint_widget: gtk::Widget = card.selection_hint.clone().upcast();
        let mut hint_index = None;
        let mut spacer_index = None;
        let mut index = 0;
        let mut child = card.header_row.first_child();
        while let Some(widget) = child {
            if widget == hint_widget {
                hint_index = Some(index);
            } else if spacer_index.is_none() && widget.hexpands() {
                spacer_index = Some(index);
            }
            child = widget.next_sibling();
            index += 1;
        }

        let hint_index = hint_index.expect("header carries the selection hint");
        let spacer_index = spacer_index.expect("header carries an expanding spacer");
        assert!(
            hint_index < spacer_index,
            "hint at {hint_index} must precede spacer at {spacer_index}"
        );
        assert_eq!(
            card.selection_hint.max_width_chars(),
            super::super::SELECTION_HINT_MAX_CHARS,
            "the natural-width cap must not permanently hide the final action"
        );
        assert_eq!(
            card.selection_hint.width_chars(),
            super::super::SELECTION_HINT_MIN_CHARS,
            "a narrow header must reserve the complete Escape affordance"
        );

        assert!(
            !card.action_box.is_sensitive(),
            "a faded action strip must not participate in keyboard focus"
        );
        reveal_block_actions(&card.action_box, true);
        assert!(card.action_box.is_sensitive());
        assert!(card.action_box.can_target());
        reveal_block_actions(&card.action_box, false);
        assert!(!card.action_box.is_sensitive());
        assert!(!card.action_box.can_target());

        card.selection_hint
            .set_text(super::super::SELECTION_HINT_RUN);
        card.selection_hint.set_visible(true);
        let refusal = "Esc cancel  ·  Prompt has input  ·  nothing recalled".to_string();
        super::super::flash_finished_selection_refusal(
            std::slice::from_ref(&card),
            Some(1),
            refusal.clone(),
        );
        assert_eq!(card.selection_hint.text().as_str(), refusal);
        assert_eq!(
            card.selection_hint.tooltip_text().as_deref(),
            Some(refusal.as_str())
        );
        assert_eq!(
            card.selection_hint.accessible_role(),
            gtk::AccessibleRole::Status
        );

        let loop_ = glib::MainLoop::new(None, false);
        let quit = loop_.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(1_700), move || {
            quit.quit();
        });
        loop_.run();
        assert_eq!(
            card.selection_hint.text().as_str(),
            super::super::SELECTION_HINT_RUN
        );
        assert_eq!(card.selection_hint.tooltip_text(), None);
    }

    #[test]
    #[ignore = "requires DISPLAY; run explicitly under Xvfb"]
    fn search_reads_the_filtered_view_and_falls_back_to_the_superset() {
        gtk::init().expect("gtk init");
        let transcript = "keep one\ndrop two\nkeep three\n";
        let block = FinishedBlock::new(
            5,
            "$ ",
            "cargo test",
            None,
            transcript,
            Some(0),
            &Config::safe_defaults(),
            None,
            None,
            None,
            80,
        );
        assert_eq!(
            block.with_searchable_output(str::to_string),
            transcript,
            "an unfiltered card searches its whole transcript"
        );

        // What the filter does to a card: swap in the visible subset. A hit
        // counted in `drop two` cannot be stepped to in the VTE, and the find
        // pass treats that failure as "no matches" for the whole session.
        let filtered = "keep one\nkeep three\n";
        *block.displayed_output.borrow_mut() = Some(filtered.to_string());
        assert_eq!(block.with_searchable_output(str::to_string), filtered);

        // The filter's own re-render holds this cell mutably on the same main
        // loop. The fallback is this card's superset — never another card's
        // text, and never a panic.
        let rendering = block.displayed_output.borrow_mut();
        assert_eq!(block.with_searchable_output(str::to_string), transcript);
        drop(rendering);
    }

    #[test]
    #[ignore = "requires DISPLAY; run explicitly under Xvfb"]
    fn block_density_switches_on_widgets_that_already_exist() {
        gtk::init().expect("gtk init");
        let config = Config::safe_defaults();
        assert!(
            !config.block_compact,
            "the default density is the roomy one"
        );
        let block = FinishedBlock::new(
            4,
            "$ ",
            "echo density",
            None,
            "density\n",
            Some(0),
            &config,
            None,
            None,
            None,
            80,
        );

        // Card margins are GTK properties, not CSS, so the class alone proves
        // nothing: the setting used to reach only panes built after it changed,
        // and a switch that repainted the class while leaving the spacing would
        // look exactly as wrong.
        let roomy = (
            block.widget().margin_top(),
            block.widget().margin_start(),
            block.header_row.margin_start(),
            block.header_row.margin_top(),
        );
        assert!(!block.widget().has_css_class("block-compact"));

        // Virtualize before switching: the explicit height request and its
        // parallel BlockData model used to stay at roomy density forever.
        let roomy_placeholder = block.set_virtualized(true);
        let mut block_data = VecDeque::from([finished_block(Some(0))]);
        block_data[0].estimated_height = roomy_placeholder;
        apply_finished_card_density(std::slice::from_ref(&block), &mut block_data, true);
        let compact_placeholder = roomy_placeholder - 13;
        assert_eq!(block_data[0].estimated_height, compact_placeholder);
        assert_eq!(block.widget().height_request(), compact_placeholder);
        let compact = (
            block.widget().margin_top(),
            block.widget().margin_start(),
            block.header_row.margin_start(),
            block.header_row.margin_top(),
        );
        assert!(block.widget().has_css_class("block-compact"));
        assert!(
            compact.0 < roomy.0
                && compact.1 < roomy.1
                && compact.2 < roomy.2
                && compact.3 < roomy.3,
            "compact must tighten every margin: {compact:?} vs {roomy:?}"
        );

        apply_finished_card_density(std::slice::from_ref(&block), &mut block_data, false);
        assert_eq!(block_data[0].estimated_height, roomy_placeholder);
        assert_eq!(block.widget().height_request(), roomy_placeholder);
        assert!(!block.widget().has_css_class("block-compact"));
        assert_eq!(
            (
                block.widget().margin_top(),
                block.widget().margin_start(),
                block.header_row.margin_start(),
                block.header_row.margin_top(),
            ),
            roomy,
            "switching back must restore construction's own margins"
        );

        // A filter removes a card from the metadata document. Its private
        // placeholder adopts the new density for a later reveal, while the
        // zero-height sentinel remains absent from scrolling calculations.
        block_data[0].estimated_height = 0;
        apply_finished_card_density(std::slice::from_ref(&block), &mut block_data, true);
        assert_eq!(block_data[0].estimated_height, 0);
        assert_eq!(block.widget().height_request(), compact_placeholder);

        // The live input cell carries the density as a class only; its height
        // comes from `BLOCK_ACTIVE_COMPACT_VCHROME_PX` via the same class.
        let live = ActiveBlock::new(&config, Rc::new(RefCell::new(VecDeque::new())));
        assert!(!live.widget().has_css_class("block-compact"));
        live.set_compact(true);
        assert!(live.widget().has_css_class("block-compact"));
        live.set_compact(false);
        assert!(!live.widget().has_css_class("block-compact"));

        // Existing correction/review, suggestion and Agent notice trees are
        // not FinishedBlocks. Their stable assistant roles must still update
        // all imperative outer/header/body margins in place.
        for (role, body_class, compact_bottom) in [
            ("command-review-standalone", Some("command-review-body"), 7),
            ("command-suggestion", None, 7),
            ("block-agent", None, 6),
        ] {
            let assistant = gtk::Box::new(gtk::Orientation::Vertical, 0);
            assistant.add_css_class("block-finished");
            assistant.add_css_class("block-assistant");
            assistant.add_css_class(role);
            assistant.set_margin_top(4);
            assistant.set_margin_start(8);
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            header.add_css_class("block-header");
            header.set_margin_top(6);
            header.set_margin_start(12);
            assistant.append(&header);
            let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
            if let Some(body_class) = body_class {
                body.add_css_class(body_class);
            }
            body.set_margin_start(12);
            body.set_margin_bottom(11);
            assistant.append(&body);

            assert!(apply_inline_assistant_density(assistant.upcast_ref(), true));
            assert!(assistant.has_css_class("block-compact"));
            assert_eq!(assistant.margin_top(), 1);
            assert_eq!(header.margin_start(), 8);
            assert_eq!(header.margin_top(), 3);
            assert_eq!(body.margin_start(), 8);
            assert_eq!(body.margin_bottom(), compact_bottom);

            assert!(apply_inline_assistant_density(
                assistant.upcast_ref(),
                false
            ));
            assert!(!assistant.has_css_class("block-compact"));
            assert_eq!(assistant.margin_top(), 4);
            assert_eq!(header.margin_start(), 12);
            assert_eq!(header.margin_top(), 6);
            assert_eq!(body.margin_start(), 12);
            assert_eq!(body.margin_bottom(), compact_bottom + 4);
        }
    }

    #[test]
    fn finished_height_estimator_accounts_for_density_chrome() {
        let roomy = Config::safe_defaults();
        let mut compact = roomy.clone();
        compact.block_compact = true;
        assert_eq!(
            estimated_finished_block_height(&compact, 17),
            estimated_finished_block_height(&roomy, 17) - 13
        );
    }

    #[test]
    #[ignore = "requires DISPLAY; run explicitly under Xvfb"]
    fn output_scrollbar_visibility_cannot_change_the_terminal_width() {
        gtk::init().expect("gtk init");
        let block = FinishedBlock::new(
            3,
            "$ ",
            "cat transcript.log",
            None,
            &"ordinary output line\n".repeat(400),
            Some(0),
            &Config::safe_defaults(),
            None,
            None,
            None,
            80,
        );
        let scrolled = gtk::ScrolledWindow::builder()
            .min_content_width(800)
            .min_content_height(400)
            .child(block.widget())
            .build();
        let window = gtk::Window::builder().child(&scrolled).build();
        window.present();
        spin_main_context_until(|| block.output_vte.is_mapped() && block.output_vte.width() > 0);

        // Whether this scrollbar is shown is decided from VTE's ring measured
        // against the visible page — a quantity the terminal's own width
        // produces. A scrollbar that took layout width would therefore move the
        // input of the decision that showed it: hide it, the terminal widens,
        // the ring rewraps to fewer rows, the ring stops overflowing, and the
        // next frame hides it again. That cycle closes inside GTK, so no
        // render-stamp guard can break it; the width edge has to not exist.
        block.output_scrollbar.set_visible(true);
        spin_main_context_for(std::time::Duration::from_millis(60));
        let with_scrollbar = block.output_vte.width();
        block.output_scrollbar.set_visible(false);
        spin_main_context_for(std::time::Duration::from_millis(60));
        let without_scrollbar = block.output_vte.width();

        assert!(
            with_scrollbar > 0,
            "the output terminal was never allocated"
        );
        assert_eq!(
            with_scrollbar, without_scrollbar,
            "the per-block scrollbar must not take width from its own terminal"
        );

        let overlay = block
            .output_scrollbar
            .parent()
            .and_then(|parent| parent.downcast::<gtk::Overlay>().ok())
            .expect("the scrollbar rides an overlay, not a box sibling");
        assert!(
            !overlay.is_measure_overlay(&block.output_scrollbar),
            "an overlay that is measured would put the width edge back"
        );

        window.close();
        while glib::MainContext::default().iteration(false) {}
    }

    #[test]
    #[ignore = "requires DISPLAY; run explicitly under Xvfb"]
    fn visual_row_cache_covers_filter_remap_expand_and_resize_refit() {
        gtk::init().expect("gtk init");
        let mut config = Config::safe_defaults();
        config.finished_block_viewport_rows = 12;
        config.finished_block_max_expanded_rows = 200;
        let output = (0..2_000)
            .map(|index| {
                if index % 2 == 0 {
                    format!("keep {index:04} wide 界 transcript payload")
                } else {
                    format!("drop {index:04} ordinary transcript payload")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let block = FinishedBlock::new(
            2,
            "$ ",
            "cat mixed.log",
            None,
            &output,
            Some(0),
            &config,
            None,
            None,
            None,
            100,
        );
        let scrolled = gtk::ScrolledWindow::builder().child(block.widget()).build();
        let window = gtk::Window::builder()
            .default_width(900)
            .default_height(420)
            .child(&scrolled)
            .build();
        window.present();
        spin_main_context_until(|| block.output_vte.is_mapped());

        (block.toggle_filter)();
        let root = block.widget().clone().upcast::<gtk::Widget>();
        let filter_entry = find_search_entry(&root).expect("finished block filter entry");
        let generation_before_query = block.displayed_generation.get();
        reset_visual_row_cache_counters();
        filter_entry.set_text("drop");
        filter_entry.set_text("keep");
        spin_main_context_until(|| block.displayed_generation.get() != generation_before_query);
        let filtered_generation = block.displayed_generation.get();
        spin_main_context_for(
            FINISHED_OUTPUT_FILTER_DEBOUNCE + std::time::Duration::from_millis(50),
        );
        assert_eq!(
            block.displayed_generation.get(),
            filtered_generation,
            "a cancelled query timeout must not publish after the quiet winner",
        );
        let displayed = block.displayed_output.borrow();
        let filtered_text = displayed.as_deref().expect("filter display override");
        assert!(filtered_text.lines().all(|line| line.contains("keep")));
        drop(displayed);
        let filtered_entry = block.visual_rows_cache.get().expect("filtered rows cached");
        assert_eq!(filtered_entry.displayed_generation, filtered_generation);
        assert!(visual_row_cache_counters().1 >= 1, "filter text must miss");

        reset_visual_row_cache_counters();
        block.set_virtualized(true);
        spin_main_context_until(|| !block.output_vte.is_mapped());
        block.set_virtualized(false);
        spin_main_context_until(|| block.output_vte.is_mapped());
        assert_eq!(
            visual_row_cache_counters(),
            (1, 0),
            "same-width filtered remap must reuse cached rows",
        );

        reset_visual_row_cache_counters();
        block.expand_btn.emit_clicked();
        while glib::MainContext::default().iteration(false) {}
        assert_eq!(
            visual_row_cache_counters(),
            (1, 0),
            "expand must reuse the current displayed rows",
        );

        let old_cols = effective_render_cols(&block.output_vte, block.cols);
        window.set_default_size(320, 420);
        window.queue_resize();
        spin_main_context_until(|| effective_render_cols(&block.output_vte, block.cols) < old_cols);
        let new_cols = effective_render_cols(&block.output_vte, block.cols);
        reset_visual_row_cache_counters();
        assert!(block.refit_output_to_viewport().is_some());
        assert_eq!(
            visual_row_cache_counters(),
            (0, 1),
            "a real column change must refresh wrapped rows",
        );
        assert_eq!(
            block
                .visual_rows_cache
                .get()
                .expect("resized rows cached")
                .effective_cols,
            new_cols,
        );

        reset_visual_row_cache_counters();
        let _ = block.refit_output_to_viewport();
        assert_eq!(
            visual_row_cache_counters(),
            (1, 0),
            "same-width refit must reuse cached rows",
        );

        window.close();
        while glib::MainContext::default().iteration(false) {}
    }

    fn finished_block(exit_code: Option<i32>) -> BlockData {
        BlockData {
            id: 1,
            prompt: "$ ".to_string(),
            cmd: "make".to_string(),
            cmd_markup: None,
            output: "built".to_string(),
            exit_code,
            lifecycle_schema: BLOCK_LIFECYCLE_SCHEMA,
            completion_provenance: CompletionProvenance::ShellReported.into(),
            start_mark_seen: true,
            estimated_height: 1,
            line_count: 1,
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: None,
            cwd: None,
            cols: 80,
            command_exact: true,
            command_truncated: false,
        }
    }

    #[test]
    fn retention_plan_accepts_an_exact_byte_limit() {
        let plan = completed_block_retention_plan(&[(11, 40), (12, 60)], 10, 100);
        assert_eq!(plan.evict_prefix, 0);
        assert_eq!(plan.retained_count, 2);
        assert_eq!(plan.retained_estimated_bytes, 100);
        assert_eq!(plan.byte_budget_evictions, 0);
        assert!(!plan.newest_exceeds_byte_budget);
    }

    #[test]
    fn retention_plan_evicts_oldest_at_one_byte_over_limit() {
        let plan = completed_block_retention_plan(&[(11, 41), (12, 60)], 10, 100);
        assert_eq!(plan.evict_prefix, 1);
        assert_eq!(plan.retained_count, 1);
        assert_eq!(plan.retained_estimated_bytes, 60);
        assert_eq!(plan.byte_budget_evictions, 1);
        assert!(!plan.newest_exceeds_byte_budget);
    }

    #[test]
    fn retention_plan_keeps_one_huge_newest_block() {
        let plan = completed_block_retention_plan(&[(99, 101)], 10, 100);
        assert_eq!(plan.evict_prefix, 0);
        assert_eq!(plan.retained_count, 1);
        assert_eq!(plan.retained_estimated_bytes, 101);
        assert!(plan.newest_exceeds_byte_budget);
    }

    #[test]
    fn retention_plan_enforces_count_separately_from_bytes() {
        let plan = completed_block_retention_plan(&[(1, 10), (2, 10), (3, 10)], 2, 100);
        assert_eq!(plan.evict_prefix, 1);
        assert_eq!(plan.retained_count, 2);
        assert_eq!(plan.retained_estimated_bytes, 20);
        assert_eq!(plan.byte_budget_evictions, 0);
    }

    #[test]
    fn retention_plan_treats_addition_overflow_as_over_budget() {
        let plan = completed_block_retention_plan(&[(1, usize::MAX), (2, 1)], 2, usize::MAX);
        assert_eq!(plan.evict_prefix, 1);
        assert_eq!(plan.retained_count, 1);
        assert_eq!(plan.retained_estimated_bytes, 1);
        assert_eq!(plan.byte_budget_evictions, 1);
    }

    #[test]
    fn dynamic_viewport_cap_cannot_outgrow_the_finished_vte_cell_budget() {
        assert_eq!(
            bounded_finished_viewport_rows(MAX_FINISHED_VTE_COLUMNS, 300),
            256,
        );
        assert_eq!(
            bounded_finished_viewport_rows(MAX_FINISHED_VTE_COLUMNS, 4_096),
            256,
        );
        assert_eq!(bounded_finished_viewport_rows(80, 0), 1);
    }

    #[test]
    fn virtualized_height_uses_the_same_bounded_rows_as_the_finished_vte() {
        let mut config = Config::safe_defaults();
        config.finished_block_viewport_rows = 5_000;
        let output = "x\n".repeat(300);
        assert_eq!(
            estimated_finished_block_height_for_text(&config, &output, MAX_FINISHED_VTE_COLUMNS,),
            estimated_finished_block_height(&config, 256),
        );
    }

    #[test]
    fn retained_estimate_charges_optional_display_and_plain_owners() {
        let small = estimated_completed_block_retained_bytes(2, 3, 0, 12, 5, 5, 5, 4, 0);
        let large = estimated_completed_block_retained_bytes(2, 3, 0, 12, 500, 500, 500, 4, 0);
        assert!(large > small);
        assert!(large - small >= 495 * (RAW_OUTPUT_RETAINED_OWNERS + PLAIN_OUTPUT_RETAINED_OWNERS));
    }

    #[test]
    fn an_unreported_status_never_becomes_a_zero() {
        assert_eq!(block_status(Some("make"), None), BlockStatus::Unreported);
        assert_eq!(block_status(Some("make"), Some(0)), BlockStatus::Succeeded);
        assert_eq!(
            block_status(Some("make"), Some(130)),
            BlockStatus::Failed(130)
        );
        assert_eq!(block_status(None, None), BlockStatus::Background);
        // Background output never was a command, so its absent status is not a
        // "the shell said nothing" case.
        assert_eq!(block_status(Some(" \t"), Some(7)), BlockStatus::Background);

        // A number nobody reported cannot be shown, so no badge is rendered.
        assert_eq!(block_status(Some("make"), None).exit_badge(), None);
        assert_eq!(
            block_status(Some("make"), Some(130))
                .exit_badge()
                .as_deref(),
            Some("exit:130")
        );
        assert_eq!(block_status(Some("make"), Some(0)).exit_badge(), None);
        // The one state that cannot explain itself gets a tooltip.
        assert!(block_status(Some("make"), None).icon_tooltip().is_some());
        assert!(block_status(Some("make"), Some(0)).icon_tooltip().is_none());
        // Every state uses a theme icon and carries a non-empty accessible
        // name, independently of whichever terminal font the user selected.
        for status in [
            BlockStatus::Background,
            BlockStatus::Succeeded,
            BlockStatus::Failed(1),
            BlockStatus::Unreported,
        ] {
            let (icon_name, _class, accessible_label) = status.icon();
            assert!(icon_name.ends_with("-symbolic"));
            assert!(!accessible_label.is_empty());
        }
        // And the unreported state is not drawn as either success or failure.
        for reported in [Some(0), Some(1)] {
            assert_ne!(
                block_status(Some("make"), None).stripe_class(),
                block_status(Some("make"), reported).stripe_class()
            );
            assert_ne!(
                block_status(Some("make"), None).icon(),
                block_status(Some("make"), reported).icon()
            );
        }
    }

    #[test]
    fn block_chrome_has_no_private_use_font_icons() {
        for source in [include_str!("blocks.rs"), include_str!("mod.rs")] {
            assert!(source
                .chars()
                .all(|ch| !(0xe000..=0xf8ff).contains(&(ch as u32))));
            for escaped in source.split("\\u{").skip(1) {
                let Some(hex) = escaped.split('}').next() else {
                    continue;
                };
                let Ok(codepoint) = u32::from_str_radix(hex, 16) else {
                    continue;
                };
                assert!(
                    !(0xe000..=0xf8ff).contains(&codepoint),
                    "BMP private-use escape remains in block chrome: {hex}"
                );
            }
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
    fn block_json_uses_shared_lifecycle_vocabulary_and_background_omits_it() {
        let mut inferred = finished_block(None);
        inferred.completion_provenance = super::CompletionProvenance::BoundaryInferred.into();
        inferred.start_mark_seen = true;
        let json: serde_json::Value = serde_json::from_str(&inferred.to_json()).unwrap();
        assert_eq!(json["completion_provenance"], "boundary_inferred");
        assert_eq!(json["lifecycle_health"], "degraded");

        inferred.cmd.clear();
        let json: serde_json::Value = serde_json::from_str(&inferred.to_json()).unwrap();
        assert!(json.get("completion_provenance").is_none());
        assert!(json.get("start_mark_seen").is_none());
        assert!(json.get("lifecycle_health").is_none());
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
        let mut dropped = false;
        for index in 0..CHUNKS {
            dropped |= append_bounded_output(&mut output, &vec![(index % 251) as u8; CHUNK], LIMIT);
        }
        assert_eq!(output.len(), LIMIT);
        assert!(
            dropped,
            "100 MiB through an 8 MiB bound lost bytes off the front"
        );
        let retained_chunks = LIMIT / CHUNK;
        let contiguous = output.make_contiguous();
        for offset in 0..retained_chunks {
            let expected = ((CHUNKS - retained_chunks + offset) % 251) as u8;
            assert!(contiguous[offset * CHUNK..(offset + 1) * CHUNK]
                .iter()
                .all(|byte| *byte == expected));
        }

        assert!(append_bounded_output(
            &mut output,
            &vec![0x5a; LIMIT + CHUNK],
            LIMIT
        ));
        assert_eq!(output.len(), LIMIT);
        assert!(output.iter().all(|byte| *byte == 0x5a));
    }

    /// The drop marker reports the stream, not the buffer: an append that fits
    /// entirely loses nothing, and an exactly-limit-sized append into an empty
    /// buffer is complete even though it replaces the whole retention window.
    #[test]
    fn bounded_output_reports_only_appends_that_lose_bytes() {
        let mut output = VecDeque::new();
        assert!(!append_bounded_output(&mut output, b"abc", 5));
        assert!(append_bounded_output(&mut output, b"def", 5));
        assert_eq!(output.make_contiguous(), b"bcdef");

        let mut exact = VecDeque::new();
        assert!(!append_bounded_output(&mut exact, b"12345", 5));
        assert!(append_bounded_output(&mut exact, b"67890", 5));

        let mut over = VecDeque::new();
        assert!(append_bounded_output(&mut over, b"123456", 5));

        let mut zero_limit = VecDeque::new();
        assert!(append_bounded_output(&mut zero_limit, b"x", 0));
        assert!(!append_bounded_output(&mut zero_limit, b"", 0));
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
    fn render_cols_follow_a_pane_narrower_than_the_recorded_width() {
        assert_eq!(clamp_render_cols(46, 31 * 10, 10), 31);
        assert_eq!(clamp_render_cols(46, 80 * 10, 10), 46);
        assert_eq!(clamp_render_cols(46, 46 * 10, 10), 46);
        assert_eq!(clamp_render_cols(46, 0, 10), 46);
        assert_eq!(clamp_render_cols(46, 310, 0), 46);
        assert_eq!(clamp_render_cols(46, 5, 10), 2);
    }

    #[test]
    fn narrow_pane_wraps_wide_glyph_rows_like_vte() {
        let line = "已最新已最新已最新已";
        assert_eq!(output_visual_row_count(line, 31), 1);
        assert_eq!(output_visual_row_count(line, 12), 2);
        assert_eq!(output_visual_row_count(line, 4), 5);
    }

    #[test]
    fn narrowed_render_cols_grow_the_height_row_count() {
        let line = "x".repeat(40);
        let recorded = 46;
        let clamped = clamp_render_cols(recorded, 31 * 10, 10);
        assert_eq!(output_visual_row_count(&line, recorded), 1);
        assert_eq!(output_visual_row_count(&line, clamped), 2);
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
    fn plain_visual_row_fast_path_matches_control_replay() {
        for text in [
            "plain output\nsecond line\n",
            "tabs\tand wide 界🙂 glyphs\n",
            "combining e\u{301} and nul \0 stay byte-identical\n",
        ] {
            assert!(memchr::memchr3(0x1b, b'\r', b'\x08', text.as_bytes()).is_none());
            let forced_replay = format!("\x1b[0m{text}");
            for cols in [4, 31, 80] {
                assert_eq!(
                    output_visual_row_count(text, cols),
                    output_visual_row_count(&forced_replay, cols),
                );
            }
        }
    }

    #[test]
    fn visual_row_cache_hits_same_key_and_refreshes_generation_or_columns() {
        let text = "123456789\n界界界";
        let cache = Cell::new(Some(OutputVisualRowsCacheEntry {
            effective_cols: 4,
            displayed_generation: 7,
            rows: 6,
        }));
        OUTPUT_VISUAL_ROW_COUNT_CALLS.with(|calls| calls.set(0));

        assert_eq!(cached_output_visual_row_count(&cache, text, 4, 7), 6);
        OUTPUT_VISUAL_ROW_COUNT_CALLS.with(|calls| assert_eq!(calls.get(), 0));

        let generation_rows = cached_output_visual_row_count(&cache, text, 4, 8);
        assert_eq!(generation_rows, output_visual_row_count(text, 4));
        OUTPUT_VISUAL_ROW_COUNT_CALLS.with(|calls| calls.set(0));
        assert_eq!(
            cached_output_visual_row_count(&cache, "ignored on a cache hit", 4, 8),
            generation_rows,
        );
        OUTPUT_VISUAL_ROW_COUNT_CALLS.with(|calls| assert_eq!(calls.get(), 0));

        let narrower_rows = cached_output_visual_row_count(&cache, text, 2, 8);
        assert!(narrower_rows > generation_rows);
        OUTPUT_VISUAL_ROW_COUNT_CALLS.with(|calls| assert_eq!(calls.get(), 1));
    }

    #[test]
    fn displayed_generation_wrap_invalidates_generation_zero_cache_entry() {
        let generation = Cell::new(u64::MAX);
        let cache = Cell::new(Some(OutputVisualRowsCacheEntry {
            effective_cols: 2,
            displayed_generation: 0,
            rows: 999,
        }));

        let wrapped = advance_displayed_generation(&generation, &cache);
        assert_eq!(wrapped, 0);
        assert_eq!(cache.get(), None);
        assert_eq!(
            cached_output_visual_row_count(&cache, "abcd", 2, wrapped),
            2
        );
    }

    /// Run with:
    /// `cargo test --release finished_output_visual_rows_cache_microbenchmark -- --ignored --nocapture`
    #[test]
    #[ignore = "manual microbenchmark"]
    fn finished_output_visual_rows_cache_microbenchmark() {
        use std::hint::black_box;
        use std::time::Instant;

        const REMAPS: usize = 100;
        const PATTERN: &str = "plain terminal output with wide 界 glyphs and tabs\t0123456789\n";

        for target_bytes in [1usize << 20, 8usize << 20] {
            let text = PATTERN.repeat(target_bytes.div_ceil(PATTERN.len()));
            let generation = 19;
            let cols = 80;
            let initial_rows = output_visual_row_count(&text, cols);
            let cache = Cell::new(None);

            reset_visual_row_cache_counters();
            let misses_started = Instant::now();
            for _ in 0..REMAPS {
                cache.set(None);
                black_box(cached_output_visual_row_count(
                    black_box(&cache),
                    black_box(&text),
                    black_box(cols),
                    black_box(generation),
                ));
            }
            let misses = misses_started.elapsed();
            assert_eq!(visual_row_cache_counters(), (0, REMAPS));

            cache.set(Some(OutputVisualRowsCacheEntry {
                effective_cols: cols,
                displayed_generation: generation,
                rows: initial_rows,
            }));
            reset_visual_row_cache_counters();
            let hits_started = Instant::now();
            for _ in 0..REMAPS {
                black_box(cached_output_visual_row_count(
                    black_box(&cache),
                    black_box(&text),
                    black_box(cols),
                    black_box(generation),
                ));
            }
            let hits = hits_started.elapsed();
            assert_eq!(visual_row_cache_counters(), (REMAPS, 0));

            eprintln!(
                "finished-output remap {} MiB x {REMAPS}: first-miss={misses:?}, repeated-hit={hits:?}, speedup={:.1}x",
                target_bytes >> 20,
                misses.as_secs_f64() / hits.as_secs_f64(),
            );
        }
    }

    #[test]
    fn every_snapshot_feed_carries_its_own_reset() {
        // The reset must ride in the byte stream, not be a separate reset() call:
        // VTE parses feeds asynchronously, so an immediate reset cannot drop an
        // earlier render's queued bytes and the two feeds get parsed back to back.
        let payload = snapshot_payload(b"listing");
        assert!(payload.starts_with(b"\x1bc"));
        assert_eq!(&payload[2..], b"listing");
    }

    #[test]
    fn a_cap_that_does_not_change_the_rows_on_screen_keeps_the_same_render() {
        // The regression that doubled `ls` output: a block is rendered on map
        // against the configured viewport cap, then the layout's first fitted pass
        // hands it a much smaller cap in the same main-loop iteration. A 3-row
        // result shows the same 3 rows either way, so it must not be re-fed.
        assert_eq!(
            output_render_stamp(137, 3, 24, 0),
            output_render_stamp(137, 3, 3, 0)
        );
    }

    #[test]
    fn a_render_that_would_look_different_is_not_skipped() {
        let base = output_render_stamp(137, 40, 24, 0);
        // Narrower pane: same text wraps into more rows.
        assert_ne!(base, output_render_stamp(135, 40, 24, 0));
        // A cap that clips more of the output.
        assert_ne!(base, output_render_stamp(137, 40, 12, 0));
        // Expanded past the content: all 40 rows on screen, and no longer clipped.
        assert_ne!(base, output_render_stamp(137, 40, 200, 0));
        // New filter text.
        assert_ne!(base, output_render_stamp(137, 40, 24, 1));
    }

    #[test]
    fn only_a_content_change_earns_a_re_feed() {
        let base = output_render_stamp(137, 40, 24, 0);
        // Cap moved: for a long block this changes the stamp (the visible rows
        // ARE the cap), but the ring already holds the right bytes.
        assert!(!stamp_change_needs_refeed(
            base,
            output_render_stamp(137, 40, 12, 0)
        ));
        // Expanded past the content: still only a window change.
        assert!(!stamp_change_needs_refeed(
            base,
            output_render_stamp(137, 40, 200, 0)
        ));
        // Different wrap width: the ring's line breaks are wrong.
        assert!(stamp_change_needs_refeed(
            base,
            output_render_stamp(135, 40, 24, 0)
        ));
        // Different displayed text (a filter was applied).
        assert!(stamp_change_needs_refeed(
            base,
            output_render_stamp(137, 40, 24, 1)
        ));
        // The construction-time zero stamp always feeds.
        assert!(stamp_change_needs_refeed((0, 0, false, 0), base));
    }

    #[test]
    fn snapshot_rows_stay_within_the_cap_the_layout_gave_the_block() {
        assert_eq!(snapshot_visible_rows(3, 24), 3);
        assert_eq!(snapshot_visible_rows(40, 24), 24);
        assert_eq!(snapshot_visible_rows(0, 24), 1);
        assert_eq!(snapshot_visible_rows(5, 0), 1);
    }

    #[test]
    fn long_output_rows_fill_the_viewport_minus_a_constant_reserve() {
        let reserve = super::super::MIN_INPUT_ROWS as i64 + FINISHED_BLOCK_NON_OUTPUT_ROWS;
        assert_eq!(
            fitted_output_rows_for_viewport(Some(40), 24, 500),
            40 - reserve
        );
        // Never below three rows, however cramped the pane.
        assert_eq!(fitted_output_rows_for_viewport(Some(4), 24, 500), 3);
        // Short output is its own cap.
        assert_eq!(fitted_output_rows_for_viewport(Some(40), 24, 12), 12);
        // No pane yet: fall back to the block's configured rows.
        assert_eq!(fitted_output_rows_for_viewport(None, 24, 500), 24);
    }

    /// A capped block always leaves room for the live input cell's minimum, so
    /// the layout never has to claw rows back from the history once a command
    /// starts — the re-fit that made the whole history blink twice per command.
    #[test]
    fn block_cap_always_leaves_room_for_the_minimum_input_cell() {
        for viewport in [12_i64, 24, 40, 80, 200] {
            let cap = fitted_output_rows_for_viewport(Some(viewport), 24, 10_000);
            assert!(
                cap + super::super::MIN_INPUT_ROWS as i64 <= viewport,
                "cap {cap} leaves no room for the input cell in a {viewport}-row pane"
            );
        }
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
            filter_output_lines("alpha\nBeta\ngamma", "BETA", false, false, false, 0).as_deref(),
            Some("Beta")
        );
    }

    #[test]
    fn filter_output_override_matches_legacy_line_and_newline_semantics() {
        let transcripts = [
            "",
            "alpha",
            "alpha\n",
            "\n",
            "alpha\nBeta\ngamma",
            "alpha\n\nBeta\n",
            "alpha\r\nBeta\r\ngamma\r\n",
            "bare\rcarriage\nwide 界\nALPHA",
        ];
        let cases = [
            ("", false, false, false, 0),
            ("alpha", false, true, false, 0),
            ("ALPHA", false, false, false, 0),
            ("nomatch", false, false, false, 0),
            ("a", false, false, true, 1),
            ("^.*$", true, true, false, 0),
            ("Beta", true, true, false, 9),
            ("[", true, false, false, 0),
        ];

        for full in transcripts {
            for (query, use_regex, case_sensitive, invert, context) in cases {
                let legacy = legacy_filter_output_lines(
                    full,
                    query,
                    use_regex,
                    case_sensitive,
                    invert,
                    context,
                );
                let filtered =
                    filter_output_lines(full, query, use_regex, case_sensitive, invert, context);
                assert_eq!(
                    filtered.as_deref().unwrap_or(full),
                    legacy,
                    "full={full:?}, query={query:?}, regex={use_regex}, case={case_sensitive}, invert={invert}, context={context}",
                );
            }
        }
    }

    #[test]
    fn filter_output_borrows_full_when_override_is_unnecessary() {
        let full = "alpha\nBeta\ngamma";
        assert_eq!(filter_output_lines(full, "", false, false, false, 0), None);
        assert_eq!(filter_output_lines(full, "[", true, false, false, 0), None);
        assert_eq!(filter_output_lines(full, ".*", true, true, false, 9), None,);
        // Legacy `lines().join("\n")` removes the final LF and normalizes
        // CRLF, so these still require an override even when every line is kept.
        assert_eq!(
            filter_output_lines("alpha\n", ".*", true, true, false, 9).as_deref(),
            Some("alpha"),
        );
        assert_eq!(
            filter_output_lines("alpha\r\nBeta", ".*", true, true, false, 9).as_deref(),
            Some("alpha\nBeta"),
        );
    }

    /// Run with:
    /// `cargo test --release finished_output_filter_microbenchmark -- --ignored --nocapture`
    #[test]
    #[ignore = "manual microbenchmark"]
    fn finished_output_filter_microbenchmark() {
        use std::hint::black_box;
        use std::time::Instant;

        const BYPASS_RUNS: usize = 100;
        const FILTER_RUNS: usize = 20;
        const PATTERN: &str = "keep alpha wide 界 payload\ndrop beta ordinary payload\ndrop gamma context payload\ndrop delta payload\n";

        for target_bytes in [1usize << 20, 8usize << 20] {
            let full = PATTERN.repeat(target_bytes.div_ceil(PATTERN.len()));

            let legacy_bypass_started = Instant::now();
            for _ in 0..BYPASS_RUNS {
                black_box(legacy_filter_output_lines(
                    black_box(&full),
                    "",
                    false,
                    false,
                    false,
                    0,
                ));
            }
            let legacy_bypass = legacy_bypass_started.elapsed();
            let borrowed_bypass_started = Instant::now();
            for _ in 0..BYPASS_RUNS {
                black_box(filter_output_lines(
                    black_box(&full),
                    "",
                    false,
                    false,
                    false,
                    0,
                ));
            }
            let borrowed_bypass = borrowed_bypass_started.elapsed();

            let expected = legacy_filter_output_lines(&full, "keep", false, true, false, 1);
            assert_eq!(
                filter_output_lines(&full, "keep", false, true, false, 1).as_deref(),
                Some(expected.as_str()),
            );
            let legacy_filter_started = Instant::now();
            for _ in 0..FILTER_RUNS {
                black_box(legacy_filter_output_lines(
                    black_box(&full),
                    "keep",
                    false,
                    true,
                    false,
                    1,
                ));
            }
            let legacy_filter = legacy_filter_started.elapsed();
            let single_build_started = Instant::now();
            for _ in 0..FILTER_RUNS {
                black_box(filter_output_lines(
                    black_box(&full),
                    "keep",
                    false,
                    true,
                    false,
                    1,
                ));
            }
            let single_build = single_build_started.elapsed();

            eprintln!(
                "finished-filter {} MiB: bypass x{BYPASS_RUNS} clone={legacy_bypass:?} borrow={borrowed_bypass:?} ({:.1}x); active x{FILTER_RUNS} legacy={legacy_filter:?} single-build={single_build:?} ({:.2}x)",
                target_bytes >> 20,
                legacy_bypass.as_secs_f64() / borrowed_bypass.as_secs_f64(),
                legacy_filter.as_secs_f64() / single_build.as_secs_f64(),
            );
        }
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

    #[test]
    fn alt_screen_exit_never_restores_stale_organism_visibility() {
        let entered = live_organism_alt_transition(true, true);
        assert_eq!(entered, (false, true));
        assert!(!live_organism_is_visible(entered.0, entered.1));

        let exited = live_organism_alt_transition(entered.0, false);
        assert_eq!(exited, (false, false));
        assert!(!live_organism_is_visible(exited.0, exited.1));
        assert!(live_organism_is_visible(true, exited.1));
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

// Non-terminal vertical chrome in a finished card. Roomy cards spend 34px in
// margins/header/border/padding. Compact removes 6px of outer margins, 4px of
// header margins and 3px of bottom CSS padding, for an exact 13px reduction.
const FINISHED_CARD_ROOMY_VCHROME_PX: i32 = 34;
const FINISHED_CARD_COMPACT_VCHROME_PX: i32 = 21;

const fn finished_card_vchrome_px(compact: bool) -> i32 {
    if compact {
        FINISHED_CARD_COMPACT_VCHROME_PX
    } else {
        FINISHED_CARD_ROOMY_VCHROME_PX
    }
}

pub(crate) fn estimated_finished_block_height(config: &Config, output_rows: i64) -> i32 {
    let cell = estimated_cell_height_px(config);
    // Header + command row + output rows + margins/borders/filter slack.
    let rows = output_rows.clamp(1, i32::MAX as i64) as i32;
    rows.saturating_add(2)
        .saturating_mul(cell)
        .saturating_add(finished_card_vchrome_px(config.block_compact))
}

/// Values `finalize_block` already derived from the very bytes it is about to
/// hand to [`FinishedBlock::new_with_pool`].
///
/// Both of these are pure functions of `(output, cols)` and both are expensive:
/// the row count is a unicode-width walk (plus a `strip_ansi` replay when the
/// text still has escapes in it) and the retention estimate folds every byte.
/// Recomputing them inside the constructor walked a 1.3 MB transcript a second
/// and third time for numbers the caller was already holding. `None` keeps the
/// old self-sufficient behavior for restore paths and tests.
#[derive(Clone, Copy, Default)]
pub(crate) struct FinishedBlockPrecomputed {
    pub(crate) output_rows: Option<i64>,
    pub(crate) retained_bytes: Option<usize>,
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
    estimated_finished_block_height_for_rows(config, output_visual_row_count(output, cols), cols)
}

/// The same estimate from a row count the caller already holds.
///
/// `output_visual_row_count` is a per-character unicode-width walk over the
/// whole transcript, preceded by a full `strip_ansi` whenever the text still
/// carries escapes. `finalize_block` needs that count anyway to build the card,
/// so it derives it once and threads it through here instead of paying for a
/// second identical walk on the way to the same number.
pub(crate) fn estimated_finished_block_height_for_rows(
    config: &Config,
    output_rows: i64,
    cols: i64,
) -> i32 {
    let requested_rows = output_rows
        .min(config.finished_block_viewport_rows as i64)
        .max(1);
    let visible_rows = bounded_finished_viewport_rows(cols, requested_rows);
    estimated_finished_block_height(config, visible_rows)
}

fn flash_button_icon(btn: &gtk::Button, icon_name: &'static str, tooltip: &'static str) {
    let old_icon = btn.icon_name().map(|s| s.to_string());
    let old_tooltip = btn.tooltip_text().map(|s| s.to_string());
    set_icon_button(btn, icon_name, tooltip);
    let btn_for_restore = btn.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(900), move || {
        if let Some(icon_name) = old_icon.as_deref() {
            btn_for_restore.set_icon_name(icon_name);
        }
        btn_for_restore.set_tooltip_text(old_tooltip.as_deref());
        if let Some(accessible_label) = old_tooltip.as_deref() {
            btn_for_restore.update_property(&[gtk::accessible::Property::Label(accessible_label)]);
        }
    });
}

/// Feed one snapshot into a read-only finished VTE so the render is atomic with
/// respect to VTE's parse queue.
///
/// `Terminal::feed` only *queues* bytes; VTE parses them from its own main-loop
/// source. An immediate `reset()` therefore cannot drop what an earlier render
/// already queued, so two renders in the same main-loop iteration were parsed
/// back to back and the block showed its output twice — the doubled `ls` listing
/// this fixes. RIS (`ESC c`) travels *with* these bytes, so it clears whatever is
/// still queued ahead of them, in order, whether or not VTE has caught up yet.
/// Outer margins and density class of a finished card.
///
/// Construction and the live setter share this so a pane cannot end up with
/// half its cards at one density and half at the other; the CSS side of the
/// same switch keys off the `block-compact` class set here.
fn apply_card_density(outer: &gtk::Box, compact: bool) {
    if compact {
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
}

/// Header-strip margins for the same two densities. See [`apply_card_density`].
fn apply_header_density(header_row: &gtk::Box, compact: bool) {
    if compact {
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
}

fn apply_review_body_density(body: &gtk::Widget, compact: bool) {
    let side = if compact { 8 } else { 12 };
    body.set_margin_start(side);
    body.set_margin_end(side);
    body.set_margin_top(2);
    body.set_margin_bottom(if compact { 7 } else { 11 });
}

fn apply_agent_body_density(body: &gtk::Widget, compact: bool) {
    let side = if compact { 8 } else { 12 };
    body.set_margin_start(side);
    body.set_margin_end(side);
    body.set_margin_top(2);
    body.set_margin_bottom(if compact { 6 } else { 10 });
}

/// Update an already-mounted correction, suggestion, Agent or integration
/// notice without depending on which subsystem built it. These transient
/// assistant cards are not [`FinishedBlock`]s, but their stable CSS roles are
/// enough to update the imperative outer/header/body margins in place.
pub(crate) fn apply_inline_assistant_density(root: &gtk::Widget, compact: bool) -> bool {
    if !root.has_css_class("block-assistant") {
        return false;
    }
    let Ok(outer) = root.clone().downcast::<gtk::Box>() else {
        return false;
    };
    apply_card_density(&outer, compact);

    // Hand-built suggestion and Agent-session bodies predate the shared body
    // roles. They are the sole non-header Box directly below their roots.
    if outer.has_css_class("command-suggestion") || outer.has_css_class("block-agent") {
        let mut child = outer.first_child();
        while let Some(widget) = child {
            let next = widget.next_sibling();
            if !widget.has_css_class("block-header") && widget.is::<gtk::Box>() {
                if outer.has_css_class("command-suggestion") {
                    apply_review_body_density(&widget, compact);
                } else {
                    apply_agent_body_density(&widget, compact);
                }
            }
            child = next;
        }
    }

    fn walk(widget: &gtk::Widget, compact: bool) {
        if widget.has_css_class("block-header") || widget.has_css_class("command-review-header") {
            if let Some(header) = widget.downcast_ref::<gtk::Box>() {
                apply_header_density(header, compact);
            }
        }
        if widget.has_css_class("command-review-body") {
            apply_review_body_density(widget, compact);
        } else if widget.has_css_class("agent-msg-body") {
            apply_agent_body_density(widget, compact);
        }

        let mut child = widget.first_child();
        while let Some(current) = child {
            let next = current.next_sibling();
            walk(&current, compact);
            child = next;
        }
    }
    walk(root, compact);
    true
}

/// Keep finished-card widgets, fixed virtualization placeholders and their
/// parallel metadata document on one density in one indexed pass. A zero
/// metadata height means the card is filtered out of the document; its private
/// placeholder still changes density for a later reveal, but zero must remain
/// zero while it is absent.
pub(crate) fn apply_finished_card_density(
    finished: &[FinishedBlock],
    block_data: &mut VecDeque<BlockData>,
    compact: bool,
) {
    debug_assert_eq!(finished.len(), block_data.len());
    for (card, data) in finished.iter().zip(block_data.iter_mut()) {
        let height = card.set_compact(compact);
        if data.estimated_height != 0 {
            data.estimated_height = height;
        }
    }
}

fn feed_snapshot_bytes(vte: &vte4::Terminal, bytes: &[u8]) {
    vte.feed(&snapshot_payload(bytes));
}

/// In-stream terminal reset carried at the head of every snapshot feed.
const SNAPSHOT_RESET: &[u8] = b"\x1bc";

fn snapshot_payload(bytes: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(bytes.len() + SNAPSHOT_RESET.len());
    payload.extend_from_slice(SNAPSHOT_RESET);
    payload.extend_from_slice(bytes);
    payload
}

/// Rows a snapshot render shows: the content height, bounded by the viewport cap
/// the layout gave this block.
fn snapshot_visible_rows(content_rows: i64, viewport_cap: i64) -> i64 {
    content_rows.min(viewport_cap.max(1)).max(1)
}

/// Identity of one rendered snapshot: columns, the rows it shows, whether it
/// fits its cap (so it settles to its content instead of scrolling internally),
/// and the generation of the text being displayed.
///
/// Two renders with equal stamps produce identical bytes and geometry, so the
/// second is pure churn — and churn is not free here: a re-feed within the same
/// main-loop iteration is what duplicated block output. Keying this on the *cap*
/// instead of the rows it yields is what made every new block render twice — a
/// short block's height is its content either way, but the cap it was measured
/// against changes from the map-time default to the layout's fitted value.
/// `(effective_cols, visible_rows, fits, generation)` — see
/// [`stamp_change_needs_refeed`] for which half is content and which geometry.
/// Identity of what a card's output VTE currently holds: wrap columns, visible
/// rows, whether the whole document fits, and which displayed text generation
/// produced it. Two equal stamps mean the parsed ring and the window onto it
/// are both unchanged; any difference means a re-feed or a re-window happened,
/// which moves every native search position inside that VTE.
pub(crate) type RenderStamp = (i64, i64, bool, u64);

/// The stamp reported by a surface with no independently re-rendered snapshot.
/// Real card stamps always have positive columns and rows, so they cannot alias
/// this value.
pub(crate) const NEUTRAL_RENDER_STAMP: RenderStamp = (0, 0, false, 0);

fn output_render_stamp(
    cols: i64,
    content_rows: i64,
    viewport_cap: i64,
    generation: u64,
) -> RenderStamp {
    (
        cols,
        snapshot_visible_rows(content_rows, viewport_cap),
        content_rows <= viewport_cap,
        generation,
    )
}

/// Test-only view of [`output_render_stamp`], used by the find-state contract
/// without duplicating the stamp packing rule.
#[cfg(test)]
pub(crate) fn output_render_stamp_for_test(
    cols: i64,
    output_rows: i64,
    cap: i64,
    generation: u64,
) -> RenderStamp {
    output_render_stamp(cols, output_rows, cap, generation)
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
    let requested_visible_rows = settle_probe_rows(output_rows.min(viewport_cap));
    let overflow_rows = output_rows
        .saturating_sub(requested_visible_rows)
        .saturating_add(64);
    let requested_scrollback = capture_rows.max(overflow_rows).max(64);
    let (cols, visible_rows, scrollback) =
        bounded_finished_vte_geometry(cols, requested_visible_rows, requested_scrollback);
    vte.set_scroll_on_output(false);
    // Size and arm scrollback BEFORE reset/feed. Reset may clamp the grid on some
    // VTE builds, so both are reasserted before processing the snapshot bytes.
    vte.set_size(cols.max(1), visible_rows);
    vte.set_scrollback_lines(scrollback);
    vte.reset(true, true);
    vte.set_size(cols.max(1), visible_rows);
    vte.set_scrollback_lines(scrollback);
    feed_snapshot_bytes(vte, display_text.as_bytes());
    if expand_to_buffer {
        settle_finished_terminal_after_feed(vte, visible_rows, viewport_cap.max(visible_rows));
    }
    if let Some(adj) = vte.vadjustment() {
        adj.set_value(adj.lower());
    }
    settle_vte_to_top(vte);
}

/// The small measuring grid a settle starts from.
///
/// `fit_finished_terminal_to_content` clamps what it measures up to the floor it
/// is given, so the floor has to be small enough that an over-counted snapshot
/// can still shrink to the rows VTE really drew. Both the feed path and the
/// re-window path go through here so the two cannot drift apart.
fn settle_probe_rows(rows: i64) -> i64 {
    rows.clamp(1, 32)
}

/// Whether a render-stamp change means the bytes in VTE are wrong, or only the
/// window onto them.
///
/// The stamp is `(effective_cols, visible_rows, fits, generation)`. Columns
/// decide how the transcript wraps and the generation identifies which text is
/// displayed; those two are the content. The other two are geometry, and
/// geometry alone never invalidates a parsed ring.
fn stamp_change_needs_refeed(previous: RenderStamp, next: RenderStamp) -> bool {
    previous.0 != next.0 || previous.3 != next.3
}

/// Re-window a finished VTE that already holds the right bytes.
///
/// How many rows a card SHOWS is not a property of what was fed into it: the
/// transcript is already parsed and sitting in VTE's ring, and the visible grid
/// is a window onto that ring. `render_bytes_into_finished_vte` was being used
/// for this anyway, which reset the terminal and re-parsed the whole snapshot —
/// up to a 1.3 MB re-parse per card. The re-fit sweep that asks for it is
/// driven from the frame clock, so dragging a window edge re-parsed every
/// mapped long block on every frame of the drag.
///
/// The scrollback dance mirrors `render_bytes_into_finished_vte`: arm a
/// generous limit first so no `set_size` in either direction can trim the ring,
/// then settle on the real one, which keeps screen + scrollback at exactly the
/// same total the feed path left behind.
fn rewindow_finished_vte(
    vte: &vte4::Terminal,
    cols: i64,
    visible_rows: i64,
    requested_scrollback: i64,
    settle_cap: Option<i64>,
) {
    let (cols, visible_rows, scrollback) =
        bounded_finished_vte_geometry(cols, visible_rows.max(1), requested_scrollback.max(64));
    vte.set_scrollback_lines(bounded_finished_vte_max_rows(cols));
    vte.set_size(cols.max(1), visible_rows);
    vte.set_scrollback_lines(scrollback);
    if let Some(cap) = settle_cap {
        // Same probe floor the feed path hands the settle. `fit_finished_terminal_to_content`
        // clamps its measurement UP to this floor, so passing the full row estimate would
        // stop an over-counted snapshot from ever shrinking to what VTE actually rendered —
        // `strip_ansi` replays horizontal motion only, so CUU/EL progress output counts far
        // more rows than it draws.
        settle_finished_terminal_after_feed(
            vte,
            settle_probe_rows(visible_rows),
            cap.max(visible_rows),
        );
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

    /// The transcript a search should scan: whatever this card is displaying.
    ///
    /// Counting a hit in a line the filter has hidden makes the VTE's own
    /// `search_find_next` come up empty, and the find pass reads that as "no
    /// matches" — then clears the query for the **whole session**, not just for
    /// this block. One filtered card could therefore zero the result count for
    /// every other card.
    ///
    /// `try_borrow`: the filter's apply closure holds this mutably while it
    /// re-renders, on the same main loop. Falling back to the full transcript
    /// there can over-count inside one block for one pass; it cannot invalidate
    /// the pass.
    pub(crate) fn with_searchable_output<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        match self.displayed_output.try_borrow() {
            Ok(displayed) => match displayed.as_deref() {
                Some(filtered) => f(filtered),
                None => f(&self.full_output.borrow()),
            },
            Err(_) => f(&self.full_output.borrow()),
        }
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
            output.len(),
            None,
            FinishedBlockPrecomputed::default(),
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
        plain_output_bytes: usize,
        recycled: Option<gtk::Box>,
        precomputed: FinishedBlockPrecomputed,
    ) -> Self {
        let is_background = cmd.trim().is_empty();
        let cols = cols.clamp(1, MAX_FINISHED_VTE_COLUMNS);
        let estimated_retained_bytes = precomputed.retained_bytes.unwrap_or_else(|| {
            estimated_live_finished_block_retained_bytes(
                prompt,
                cmd,
                cmd_ansi,
                output,
                plain_output_bytes,
                cwd,
                cols,
                images,
            )
        });

        // Keep ordinary output on the outer continuous canvas, but cap very long
        // snapshots. GTK cannot allocate an arbitrarily tall single widget, so
        // long blocks retain VTE's private scrollback inside this viewport.
        let output_rows = precomputed
            .output_rows
            .unwrap_or_else(|| output_visual_row_count(output, cols));
        let estimated_visible_rows = bounded_finished_viewport_rows(
            cols,
            output_rows
                .min(config.finished_block_viewport_rows as i64)
                .max(1),
        );
        let (_, viewport_cap, _) = bounded_finished_vte_geometry(
            cols,
            (config.finished_block_viewport_rows as i64).max(1),
            0,
        );
        let dynamic_viewport_rows = Rc::new(Cell::new(viewport_cap));
        let (_, max_expanded_cap, _) = bounded_finished_vte_geometry(
            cols,
            (config.finished_block_max_expanded_rows as i64)
                .min(1000)
                .max(viewport_cap),
            0,
        );
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
            reused.remove_css_class("block-bookmarked");
            // A pooled widget keeps every class it was last given, so the new
            // block's status stripe would sit under the recycled one.
            reused.remove_css_class("block-unknown");
            // Same for the lifecycle notice. Only a degraded record sets one,
            // and only as an `if let Some` with no `else`, so a healthy card
            // built on a recycled shell inherited the dead block's explanation
            // of why *its* status could not be trusted.
            reused.set_has_tooltip(false);
            reused
        } else {
            let b = gtk::Box::new(Orientation::Vertical, 0);
            b.add_css_class("block-finished");
            b
        };
        // Pooled cards must not inherit the placeholder geometry of whatever
        // block last used them; virtualization owns this request from here on.
        outer.set_hexpand(true);
        outer.set_vexpand(false);
        outer.set_height_request(-1);
        // A pooled card may also have been hidden outright by an alt-screen
        // takeover; a new block always starts on screen.
        outer.set_visible(true);
        // One level below the card: virtualization hides this and pins the
        // card's height, so an off-screen block keeps its place in the document
        // instead of collapsing it. See `set_virtualized`.
        let content = gtk::Box::new(Orientation::Vertical, 0);
        content.set_hexpand(true);
        content.set_vexpand(false);
        outer.append(&content);
        apply_card_density(&outer, config.block_compact);

        // Status stripe: green on success, red on failure, cyan for output
        // emitted while the shell prompt was idle (Warp background blocks),
        // neutral when the shell never reported a status.
        let status = block_status(Some(cmd), exit_code);
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
        apply_header_density(&header_row, config.block_compact);

        // Warp-style accent prompt chevron. Leads the header rather than the
        // command row; see the command-row comment below.
        let chevron = gtk::Label::new(Some("\u{276f}"));
        chevron.add_css_class("block-prompt-chevron");
        chevron.set_visible(!is_background);
        header_row.append(&chevron);

        // Bookmark marker (gutter marker), hidden until the block is bookmarked.
        let bookmark_star = gtk::Image::from_icon_name("user-bookmarks-symbolic");
        bookmark_star.add_css_class("block-bookmark-star");
        bookmark_star.update_property(&[gtk::accessible::Property::Label("Bookmarked block")]);
        bookmark_star.set_halign(gtk::Align::Start);
        bookmark_star.set_visible(false);
        header_row.append(&bookmark_star);

        // Status icon: success, failure, asynchronous/background output, or an
        // unknown result when the shell reported nothing.
        let (status_icon_name, status_class, status_accessible_label) = status.icon();
        let status_icon = gtk::Image::from_icon_name(status_icon_name);
        status_icon.add_css_class(status_class);
        status_icon.set_tooltip_text(status.icon_tooltip());
        status_icon.update_property(&[gtk::accessible::Property::Label(status_accessible_label)]);
        status_icon.set_halign(gtk::Align::Start);
        header_row.append(&status_icon);

        if is_background {
            let background_chip = gtk::Label::new(Some("Background output"));
            background_chip.add_css_class("block-background-chip");
            background_chip.set_halign(gtk::Align::Start);
            header_row.append(&background_chip);
        }

        // Lifecycle chip: shown only when this record's completion is not fully
        // trusted. It used to be a card-level tooltip, which is the one place a
        // caveat about an exit code cannot be seen — the header's own chips and
        // buttons shadow it, and a tooltip has to be hunted for. The full
        // explanation is this chip's tooltip, where nothing can shadow it.
        let lifecycle_chip = gtk::Label::new(None);
        lifecycle_chip.add_css_class("block-lifecycle-chip");
        lifecycle_chip.set_halign(gtk::Align::Start);
        lifecycle_chip.set_max_width_chars(14);
        lifecycle_chip.set_ellipsize(gtk::pango::EllipsizeMode::End);
        lifecycle_chip.set_visible(false);
        header_row.append(&lifecycle_chip);

        // Context chips (Warp-style): cwd pill + git-branch pill.
        if let Some(cwd_path) = cwd {
            let shortened = shorten_path(cwd_path);
            let cwd_chip = gtk::Label::new(Some(&format!("Folder: {shortened}")));
            cwd_chip.add_css_class("block-chip");
            // The chip shows `…/two/tail`; the tooltip is where the rest of a
            // deep path lives.
            cwd_chip.set_tooltip_text(Some(cwd_path));
            cwd_chip.set_halign(gtk::Align::Start);
            cwd_chip.set_ellipsize(gtk::pango::EllipsizeMode::Start);
            cwd_chip.set_max_width_chars(40);
            header_row.append(&cwd_chip);

            if let Some(branch) = git_branch_for(cwd_path) {
                let git_chip = gtk::Label::new(Some(&format!("Branch: {branch}")));
                git_chip.add_css_class("block-chip-git");
                git_chip.set_halign(gtk::Align::Start);
                git_chip.set_ellipsize(gtk::pango::EllipsizeMode::End);
                git_chip.set_max_width_chars(28);
                header_row.append(&git_chip);
            }
        }

        // A selected card is a lightweight navigation mode. Put its actual
        // keyboard actions on screen instead of requiring the user to remember
        // them. This label deliberately precedes the expanding spacer: it
        // spends the spacer's slack and therefore cannot shove timestamp,
        // duration, exit status, or quick actions sideways as selection moves.
        let selection_hint = gtk::Label::new(None);
        selection_hint.add_css_class("block-selection-hint");
        selection_hint.set_accessible_role(gtk::AccessibleRole::Status);
        selection_hint.set_visible(false);
        // Escape is first, so end ellipsis in a narrow split preserves the
        // universal way out before trimming lower-priority actions.
        selection_hint.set_ellipsize(gtk::pango::EllipsizeMode::End);
        // Reserve the complete universal exit even when metadata/actions crowd
        // a narrow header, while still capping the longest capability row.
        selection_hint.set_width_chars(super::SELECTION_HINT_MIN_CHARS);
        selection_hint.set_max_width_chars(super::SELECTION_HINT_MAX_CHARS);
        header_row.append(&selection_hint);

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

        // Duration badge. Shares Unified's formatter: the card used to round
        // whole minutes off a float, so a 90-second command was labelled `2m`
        // and an hour was labelled `60m`.
        if let Some(dur_ms) = duration_ms {
            let duration_text = super::unified_chrome::format_block_duration(dur_ms);
            let dur_label = gtk::Label::new(Some(&duration_text));
            dur_label.add_css_class("block-meta-badge");
            // The badge rounds; the tooltip does not. Comparing two runs of the
            // same build is the reason anyone reads this number at all.
            dur_label.set_tooltip_text(Some(&format!("{dur_ms} ms")));
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
        // Faded, not hidden. Hiding it removed ~150px from a hexpand header, so
        // every timestamp/duration/exit badge slid left on hover and snapped
        // back on leave — once per card while dragging the pointer down a list.
        // `can_target(false)` is load-bearing: an invisible-but-present button
        // would otherwise swallow the header clicks that select the block.
        action_box.add_css_class("block-action-box");
        reveal_block_actions(&action_box, false);
        // Small gap between the meta badges (timestamp/duration/exit) on the
        // right and the action button group, so they read as separate units
        // rather than one undifferentiated cluster.
        action_box.set_margin_start(6);
        // Three distinct glyphs. The two copy actions shared `edit-copy` and
        // were therefore separable only by hovering for a tooltip, which is the
        // one thing a quick-action row exists to avoid.
        let copy_cmd_btn = icon_button("edit-copy-symbolic", "Copy command");
        let copy_output_btn = icon_button("text-x-generic-symbolic", "Copy output");
        let rerun_btn = icon_button("insert-text-symbolic", "Insert command at prompt");
        // Commandless background blocks retain output actions, find/filter,
        // bookmarks and selection, but cannot copy or recall a command.
        copy_cmd_btn.set_visible(!is_background);
        rerun_btn.set_visible(!is_background);
        let filter_btn = icon_button("edit-find-symbolic", "Filter output");
        let jump_bottom_btn = icon_button("go-bottom-symbolic", "Jump to bottom of this block");
        jump_bottom_btn.set_visible(long_output);
        // Expand button: appears only when output_rows > viewport_cap; toggles
        // the output VTE between the capped height and a roomier expanded height
        // (`finished_block_max_expanded_rows`). Wired below once output_rows and
        // the output VTE exist.
        let expand_btn = icon_button("view-fullscreen-symbolic", "Expand block");
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
            reveal_block_actions(&action_box_for_enter, true);
        });
        let outer_for_leave = outer.clone();
        let action_box_for_leave = action_box.clone();
        hover_ctrl.connect_leave(move |_| {
            outer_for_leave.remove_css_class("block-hovered");
            // Only the active edge of a multi-selection owns persistent actions.
            if !outer_for_leave.has_css_class("block-selection-active") {
                reveal_block_actions(&action_box_for_leave, false);
            }
        });
        outer.add_controller(hover_ctrl);

        // Collapse toggle button
        let collapse_btn = icon_button("go-down-symbolic", "Hide output");
        collapse_btn.add_css_class("block-collapse-btn");
        collapse_btn.add_css_class("flat");
        header_row.append(&collapse_btn);

        content.append(&header_row);

        // ── VTE-rendered command + output ─────────────────────────────────
        // Command VTE: single-row read-only renderer for the executed command.
        let cmd_bytes = rendered_command_bytes(cmd, cmd_ansi);
        // Captured command strings use logical newlines. Convert them to CRLF
        // for VTE so every pasted/continued line begins at the command column.
        let cmd_bytes = Rc::new(terminalize_line_breaks(&cmd_bytes));
        // Allocate every logical command row up front; VTE's post-feed pass
        // adds any further rows caused by soft wrapping or control sequences.
        let cmd_rows = cmd_bytes.iter().filter(|&&b| b == b'\n').count() as i64 + 1;
        let command_vte =
            create_finished_terminal(config, cols, cmd_rows.max(1), cmd_rows.max(1), true);
        let command_render_cols = Rc::new(Cell::new(0_i64));
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
            let command_render_cols = command_render_cols.clone();
            command_vte.connect_map(move |w| {
                let effective_cols = effective_render_cols(w, cols_for_map);
                if command_render_cols.replace(effective_cols) == effective_cols {
                    return;
                }
                let rendered_rows = output_visual_row_count(
                    &String::from_utf8_lossy(cmd_bytes_for_map.as_slice()),
                    effective_cols,
                )
                .max(cmd_rows_for_map);
                let (effective_cols, rendered_rows, scrollback) =
                    bounded_finished_vte_geometry(effective_cols, rendered_rows, 0);
                w.set_size(effective_cols, rendered_rows);
                w.set_scrollback_lines(scrollback);
                w.reset(true, true);
                w.set_size(effective_cols, rendered_rows);
                w.set_scrollback_lines(scrollback);
                feed_snapshot_bytes(w, cmd_bytes_for_map.as_slice());
                settle_finished_terminal_after_feed(w, rendered_rows, rendered_rows);
                // Gtk may otherwise allocate this VTE at one row, leaving the
                // continuation lines in its internal scrollback.
                let ch = w.char_height() as i32;
                if ch > 0 {
                    w.set_height_request(finished_vte_height_px(rendered_rows, ch));
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
        output_vte.set_height_request(finished_vte_height_px(
            initial_visible_rows,
            estimated_cell_height_px(config),
        ));
        // Tracks whether the user has toggled this block to its expanded
        // height. Survives unmap/remap so re-feeding picks the right cap.
        let expanded: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let render_stamp: Rc<Cell<RenderStamp>> = Rc::new(Cell::new(NEUTRAL_RENDER_STAMP));
        let displayed_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        // Construction already paid for the initial transcript scan. Seed the
        // cache so a first map at the recorded width, and every same-width
        // virtualization remap after it, can reuse those rows.
        let visual_rows_cache = Rc::new(Cell::new(Some(OutputVisualRowsCacheEntry {
            effective_cols: cols,
            displayed_generation: 0,
            rows: output_rows,
        })));
        {
            let cols_for_map = cols.max(1);
            let cap_for_map = dynamic_viewport_rows.clone();
            let fallback_cap_for_map = viewport_cap;
            let max_for_map = max_expanded_cap;
            let full_for_map = full_output.clone();
            let displayed_for_map = displayed_output.clone();
            let expanded_for_map = expanded.clone();
            let render_stamp_for_map = render_stamp.clone();
            let displayed_generation_for_map = displayed_generation.clone();
            let visual_rows_cache_for_map = visual_rows_cache.clone();
            let expand_btn_for_map = expand_btn.downgrade();
            let jump_btn_for_map = jump_bottom_btn.downgrade();
            output_vte.connect_map(move |w| {
                let full = full_for_map.borrow();
                let displayed = displayed_for_map.borrow();
                let text = displayed.as_deref().unwrap_or(full.as_str());
                let effective_cols = effective_render_cols(w, cols_for_map);
                let rows = cached_output_visual_row_count(
                    &visual_rows_cache_for_map,
                    text,
                    effective_cols,
                    displayed_generation_for_map.get(),
                );
                // Fit against the pane this card just entered. The global
                // layout pass now runs only on a real geometry change, so a
                // freshly appended (or re-virtualized) card has to establish
                // its own cap here rather than waiting to be swept up.
                let fitted_cap = bounded_finished_viewport_rows(
                    effective_cols,
                    fitted_output_rows_for_widget(w, fallback_cap_for_map, rows),
                );
                cap_for_map.set(fitted_cap);
                let requested_cap = if expanded_for_map.get() {
                    max_for_map
                } else {
                    fitted_cap
                };
                let cap = bounded_finished_viewport_rows(effective_cols, requested_cap);
                if let Some(expand_btn) = expand_btn_for_map.upgrade() {
                    expand_btn.set_visible(rows > fitted_cap);
                }
                if let Some(jump_btn) = jump_btn_for_map.upgrade() {
                    jump_btn.set_visible(rows > fitted_cap);
                }
                let stamp = output_render_stamp(
                    effective_cols,
                    rows,
                    cap,
                    displayed_generation_for_map.get(),
                );
                if render_stamp_for_map.replace(stamp) == stamp {
                    return;
                }
                let visible_rows = snapshot_visible_rows(rows, cap);
                render_bytes_into_finished_vte(
                    w,
                    text,
                    effective_cols,
                    rows,
                    cap,
                    capture_rows,
                    rows <= cap,
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
                    w.set_height_request(finished_vte_height_px(visible_rows, ch));
                }
                if rows <= cap {
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
        expand_btn.set_visible(output_rows > viewport_cap);
        {
            let expand_for_btn = expanded.clone();
            let viewport_for_btn = dynamic_viewport_rows.clone();
            let output_vte_for_btn = output_vte.clone();
            let full_for_btn = full_output.clone();
            let displayed_for_btn = displayed_output.clone();
            let cols_for_btn = cols.max(1);
            let render_stamp_for_btn = render_stamp.clone();
            let displayed_generation_for_btn = displayed_generation.clone();
            let visual_rows_cache_for_btn = visual_rows_cache.clone();
            expand_btn.connect_clicked(move |btn| {
                let now_expanded = !expand_for_btn.get();
                expand_for_btn.set(now_expanded);
                let full = full_for_btn.borrow();
                let displayed = displayed_for_btn.borrow();
                let text = displayed.as_deref().unwrap_or(full.as_str());
                let effective_cols = effective_render_cols(&output_vte_for_btn, cols_for_btn);
                let requested_cap = if now_expanded {
                    max_expanded_cap
                } else {
                    viewport_for_btn.get()
                };
                let cap = bounded_finished_viewport_rows(effective_cols, requested_cap);
                let rows = cached_output_visual_row_count(
                    &visual_rows_cache_for_btn,
                    text,
                    effective_cols,
                    displayed_generation_for_btn.get(),
                );
                let visible_rows = snapshot_visible_rows(rows, cap);
                render_stamp_for_btn.set(output_render_stamp(
                    effective_cols,
                    rows,
                    cap,
                    displayed_generation_for_btn.get(),
                ));
                render_bytes_into_finished_vte(
                    &output_vte_for_btn,
                    text,
                    effective_cols,
                    rows,
                    cap,
                    capture_rows,
                    rows <= cap,
                );
                let ch = output_vte_for_btn.char_height() as i32;
                if ch > 0 {
                    output_vte_for_btn.set_height_request(finished_vte_height_px(visible_rows, ch));
                }
                if now_expanded {
                    set_icon_button(btn, "view-restore-symbolic", "Collapse to default height");
                } else {
                    set_icon_button(btn, "view-fullscreen-symbolic", "Expand block");
                }
            });
        }

        // Command row: just the command VTE. The prompt chevron moved up into
        // the header, because as a sibling here it indented the command by its
        // own width while the output below started at the card's edge — two
        // left margins inside one card, for text meant to be read together.
        // Giving the *output* the matching indent instead was the other way to
        // align them, and it is not available: column count comes from the
        // terminal's pixel width, so the card would re-wrap `ls` differently
        // from the live pane the user watched it in.
        let cmd_row = gtk::Box::new(Orientation::Horizontal, 0);
        cmd_row.append(&command_vte);

        content.append(&cmd_row);
        cmd_row.set_visible(!is_background);
        // The per-block scrollbar rides an overlay, never a box sibling.
        //
        // As a sibling its ~14px came out of the terminal's own allocation, and
        // the condition that shows it — VTE's ring overflowing the visible page
        // — is itself a function of that width. So hiding it widened the
        // terminal by a column, the wider terminal rewrapped its ring to fewer
        // rows, the ring stopped overflowing, and the next frame hid it again:
        // a two-state layout loop that flickered the whole card (and the output
        // row inside it) at frame rate, for as long as the pane stayed open.
        // The loop closes entirely inside GTK — no anvil re-feed is involved —
        // so it cannot be broken by any of the render-stamp guards above it.
        // Overlaying the scrollbar breaks the width edge of that cycle; the
        // live card's scrollbar already rides its clip for the same reason.
        let output_box = gtk::Overlay::new();
        output_box.set_hexpand(true);
        output_box.set_child(Some(&output_vte));
        let output_scrollbar =
            gtk::Scrollbar::new(Orientation::Vertical, output_vte.vadjustment().as_ref());
        output_scrollbar.add_css_class("block-output-scrollbar");
        output_scrollbar.set_visible(output_scrollable);
        output_scrollbar.set_tooltip_text(Some("Scroll within this block"));
        output_scrollbar.set_halign(gtk::Align::End);
        output_scrollbar.set_valign(gtk::Align::Fill);
        if let Some(adjustment) = output_vte.vadjustment() {
            let scrollbar = output_scrollbar.downgrade();
            let sync_visibility = move |adjustment: &gtk::Adjustment| {
                if let Some(scrollbar) = scrollbar.upgrade() {
                    scrollbar.set_visible(
                        adjustment.upper() - adjustment.lower()
                            > adjustment.page_size() + f64::EPSILON,
                    );
                }
            };
            sync_visibility(&adjustment);
            adjustment.connect_changed(sync_visibility);
        }
        output_box.add_overlay(&output_scrollbar);
        output_box.set_measure_overlay(&output_scrollbar, false);
        output_box.set_clip_overlay(&output_scrollbar, true);
        let output_widget: gtk::Widget = output_box.clone().upcast::<gtk::Widget>();
        content.append(&output_box);

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
            content.append(&ib);
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
        content.append(&collapsed_summary);

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
            set_icon_button(&collapse_btn, "go-next-symbolic", "No output");
        } else if has_output {
            set_icon_button(
                &collapse_btn,
                "go-down-symbolic",
                &format!("Toggle output ({})", line_count_text(output_rows)),
            );
        }
        let collapsed_state: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let set_collapsed: Rc<dyn Fn(bool)> = {
            let collapsed_state = collapsed_state.clone();
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
                collapsed_state.set(collapsed);
                // Image-only blocks keep their empty output VTE hidden even
                // while expanded; only the Pictures fold and unfold.
                output_widget.set_visible(!collapsed && has_output);
                if let Some(ib) = images_box.as_ref().and_then(|ib| ib.upgrade()) {
                    ib.set_visible(!collapsed);
                }
                collapsed_summary.set_visible(collapsed);
                set_icon_button(
                    &collapse_btn,
                    if collapsed {
                        "go-next-symbolic"
                    } else {
                        "go-down-symbolic"
                    },
                    if collapsed {
                        "Show output"
                    } else {
                        "Hide output"
                    },
                );
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
        // Same shape as `toggle_filter`: a weak-only closure the menu and the
        // keyboard action can call without either of them knowing how folding
        // is implemented. A 400-line `cargo build` should not need a mouse.
        let toggle_collapsed: Rc<dyn Fn()> = {
            let set_collapsed = set_collapsed.clone();
            let collapsed_state = collapsed_state.clone();
            let can_fold = has_output || has_images;
            Rc::new(move || {
                if can_fold {
                    set_collapsed(!collapsed_state.get());
                }
            })
        };
        if !has_output && !has_images {
            set_icon_button(&collapse_btn, "go-next-symbolic", "No output");
            collapsed_summary.set_visible(false);
        }

        // Per-block output filter (Warp's BlockFilterQuery). Closing the
        // editor disables filtering but deliberately preserves the query/options;
        // reopening it reapplies the same filter, matching Warp's toggle behavior.
        let filter_enabled = Rc::new(Cell::new(false));
        let toggle_filter: Rc<dyn Fn()> = {
            // The filter editor is built on the FIRST toggle, not at card
            // construction. A search entry, three toggles, a spin button and a
            // status label are ~15-20 GtkWidgets per card — a third of the
            // whole history's widget population at the default block cap —
            // and they existed solely so a keystroke could make them visible.
            // Everything the builder needs is captured weakly (widgets) or by
            // Rc (state), exactly as the eager version captured it, so the
            // toggle closure the filter button owns still cannot keep the card
            // alive after eviction.
            type FilterRowHandles = (
                glib::WeakRef<gtk::Box>,
                glib::WeakRef<gtk::SearchEntry>,
                Rc<dyn Fn()>,
            );
            type FilterRowBuilder = dyn Fn(&gtk::Box, &gtk::Box) -> Option<FilterRowHandles>;
            let output_vte = output_vte.downgrade();
            let expand_btn = expand_btn.downgrade();
            let output_scrollbar = output_scrollbar.downgrade();
            let collapsed_summary = collapsed_summary.downgrade();
            let build_filter_row: Rc<FilterRowBuilder> = {
                let full_output = full_output.clone();
                let displayed_output = displayed_output.clone();
                let expanded = expanded.clone();
                let dynamic_viewport_rows = dynamic_viewport_rows.clone();
                let filter_enabled = filter_enabled.clone();
                let render_stamp = render_stamp.clone();
                let displayed_generation = displayed_generation.clone();
                let visual_rows_cache = visual_rows_cache.clone();
                Rc::new(move |content: &gtk::Box, cmd_row: &gtk::Box| {
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

                    content.append(&filter_row);
                    content.reorder_child_after(&filter_row, Some(cmd_row));

                    let apply = {
                        let output_vte = output_vte.clone();
                        let full_output = full_output.clone();
                        let displayed_output = displayed_output.clone();
                        let filter_entry = filter_entry.downgrade();
                        let regex_tg = regex_tg.downgrade();
                        let case_tg = case_tg.downgrade();
                        let invert_tg = invert_tg.downgrade();
                        let ctx_spin = ctx_spin.downgrade();
                        let filter_status = filter_status.downgrade();
                        let expand_btn = expand_btn.clone();
                        let output_scrollbar = output_scrollbar.clone();
                        let expanded = expanded.clone();
                        let dynamic_viewport_rows = dynamic_viewport_rows.clone();
                        let collapsed_summary = collapsed_summary.clone();
                        let filter_enabled = filter_enabled.clone();
                        let render_stamp = render_stamp.clone();
                        let displayed_generation = displayed_generation.clone();
                        let visual_rows_cache = visual_rows_cache.clone();
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
                            let next_display = if filter_enabled.get() {
                                filter_output_lines(
                                    full.as_str(),
                                    &q,
                                    regex_tg.is_active(),
                                    case_tg.is_active(),
                                    invert_tg.is_active(),
                                    ctx_spin.value() as usize,
                                )
                            } else {
                                None
                            };
                            let shown = next_display.as_deref().unwrap_or(full.as_str());
                            let shown_rows = output_row_count(shown);
                            let full_rows = if next_display.is_none() {
                                shown_rows
                            } else {
                                output_row_count(&full)
                            };
                            let effective_cols = effective_render_cols(&output_vte, cols);
                            let display_changed =
                                displayed_output.borrow().as_deref() != next_display.as_deref();
                            let generation = if display_changed {
                                advance_displayed_generation(
                                    &displayed_generation,
                                    &visual_rows_cache,
                                )
                            } else {
                                displayed_generation.get()
                            };
                            let shown_visual_rows = cached_output_visual_row_count(
                                &visual_rows_cache,
                                shown,
                                effective_cols,
                                generation,
                            );
                            let requested_cap = if expanded.get() {
                                max_expanded_cap
                            } else {
                                dynamic_viewport_rows.get()
                            };
                            let active_cap =
                                bounded_finished_viewport_rows(effective_cols, requested_cap);
                            let stamp = output_render_stamp(
                                effective_cols,
                                shown_visual_rows,
                                active_cap,
                                generation,
                            );
                            if render_stamp.replace(stamp) != stamp {
                                render_bytes_into_finished_vte(
                                    &output_vte,
                                    shown,
                                    effective_cols,
                                    shown_visual_rows,
                                    active_cap,
                                    capture_rows,
                                    shown_visual_rows <= active_cap,
                                );
                                let ch = output_vte.char_height() as i32;
                                if ch > 0 {
                                    let probe_rows = shown_visual_rows.min(active_cap).clamp(1, 32);
                                    output_vte
                                        .set_height_request(finished_vte_height_px(probe_rows, ch));
                                }
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
                            if display_changed {
                                *displayed_output.borrow_mut() = next_display;
                            }
                        }
                    };
                    let apply = Rc::new(apply);
                    let pending_apply: Rc<RefCell<Option<glib::SourceId>>> =
                        Rc::new(RefCell::new(None));
                    let apply_generation = Rc::new(Cell::new(0_u64));
                    // Option/context/filter-row changes are explicit actions and apply
                    // immediately. They also invalidate a pending keystroke timeout so
                    // an older query can never render over the newer state.
                    let apply_now = {
                        let pending_apply = pending_apply.clone();
                        let apply_generation = apply_generation.clone();
                        let apply = apply.clone();
                        Rc::new(move || {
                            apply_generation.set(apply_generation.get().wrapping_add(1));
                            if let Some(source) = pending_apply.borrow_mut().take() {
                                source.remove();
                            }
                            apply();
                        })
                    };
                    let schedule_apply = {
                        let pending_apply = pending_apply.clone();
                        let apply_generation = apply_generation.clone();
                        let apply = apply.clone();
                        Rc::new(move || {
                            let generation = apply_generation.get().wrapping_add(1);
                            apply_generation.set(generation);
                            if let Some(source) = pending_apply.borrow_mut().take() {
                                source.remove();
                            }

                            let pending_slot = pending_apply.clone();
                            let pending_clear = pending_apply.clone();
                            let apply_generation = apply_generation.clone();
                            let apply = apply.clone();
                            let source = glib::timeout_add_local(
                                FINISHED_OUTPUT_FILTER_DEBOUNCE,
                                move || {
                                    if apply_generation.get() == generation {
                                        // A stale callback must not clear a newer
                                        // timeout stored in the shared slot.
                                        pending_clear.borrow_mut().take();
                                        apply();
                                    }
                                    glib::ControlFlow::Break
                                },
                            );
                            *pending_slot.borrow_mut() = Some(source);
                        })
                    };
                    {
                        let schedule_apply = schedule_apply.clone();
                        filter_entry.connect_search_changed(move |_| schedule_apply());
                    }
                    for tg in [&regex_tg, &case_tg, &invert_tg] {
                        let apply_now = apply_now.clone();
                        tg.connect_toggled(move |_| apply_now());
                    }
                    {
                        let apply_now = apply_now.clone();
                        ctx_spin.connect_value_changed(move |_| apply_now());
                    }
                    {
                        let pending_apply = pending_apply.clone();
                        let apply_generation = apply_generation.clone();
                        filter_entry.connect_destroy(move |_| {
                            apply_generation.set(apply_generation.get().wrapping_add(1));
                            if let Some(source) = pending_apply.borrow_mut().take() {
                                source.remove();
                            }
                        });
                    }

                    Some((filter_row.downgrade(), filter_entry.downgrade(), apply_now))
                })
            };
            let built: Rc<RefCell<Option<FilterRowHandles>>> = Rc::new(RefCell::new(None));
            let content_for_toggle = content.downgrade();
            let cmd_row_for_toggle = cmd_row.downgrade();
            let filter_btn_for_toggle = filter_btn.downgrade();
            let filter_enabled_for_toggle = filter_enabled.clone();
            let toggle: Rc<dyn Fn()> = Rc::new(move || {
                let (Some(content), Some(cmd_row), Some(filter_btn)) = (
                    content_for_toggle.upgrade(),
                    cmd_row_for_toggle.upgrade(),
                    filter_btn_for_toggle.upgrade(),
                ) else {
                    return;
                };
                if built.borrow().is_none() {
                    let handles = build_filter_row(&content, &cmd_row);
                    *built.borrow_mut() = handles;
                }
                let built = built.borrow();
                let Some((row, entry, apply_now)) = built.as_ref() else {
                    return;
                };
                let (Some(row), Some(entry)) = (row.upgrade(), entry.upgrade()) else {
                    return;
                };
                let show = !row.is_visible();
                row.set_visible(show);
                filter_enabled_for_toggle.set(show);
                if show {
                    filter_btn.add_css_class("block-action-active");
                    entry.grab_focus();
                } else {
                    filter_btn.remove_css_class("block-action-active");
                }
                apply_now();
            });
            let toggle_for_button = toggle.clone();
            filter_btn.connect_clicked(move |_| toggle_for_button());
            toggle
        };

        FinishedBlock {
            id,
            is_background,
            widget: outer,
            content,
            virtualized_height: Rc::new(Cell::new(estimated_finished_block_height(
                config,
                estimated_visible_rows,
            ))),
            virtualized: Rc::new(Cell::new(false)),
            compact: Rc::new(Cell::new(config.block_compact)),
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
            selection_hint,
            selection_hint_steady: Rc::new(RefCell::new(String::new())),
            selection_feedback_generation: Rc::new(Cell::new(0)),
            toggle_collapsed,
            collapsed_state,
            toggle_filter,
            jump_bottom_btn,
            bookmark_star,
            lifecycle_chip,
            status_icon,
            cols,
            viewport_cap,
            dynamic_viewport_rows,
            render_stamp,
            visual_rows_cache,
            displayed_generation,
            command_bytes: cmd_bytes,
            command_render_cols,
            command_base_rows: cmd_rows,
            capture_rows,
            max_expanded_cap,
            output_rows,
            expanded,
            expand_btn,
            output_scrollable,
            long_output,
            estimated_retained_bytes,
        }
    }

    pub(crate) const fn estimated_retained_bytes(&self) -> usize {
        self.estimated_retained_bytes
    }

    pub(crate) fn widget(&self) -> &gtk::Box {
        &self.widget
    }

    /// Scroll this block's command edge or output edge into the outer history
    /// canvas. The child VTEs never own this navigation.
    pub(crate) fn scroll_to_edge(&self, outer: &gtk::ScrolledWindow, bottom: bool) {
        Self::scroll_finished_widget_to_edge(&self.widget, outer, bottom);
    }

    fn scroll_finished_widget_to_edge(
        widget: &gtk::Box,
        outer: &gtk::ScrolledWindow,
        bottom: bool,
    ) {
        let widget = widget.clone();
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

    /// Re-render a mapped snapshot when pane height, width, or filter text
    /// changes. Width is part of the stamp because VTE follows its allocation
    /// below the recorded columns; ignoring it causes the narrow-pane two-frame
    /// height oscillation this method is designed to prevent.
    /// Park this card as a fixed-height placeholder, or restore it.
    ///
    /// Virtualizing hides the card's *contents* while the card itself stays in
    /// the document at the height it last really occupied. Hiding the card
    /// outright instead — which is what this used to do — drops its height to
    /// zero, so the history's scroll `upper` moves every time a block crosses
    /// the viewport edge: the page shifts under the reader, the follow-bottom
    /// pin chases it, and blocks flip in and out. Returns the height the card
    /// now claims, so the caller can keep its virtualization metadata in step.
    pub(crate) fn set_virtualized(&self, virtualized: bool) -> i32 {
        self.set_virtualized_with_measurement(virtualized, true)
    }

    /// Density switches have already translated the saved height before GTK
    /// allocates the new margins. Immediate visibility reconciliation must not
    /// sample the still-old allocation back over that new model.
    pub(crate) fn set_virtualized_preserving_height(&self, virtualized: bool) -> i32 {
        self.set_virtualized_with_measurement(virtualized, false)
    }

    fn set_virtualized_with_measurement(&self, virtualized: bool, measure_allocation: bool) -> i32 {
        if self.virtualized.replace(virtualized) == virtualized {
            return self.virtualized_height.get().max(1);
        }
        if virtualized {
            let allocated = self.widget.height();
            if measure_allocation && allocated > 1 {
                self.virtualized_height.set(allocated);
            }
            let height = self.virtualized_height.get().max(1);
            self.widget.set_height_request(height);
            self.content.set_visible(false);
            height
        } else {
            self.content.set_visible(true);
            self.widget.set_height_request(-1);
            self.virtualized_height.get().max(1)
        }
    }

    /// Whether this card's output is currently folded away.
    pub(crate) fn is_output_collapsed(&self) -> bool {
        self.collapsed_state.get()
    }

    /// Fold or unfold this card's output, reporting whether anything moved.
    ///
    /// The pane uses the answer to skip the layout pass entirely when a bulk
    /// collapse found nothing to do. A card with neither output nor images
    /// cannot fold, so the toggle no-ops there and the report stays honest.
    pub(crate) fn set_collapsed(&self, collapsed: bool) -> bool {
        if self.is_output_collapsed() == collapsed {
            return false;
        }
        (self.toggle_collapsed)();
        self.is_output_collapsed() == collapsed
    }

    /// Mark, or unmark, this card as carrying a completion nobody vouched for.
    ///
    /// Takes the notice as well as the health because the two backing record
    /// types word it from different evidence; the chip's own word comes from
    /// the vocabulary Unified's status line uses, so one record cannot be
    /// described two ways depending on which mode is showing it.
    pub(crate) fn set_lifecycle(&self, health: BlockLifecycleHealth, notice: Option<&str>) {
        // A background block has no command to have completed, and its header
        // already says what it is.
        let badge = super::unified_chrome::lifecycle_badge(health).filter(|_| !self.is_background);
        match badge {
            Some(badge) => {
                self.lifecycle_chip.set_text(badge);
                self.lifecycle_chip.set_tooltip_text(notice);
                self.lifecycle_chip
                    .update_property(&[gtk::accessible::Property::Label(notice.unwrap_or(badge))]);
                self.lifecycle_chip.set_visible(true);
            }
            None => {
                self.lifecycle_chip.set_visible(false);
                self.lifecycle_chip.set_tooltip_text(None);
            }
        }
    }

    /// Switch this card between the normal and compact densities in place and
    /// return the height its virtualization placeholder must contribute.
    pub(crate) fn set_compact(&self, compact: bool) -> i32 {
        let previous = self.compact.replace(compact);
        if previous != compact {
            let delta = finished_card_vchrome_px(compact)
                .saturating_sub(finished_card_vchrome_px(previous));
            let height = self.virtualized_height.get().saturating_add(delta).max(1);
            self.virtualized_height.set(height);
            if self.virtualized.get() {
                self.widget.set_height_request(height);
            }
        }
        apply_card_density(&self.widget, compact);
        apply_header_density(&self.header_row, compact);
        self.virtualized_height.get().max(1)
    }

    /// What this card's output VTE currently holds.
    ///
    /// A find pass records this value per surface. If it changes before the
    /// pass navigates, a reset/re-window has invalidated the native cursor that
    /// pass was stepping from, so the caller must rebuild the search.
    pub(crate) fn render_stamp(&self) -> RenderStamp {
        self.render_stamp.get()
    }

    /// Re-fit this block's output to the pane's current geometry.
    ///
    /// The cap comes from the pane the block hangs in, not from a value the
    /// caller computed against the live input cell — see
    /// [`fitted_output_rows_for_viewport`]. Virtualized (unmapped) cards are
    /// left alone; their own `connect_map` handler fits them on the way back in.
    pub(crate) fn refit_output_to_viewport(&self) -> Option<i32> {
        // This method is called for every retained block on a pane resize.
        // Reject parked cards before borrowing their output or counting visual
        // rows: `connect_map` recomputes the cap when a card becomes visible,
        // while scanning an off-screen transcript cannot affect this frame.
        if !self.output_vte.is_mapped() {
            return None;
        }
        let cell_height = (self.output_vte.char_height() as i32).max(1);
        let effective_cols = effective_render_cols(&self.output_vte, self.cols);
        let full = self.full_output.borrow();
        let displayed = self.displayed_output.borrow();
        let text = displayed.as_deref().unwrap_or(full.as_str());
        let output_rows = cached_output_visual_row_count(
            &self.visual_rows_cache,
            text,
            effective_cols,
            self.displayed_generation.get(),
        );
        let fitted_rows = bounded_finished_viewport_rows(
            effective_cols,
            fitted_output_rows_for_widget(&self.output_vte, self.viewport_cap, output_rows),
        );
        let generation = self.displayed_generation.get();
        let (last_cols, ..) = self.render_stamp.get();
        let geometry_changed =
            last_cols != effective_cols || self.dynamic_viewport_rows.get() != fitted_rows;
        let command_effective_cols = effective_render_cols(&self.command_vte, self.cols);
        let command_needs_refit = self.command_vte.is_mapped()
            && self.command_render_cols.get() != command_effective_cols;
        if !geometry_changed && !command_needs_refit {
            return None;
        }

        self.dynamic_viewport_rows.set(fitted_rows);
        // Pane sizing is authoritative over a manual expansion: a block expanded
        // for the old geometry must not outlive it.
        if self.expanded.replace(false) {
            set_icon_button(&self.expand_btn, "view-fullscreen-symbolic", "Expand block");
        }
        self.expand_btn.set_visible(output_rows > fitted_rows);
        let stamp = output_render_stamp(effective_cols, output_rows, fitted_rows, generation);
        let visible_rows = snapshot_visible_rows(output_rows, fitted_rows);
        // Only re-feed when this pass would actually draw something different.
        // A cap that changed without changing the rows on screen (the layout's
        // first fitted pass over a short block always does this) leaves the
        // snapshot as it is: re-feeding it in the same main-loop iteration as the
        // map-time render is what made block output appear twice.
        let previous_stamp = self.render_stamp.replace(stamp);
        if previous_stamp != stamp {
            // Same columns and same displayed generation means VTE already
            // holds exactly these bytes wrapped exactly this way; only the
            // number of rows on screen moved. Re-window it instead of resetting
            // and re-parsing the transcript.
            if !stamp_change_needs_refeed(previous_stamp, stamp) {
                rewindow_finished_vte(
                    &self.output_vte,
                    effective_cols,
                    visible_rows,
                    self.capture_rows.max(output_rows),
                    (output_rows <= fitted_rows).then_some(fitted_rows),
                );
            } else {
                render_bytes_into_finished_vte(
                    &self.output_vte,
                    text,
                    effective_cols,
                    output_rows,
                    fitted_rows,
                    self.capture_rows,
                    output_rows <= fitted_rows,
                );
            }
        }
        self.output_vte
            .set_height_request(finished_vte_height_px(visible_rows, cell_height));
        self.output_scrollbar.set_visible(output_rows > fitted_rows);
        self.output_vte.queue_allocate();

        let command_text = String::from_utf8_lossy(self.command_bytes.as_slice());
        let unbounded_command_rows = if self.is_background {
            0
        } else {
            output_visual_row_count(&command_text, command_effective_cols)
                .max(self.command_base_rows)
        };
        let (bounded_command_cols, bounded_command_rows, command_scrollback) =
            bounded_finished_vte_geometry(command_effective_cols, unbounded_command_rows.max(1), 0);
        if command_needs_refit {
            self.command_render_cols.set(bounded_command_cols);
            self.command_vte
                .set_size(bounded_command_cols, bounded_command_rows);
            self.command_vte.set_scrollback_lines(command_scrollback);
            self.command_vte.reset(true, true);
            self.command_vte
                .set_size(bounded_command_cols, bounded_command_rows);
            self.command_vte.set_scrollback_lines(command_scrollback);
            feed_snapshot_bytes(&self.command_vte, self.command_bytes.as_slice());
            settle_finished_terminal_after_feed(
                &self.command_vte,
                bounded_command_rows,
                bounded_command_rows,
            );
            self.command_vte
                .set_height_request(finished_vte_height_px(bounded_command_rows, cell_height));
        }

        let command_height_rows = if self.is_background {
            0
        } else {
            bounded_command_rows
        };
        let rows_for_height = visible_rows
            .saturating_add(2)
            .saturating_add(command_height_rows.saturating_sub(1));
        Some(
            (rows_for_height.clamp(1, i32::MAX as i64) as i32)
                .saturating_mul(cell_height)
                .saturating_add(34),
        )
    }

    /// Give a long block first refusal on wheel events while the pointer is over
    /// either its output or its scrollbar. Only hand off to the outer history
    /// canvas after the inner adjustment reaches the requested boundary.
    /// `debouncer` records the user's scroll intent for wheel motion this card
    /// hands on to the history — see [`ScrollDebouncer::record_wheel_intent`].
    pub(crate) fn connect_scroll_forwarding(
        &self,
        outer: &gtk::ScrolledWindow,
        debouncer: &ScrollDebouncer,
    ) {
        // The button belongs to this FinishedBlock. Capturing `self.clone()`
        // here forms button -> signal closure -> FinishedBlock -> button/VTE;
        // weak widget handles keep eviction and pane teardown reclaimable.
        let widget_for_jump = self.widget.downgrade();
        let outer_for_jump = outer.downgrade();
        self.jump_bottom_btn.connect_clicked(move |_| {
            let (Some(widget), Some(outer)) = (widget_for_jump.upgrade(), outer_for_jump.upgrade())
            else {
                return;
            };
            FinishedBlock::scroll_finished_widget_to_edge(&widget, &outer, true);
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
            let debouncer = debouncer.clone();
            scroll_ctrl.connect_scroll(move |_, _dx, dy| {
                let (Some(vte), Some(outer_for_vte)) = (vte.upgrade(), outer_for_vte.upgrade())
                else {
                    return glib::Propagation::Proceed;
                };
                if let Some(inner_adj) = vte.vadjustment() {
                    if scroll_adjustment_by_wheel(&inner_adj, dy) {
                        return glib::Propagation::Stop;
                    }
                }
                forward_outer_scroll(&outer_for_vte, dy);
                debouncer.record_wheel_intent(&outer_for_vte);
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
        verified_submission: &super::VerifiedSubmissionCtx,
        bracketed_paste: &Rc<Cell<bool>>,
        active: &Rc<RefCell<ActiveBlock>>,
    ) {
        let vte_for_cmd = vte.clone();
        let cmd_for_copy = self.cmd_text.clone();
        self.copy_cmd_btn.connect_clicked(move |btn| {
            vte_for_cmd.clipboard().set_text(&cmd_for_copy);
            flash_button_icon(btn, "emblem-ok-symbolic", "Command copied");
        });

        let vte_for_out = vte.clone();
        // Copies what the card is showing: the whole transcript normally, and
        // the filtered lines while a filter is on. Copying the full transcript
        // out of a filtered card is the one thing nobody asks for — filtering
        // is how you decide what to copy — and it arrives with no sign that the
        // clipboard holds more than the screen does.
        //
        // `try_borrow`: the filter's own apply closure holds this mutably while
        // it re-renders, on the same main loop. Falling back to the full
        // transcript there copies a superset, never a wrong block.
        let full_output_for_copy = self.full_output.clone();
        let displayed_output_for_copy = self.displayed_output.clone();
        self.copy_output_btn.connect_clicked(move |btn| {
            let displayed = displayed_output_for_copy.try_borrow().ok();
            let filtered = displayed
                .as_ref()
                .and_then(|displayed| displayed.as_deref())
                .map(strip_ansi);
            let (text, label) = match filtered {
                Some(text) => (text, "Filtered output copied"),
                None => (strip_ansi(&full_output_for_copy.borrow()), "Output copied"),
            };
            vte_for_out.clipboard().set_text(&text);
            flash_button_icon(btn, "emblem-ok-symbolic", label);
        });

        let verified_submission_for_rerun = verified_submission.clone();
        let bracketed_paste_for_rerun = bracketed_paste.clone();
        let active_for_rerun = active.clone();
        let cmd_for_rerun = self.cmd_text.clone();
        self.rerun_btn.connect_clicked(move |btn| {
            let recall_is_lossless = super::selected_command_recall_is_lossless(
                &cmd_for_rerun,
                bracketed_paste_for_rerun.get(),
            );
            if verified_submission_for_rerun
                .try_recall_command(&cmd_for_rerun, bracketed_paste_for_rerun.get())
            {
                active_for_rerun.borrow().grab_focus();
                flash_button_icon(btn, "emblem-ok-symbolic", "Command inserted");
            } else if !recall_is_lossless {
                flash_button_icon(
                    btn,
                    "dialog-warning-symbolic",
                    "Bracketed paste required for multiline command",
                );
            } else {
                flash_button_icon(
                    btn,
                    "dialog-warning-symbolic",
                    "Wait for an editable prompt",
                );
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
    /// The measured wrapper around the live VTE. It carries the terminal's
    /// requested pixel size — see [`ActiveBlock::set_live_geometry`], which
    /// keeps the grid at the full viewport while the card shows less.
    vte_overlay: gtk::Overlay,
    /// Sole measured child of the clip: its height IS the live card's height.
    live_spacer: gtk::Box,
    /// The clip itself; its allocated width is the space the terminal may use.
    live_clip: gtk::Overlay,
    /// Last applied `(width_px, grid_px, visible_px)`, so a layout pass that
    /// changes nothing does not queue a resize.
    live_geometry: Cell<(i32, i32, i32)>,
    /// High-water row extent of the command in flight. Monotone within one
    /// command so a `\r` progress bar, an `ESC[1A` redraw or a mid-command
    /// `clear` can never make the card shrink under the output already on
    /// screen. `reset_active` — the single funnel every reset path uses —
    /// clears it for the next command.
    live_extent_rows: Rc<Cell<i64>>,
    /// Cursor row this command's output started from. Paired with
    /// `live_extent_rows` so both are re-based by the same reset funnel.
    /// Lowest ring row the prompt drew on, and the highest the cursor has
    /// reached since. Both are `cursor_position()` readings, so they are in
    /// one coordinate system by construction — which the live adjustment is
    /// not, and neither is a literal zero: `vte.reset()` does not rewind
    /// VTE's absolute row counter (`Ring::reset` returns `m_end` unchanged),
    /// so rows keep climbing for the life of the pane.
    live_cursor_origin: Rc<Cell<Option<i64>>>,
    live_cursor_high: Rc<Cell<i64>>,
    /// `preserve_live_scrollback` as it stands now (`reload_config` writes it).
    /// It decides where the prompt lives inside the live grid, which is what
    /// the compact-card layout has to know: with the default reset the prompt
    /// starts at the top of a freshly cleared screen, so the card can show the
    /// grid's first rows and grow into whatever the shell drew below the input.
    /// When the previous command's output is deliberately kept, the prompt is
    /// at the *bottom* of the ring instead, so the grid stays pinned to the
    /// card exactly as it was before.
    preserve_live_scrollback: Cell<bool>,
    /// Pass-through, non-measuring surface for small live widgets. The VTE
    /// remains the overlay's measured child, so the organism never changes
    /// the terminal grid or steals input.
    pub(crate) live_organism_surface: gtk::Fixed,
    /// Probe-addressed Kitty image layer used by Unified mode. Added before
    /// the organism surface so inline assistant UI always remains readable.
    /// Hidden in Block mode, whose images move into finished cards instead.
    pub(crate) unified_image_surface: gtk::Fixed,
    /// Pass-through, non-measuring chrome overlay used only by Unified mode.
    /// It exists (hidden) in Block mode so the widget tree stays mode-neutral.
    pub(crate) unified_chrome_surface: gtk::DrawingArea,
    /// Overlay scrollbar for a still-running command. It is painted above the
    /// organism surface and therefore remains reachable at every pane width.
    pub(crate) live_scrollbar: gtk::Scrollbar,
    /// Feature-requested visibility. Alternate-screen applications suppress
    /// it without allowing stale pre-TUI coordinates to reappear on exit.
    live_organism_visible: Cell<bool>,
    live_organism_alt_screen: Cell<bool>,
    /// Raw output bytes accumulated during CollectingOutput (anvil's
    /// `out_buf`). Engine-owned shared state constructed in `TermView::new`:
    /// the reader engine appends, clears, and snapshots it directly; this
    /// clone exists only so live-find can read it ([`Self::output_text`]).
    pub(crate) raw_output: Rc<RefCell<VecDeque<u8>>>,
}

/// Retain only the newest `limit` bytes without repeatedly shifting the whole
/// retained buffer. A `VecDeque` makes long-running output proportional to the
/// new bytes discarded instead of copying the full multi-megabyte tail for
/// every PTY chunk.
///
/// Returns whether any byte of the stream failed to survive this append. The
/// retained tail cannot reveal that on its own, so a consumer that snapshots
/// the tail must accumulate this to report the stream truthfully.
#[must_use]
pub(super) fn append_bounded_output(buffer: &mut VecDeque<u8>, bytes: &[u8], limit: usize) -> bool {
    if limit == 0 {
        let dropped = !buffer.is_empty() || !bytes.is_empty();
        buffer.clear();
        return dropped;
    }
    if bytes.len() >= limit {
        let dropped = !buffer.is_empty() || bytes.len() > limit;
        buffer.clear();
        buffer.extend(bytes[bytes.len() - limit..].iter().copied());
        return dropped;
    }
    let overflow = buffer
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(limit);
    if overflow > 0 {
        buffer.drain(..overflow);
    }
    buffer.extend(bytes.iter().copied());
    overflow > 0
}

impl ActiveBlock {
    /// `pub(super)`: only `TermView::new` constructs the live block, and it is
    /// the owner of the engine-side `raw_output` ring passed in here.
    pub(super) fn new(config: &Config, raw_output: Rc<RefCell<VecDeque<u8>>>) -> Self {
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
        let vte_overlay = gtk::Overlay::new();
        vte_overlay.set_hexpand(true);
        vte_overlay.set_vexpand(false);
        vte_overlay.set_child(Some(&active_vte));

        let live_organism_surface = gtk::Fixed::new();
        live_organism_surface.set_hexpand(true);
        live_organism_surface.set_vexpand(true);
        live_organism_surface.set_halign(gtk::Align::Fill);
        live_organism_surface.set_valign(gtk::Align::Fill);
        live_organism_surface.set_overflow(gtk::Overflow::Hidden);
        live_organism_surface.set_can_target(false);
        live_organism_surface.set_focusable(false);
        live_organism_surface.set_visible(false);

        let unified_image_surface = gtk::Fixed::new();
        unified_image_surface.set_hexpand(true);
        unified_image_surface.set_vexpand(true);
        unified_image_surface.set_halign(gtk::Align::Fill);
        unified_image_surface.set_valign(gtk::Align::Fill);
        unified_image_surface.set_overflow(gtk::Overflow::Hidden);
        unified_image_surface.set_can_target(false);
        unified_image_surface.set_focusable(false);
        unified_image_surface.set_visible(false);
        // Paint above terminal cells but below the organism and chrome. The
        // insertion order is load-bearing: Unified's organism has no other
        // body surface and must never be silently covered by a large image.
        vte_overlay.add_overlay(&unified_image_surface);
        vte_overlay.set_measure_overlay(&unified_image_surface, false);
        vte_overlay.set_clip_overlay(&unified_image_surface, true);
        vte_overlay.add_overlay(&live_organism_surface);
        vte_overlay.set_measure_overlay(&live_organism_surface, false);
        vte_overlay.set_clip_overlay(&live_organism_surface, true);

        let unified_chrome_surface = gtk::DrawingArea::new();
        unified_chrome_surface.set_hexpand(true);
        unified_chrome_surface.set_vexpand(true);
        unified_chrome_surface.set_halign(gtk::Align::Fill);
        unified_chrome_surface.set_valign(gtk::Align::Fill);
        unified_chrome_surface.set_can_target(false);
        unified_chrome_surface.set_focusable(false);
        unified_chrome_surface.set_visible(false);
        vte_overlay.add_overlay(&unified_chrome_surface);
        vte_overlay.set_measure_overlay(&unified_chrome_surface, false);
        vte_overlay.set_clip_overlay(&unified_chrome_surface, true);

        let live_scrollbar =
            gtk::Scrollbar::new(Orientation::Vertical, active_vte.vadjustment().as_ref());
        live_scrollbar.add_css_class("block-output-scrollbar");
        live_scrollbar.set_tooltip_text(Some("Scroll within the running output"));
        live_scrollbar.set_halign(gtk::Align::End);
        live_scrollbar.set_visible(false);
        // Added last so it paints above the pass-through organism surface.

        // ── Live card clip ────────────────────────────────────────────────
        // The card is only as tall as the running command's output so far, but
        // the terminal underneath keeps the FULL viewport grid: that is the
        // winsize the child was told about (`pty_grid_size`), and anything that
        // addresses rows absolutely — `top`, `watch`, any repaint that clears
        // the screen without switching to the alternate one — would otherwise
        // be drawing into a grid too short to hold it.
        //
        // GTK derives the grid from the VTE's *allocation*: `set_size` cannot
        // hold a taller grid than the space the parent hands out (measured — an
        // explicit `set_size(200, 50)` reverted on the next reallocation), and
        // neither a ScrolledWindow/Viewport nor a plain non-FILL overlay child
        // keeps them apart (both squeeze the terminal to the visible height).
        // `gtk::Fixed` does: it allocates each child the size the child asked
        // for, whatever height the Fixed itself has. Riding it as a non-measured
        // overlay above a spacer means the card measures the spacer alone while
        // the terminal keeps every row, and `Overflow::Hidden` clips the rows
        // below the card — for input as well as for paint. Both dimensions of
        // the child's size request are required: inside a Fixed a `-1` collapses
        // the child to its minimum (the same recipe the organism surface uses).
        let live_spacer = gtk::Box::new(Orientation::Vertical, 0);
        live_spacer.set_hexpand(true);
        live_spacer.set_vexpand(false);
        let live_surface = gtk::Fixed::new();
        live_surface.set_overflow(gtk::Overflow::Hidden);
        live_surface.set_halign(gtk::Align::Fill);
        live_surface.set_valign(gtk::Align::Fill);
        live_surface.put(&vte_overlay, 0.0, 0.0);
        let live_clip = gtk::Overlay::new();
        live_clip.set_hexpand(true);
        live_clip.set_vexpand(false);
        live_clip.set_overflow(gtk::Overflow::Hidden);
        live_clip.set_child(Some(&live_spacer));
        live_clip.add_overlay(&live_surface);
        live_clip.set_measure_overlay(&live_surface, false);
        live_clip.set_clip_overlay(&live_surface, true);
        // The scrollbar rides the CLIP, not the terminal: `vte_overlay` is now
        // allocated the whole grid, so a scrollbar inside it would be sized
        // against rows the card is not showing and cut off halfway.
        live_clip.add_overlay(&live_scrollbar);
        widget.append(&live_clip);
        if let Some(adjustment) = active_vte.vadjustment() {
            let scrollbar = live_scrollbar.downgrade();
            let sync_visibility = move |adjustment: &gtk::Adjustment| {
                let Some(scrollbar) = scrollbar.upgrade() else {
                    return;
                };
                let overflows =
                    adjustment.upper() - adjustment.lower() > adjustment.page_size() + f64::EPSILON;
                scrollbar.set_visible(overflows);
            };
            sync_visibility(&adjustment);
            adjustment.connect_changed(sync_visibility);
        }

        ActiveBlock {
            widget,
            active_vte,
            vte_overlay,
            live_spacer,
            live_clip,
            live_geometry: Cell::new((0, 0, 0)),
            live_extent_rows: Rc::new(Cell::new(0)),
            live_cursor_origin: Rc::new(Cell::new(None)),
            live_cursor_high: Rc::new(Cell::new(0)),
            preserve_live_scrollback: Cell::new(config.preserve_live_scrollback),
            live_organism_surface,
            unified_image_surface,
            unified_chrome_surface,
            live_scrollbar,
            live_organism_visible: Cell::new(false),
            live_organism_alt_screen: Cell::new(false),
            raw_output,
        }
    }

    /// Snapshot the engine-owned capture for live-find. The engine reads the
    /// same ring through `super::live_output_text` at finalize.
    pub(crate) fn output_text(&self) -> String {
        super::live_output_text(&self.raw_output)
    }

    /// Return at most `max_bytes` from the live capture for bounded find.
    /// Lossy conversion remains safe when the byte ceiling splits UTF-8.
    pub(crate) fn output_text_prefix(&self, max_bytes: usize) -> (String, bool) {
        let mut raw = self.raw_output.borrow_mut();
        if raw.is_empty() {
            return (String::new(), false);
        }
        let bytes = raw.make_contiguous();
        let end = bytes.len().min(max_bytes);
        (
            String::from_utf8_lossy(&bytes[..end]).into_owned(),
            end < bytes.len(),
        )
    }

    /// The column count the live VTE is wrapping at — the single source of truth
    /// for pre-wrapping finished blocks so they align with what the user watched.
    pub(crate) fn grid_cols(&self) -> usize {
        (self.active_vte.column_count().max(20)) as usize
    }

    /// Give the live terminal a `grid_px`-tall grid and show `visible_px` of it.
    ///
    /// The two are equal everywhere except while a command is running, where the
    /// card grows with the output and the grid stays a full viewport (see the
    /// clip construction in [`ActiveBlock::new`]). Returns whether anything
    /// changed, so callers can skip follow-up work on a no-op layout pass.
    ///
    /// The width comes from the clip's own allocation — inside a `gtk::Fixed`
    /// the terminal is allocated exactly what it requests, so it cannot pick up
    /// the pane width by expanding. Before the first allocation there is no
    /// width to hand out and the request is left alone; the next layout pass
    /// (contents, adjustment or resize settle tick) applies it.
    pub(crate) fn set_live_geometry(&self, cell_h: i32, grid_rows: i64, visible_rows: i64) -> bool {
        let cell_h = cell_h.max(1);
        let grid_rows = grid_rows.max(1);
        let visible_rows = visible_rows.clamp(1, grid_rows);
        let width_px = self.live_clip.width();
        if width_px <= 0 {
            // Before the first allocation there is no width to hand out, but
            // the card height does not depend on one and the caller has already
            // moved the holder's request: leave the two in step.
            self.live_spacer
                .set_height_request((visible_rows as i32).saturating_mul(cell_h));
            return false;
        }
        // Ask for a sliver more than the grid needs. The terminal takes its row
        // count from the allocation, and a container that hands back a pixel or
        // two less than requested would cost a whole row; anything under one
        // cell cannot add one.
        let grid_px = (grid_rows as i32).saturating_mul(cell_h) + cell_h - 1;
        let visible_px = (visible_rows as i32).saturating_mul(cell_h);
        let geometry = (width_px, grid_px, visible_px);
        if self.live_geometry.get() == geometry {
            return false;
        }
        self.live_geometry.set(geometry);
        self.vte_overlay.set_size_request(width_px, grid_px);
        self.live_spacer.set_height_request(visible_px);
        true
    }

    /// The measured live card. The pane resize watcher keys off its width:
    /// the terminal is sized by an explicit request now and cannot follow the
    /// pane on its own.
    pub(crate) fn live_clip(&self) -> gtk::Overlay {
        self.live_clip.clone()
    }

    /// Whether the live surface keeps the previous command's scrollback at the
    /// prompt. Read by `block_layout_active_surface`; written by
    /// `TermView::reload_config` so a runtime config change is not stale here.
    pub(crate) fn preserve_live_scrollback(&self) -> bool {
        self.preserve_live_scrollback.get()
    }

    pub(crate) fn set_preserve_live_scrollback(&self, preserve: bool) {
        self.preserve_live_scrollback.set(preserve);
    }

    /// Shared high-water extent, cloned into `block_layout_active_surface`.
    pub(crate) fn live_extent_rows(&self) -> Rc<Cell<i64>> {
        self.live_extent_rows.clone()
    }

    /// Shared measurement origin, cloned into `block_layout_active_surface`.
    pub(crate) fn live_cursor_origin(&self) -> Rc<Cell<Option<i64>>> {
        self.live_cursor_origin.clone()
    }

    pub(crate) fn live_cursor_high(&self) -> Rc<Cell<i64>> {
        self.live_cursor_high.clone()
    }

    /// Height of the live card in pixels — the part of the grid the user can
    /// see. Live widgets positioned over the terminal (the organism) must stay
    /// inside it or they are clipped away.
    pub(crate) fn live_visible_height_px(&self) -> i32 {
        let (_, _, visible) = self.live_geometry.get();
        if visible > 0 {
            visible
        } else {
            self.active_vte.height().max(0)
        }
    }

    /// Reset the live VTE for the next prompt (anvil block.rs:1028-1044). `reset`
    /// acts immediately, but already-queued feed() bytes are processed async, so the
    /// in-stream clear (fed after them) wipes stale output in the correct order.
    ///
    /// `preserve_scrollback`: when true, keep the VTE's buffer + scrollback intact
    /// (SGR state is soft-reset). This mirrors a traditional VTE where PageUp
    /// at a prompt reveals the previous command's output tail. The default (false)
    /// wipes the live VTE on every PromptStart, since the finished blocks above
    /// already hold the authoritative scrollback.
    ///
    /// Deliberately does NOT touch the `raw_output` ring: that is engine-owned
    /// state, cleared explicitly by the reader engine around this reset (see
    /// `RenderBackend::reset_active_surface`).
    pub(crate) fn reset_active(&self, preserve_scrollback: bool) {
        // A new command starts a new card: forget how far the last one grew,
        // and the row its predecessor grew from.
        self.live_extent_rows.set(0);
        // Forget where the last card began. The next one re-latches its origin
        // from the prompt's own cursor samples; nothing here can name a row in
        // that coordinate system yet, because the bytes below are applied
        // asynchronously.
        self.live_cursor_origin.set(None);
        self.live_cursor_high.set(0);
        if preserve_scrollback {
            self.active_vte.feed(b"\x1b[0m");
        } else {
            self.active_vte.reset(true, true);
            self.active_vte.feed(b"\x1b[H\x1b[2J\x1b[3J");
        }
    }

    pub(crate) fn widget(&self) -> &gtk::Box {
        &self.widget
    }

    /// Switch the live input cell's density. Unified's holder carries
    /// `block-fullscreen` instead and is left alone by its caller.
    pub(crate) fn set_compact(&self, compact: bool) {
        if compact {
            self.widget.add_css_class("block-compact");
        } else {
            self.widget.remove_css_class("block-compact");
        }
    }

    pub(crate) fn grab_focus(&self) {
        self.active_vte.grab_focus();
    }

    pub(crate) fn set_live_organism_visible(&self, visible: bool) {
        self.live_organism_visible.set(visible);
        self.sync_live_organism_visibility();
    }

    pub(crate) fn set_live_organism_alt_screen(&self, alt_screen: bool) {
        let (desired, alt_screen) =
            live_organism_alt_transition(self.live_organism_visible.get(), alt_screen);
        self.live_organism_visible.set(desired);
        self.live_organism_alt_screen.set(alt_screen);
        self.sync_live_organism_visibility();
    }

    pub(crate) fn live_organism_alt_screen(&self) -> bool {
        self.live_organism_alt_screen.get()
    }

    fn sync_live_organism_visibility(&self) {
        self.live_organism_surface
            .set_visible(live_organism_is_visible(
                self.live_organism_visible.get(),
                self.live_organism_alt_screen.get(),
            ));
    }
}

fn live_organism_alt_transition(desired: bool, entering: bool) -> (bool, bool) {
    if entering {
        // Exit only removes the override; a later measured heartbeat opts in
        // again instead of resurrecting stale pre-TUI coordinates.
        (false, true)
    } else {
        (desired, false)
    }
}

fn live_organism_is_visible(desired: bool, alt_screen: bool) -> bool {
    desired && !alt_screen
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
