//! Bounded find/search state and navigation for Block-mode terminal history.
//!
//! Find-within-blocks: VTE's native PCRE2 highlighter paints every hit inside
//! each finished block's command/output VTE; we only track which (block, surface)
//! each hit belongs to so Next/Prev can step the per-VTE search cursor across
//! block boundaries. Also hosts the metadata-only filter pass used by the
//! command palette's failed/slow toggles and by the debug dashboard counts.

use gtk::glib;
use gtk::prelude::*;
use relm4::gtk;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use vte4::TerminalExt;

use super::{
    contains_case_insensitive, replace_finished_block_selection, BackendRecordRef, BlockFilters,
    TermView, MAX_ZONE_SNAPSHOT_BYTES,
};

fn outcome_matches_filters(
    resolved_command: &str,
    raw_exit_code: Option<i32>,
    filters: &BlockFilters,
) -> bool {
    let outcome =
        jterm_core::block_contract::classify_completed(Some(resolved_command), raw_exit_code);
    if filters
        .exit_code
        .is_some_and(|exit_code| outcome.reported_exit_code() != Some(exit_code))
    {
        return false;
    }
    !filters.failed_only
        || matches!(
            outcome,
            jterm_core::block_contract::CompletedBlockOutcome::Failed(_)
        )
}

/// Stop common queries from turning a bounded output history into unbounded
/// match metadata or a long-running main-thread scan. Reaching the limit is
/// deliberately reported as capped even when the retained history happens to
/// contain exactly this many hits: proving equality would require scanning the
/// remainder, defeating the early-stop guarantee.
pub(crate) const FIND_MATCH_LIMIT: usize = 10_000;
const FIND_SCAN_BYTE_LIMIT: usize = 4 * 1024 * 1024;
const FIND_SCAN_TIME_LIMIT: Duration = Duration::from_millis(12);
const CROSS_BLOCK_REGEX_SIZE_LIMIT: usize = 2 * 1024 * 1024;
/// VTE uses PCRE2 while match counting uses Rust's Unicode-aware regex engine.
/// UTF validates/decodes the subject as Unicode and UCP makes shorthand classes
/// such as `\d`, `\s`, and `\w` use Unicode properties on the VTE side too.
const VTE_SEARCH_FLAGS: u32 = pcre2_sys::PCRE2_CASELESS
    | pcre2_sys::PCRE2_MULTILINE
    | pcre2_sys::PCRE2_UTF
    | pcre2_sys::PCRE2_UCP;

/// One searchable VTE surface. VTE owns the exact match positions and paints
/// every occurrence; Forge needs only the number of occurrences on each
/// surface to navigate across block boundaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FindSurface {
    pub(crate) block_id: u64,
    pub(crate) block_index: usize,
    /// false = command VTE, true = output VTE.
    pub(crate) is_output: bool,
    /// Hit is painted on the backend's live VTE. In Block this means the
    /// still-running command (`block_id == 0`); in Unified completed records
    /// also map here because its whole history is one persistent surface.
    pub(crate) is_live: bool,
    /// Number of occurrences retained for navigation on this surface. Always
    /// positive and bounded by [`FIND_MATCH_LIMIT`].
    pub(crate) count: usize,
    /// Native VTE cursor position last confirmed by a successful search call.
    vte_cursor: Option<usize>,
    /// False when the match or scan budget stopped inside this surface.
    complete: bool,
    /// The first native step must wrap from a deliberately reset viewport
    /// cursor into a selected oldest-history window.
    initial_wrap: bool,
    /// Occurrence whose entry crosses VTE's one physical wrap boundary. For a
    /// viewport-first Unified domain this is the first counted history hit.
    wrap_before: Option<usize>,
    /// What this surface held at scan time. A card re-feed resets VTE's native
    /// selection and a re-window moves its rows, so the recorded cursor cannot
    /// be used after this stamp changes. Live/Unified surfaces use the neutral
    /// stamp because they are not independently re-rendered per record.
    render_stamp: super::blocks::RenderStamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FindHighlight {
    block_id: u64,
    block_index: usize,
    is_output: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FindCursor {
    surface: usize,
    occurrence: usize,
    /// Zero-based position across the compressed surface list.
    global: usize,
}

#[derive(Default)]
pub(crate) struct FindState {
    pub(crate) surfaces: Vec<FindSurface>,
    cursor: FindCursor,
    total: usize,
    capped: bool,
    scan_limited: bool,
    /// Exact terminals with regexes installed by the current pass. Several
    /// logical Unified records can map to the same persistent VTE, so cleanup
    /// must deduplicate these handles rather than reconstructing them from the
    /// Block-only finished-widget list.
    highlighted_terminals: Vec<vte4::Terminal>,
    /// Highlights installed by the flat cross-block palette do not participate
    /// in incremental navigation, but must still be cleared without walking all
    /// retained blocks on every debounced query.
    extra_highlights: Vec<FindHighlight>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FindProgress {
    pub(crate) current: usize,
    pub(crate) total: usize,
    pub(crate) capped: bool,
    pub(crate) scan_limited: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FindSearchResult {
    NoMatches,
    InvalidRegex,
    ScanLimit,
    Matches(FindProgress),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FindNavigationResult {
    /// No compressed Block search is active; the UI may use its classic VTE
    /// fallback instead.
    Inactive,
    Progress(FindProgress),
    /// The target block disappeared or VTE could not confirm the expected hit.
    /// The stale Block search has already been cleared.
    Invalidated,
}

/// Result of an action that targets one completed record by stable identity.
/// A Unified record jump is exact only when chrome proves the zone's row;
/// otherwise the retained snapshot is offered read-only, and only a record
/// with neither proof nor snapshot reports `LocationUnavailable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordNavigationResult {
    Navigated,
    NoMatchingRecord,
    /// The record exists and retains a bounded output snapshot; the UI
    /// presents it as a read-only view instead of scrolling anywhere.
    SnapshotView {
        record_id: u64,
    },
    LocationUnavailable,
}

/// Everything the read-only snapshot dialog presents for one metadata record.
/// Command identity and outcome come from the parser-fed record — never
/// re-read from any terminal surface — and the output text is the bounded
/// finalize-time snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecordSnapshotView {
    pub(crate) cmd: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) is_background: bool,
    pub(crate) output: String,
    pub(crate) truncated: bool,
}

impl RecordSnapshotView {
    /// One-line outcome header for the snapshot view. Unknown outcomes stay
    /// explicit and background output names itself; nothing here is inferred
    /// from a terminal surface.
    pub(crate) fn status_line(&self) -> String {
        let mut status = if self.is_background {
            "Background output".to_string()
        } else {
            match self.exit_code {
                Some(code) => format!("Exit code {code}"),
                None => "Exit code unknown (the shell reported none)".to_string(),
            }
        };
        if let Some(duration_ms) = self.duration_ms {
            status.push_str(&format!(
                " · {}",
                super::unified_chrome::format_block_duration(duration_ms)
            ));
        }
        status
    }

    /// User-facing truncation note; `None` when the snapshot is complete.
    pub(crate) fn truncation_note(&self) -> Option<String> {
        self.truncated.then(|| {
            format!(
                "Output truncated to the last {} KiB.",
                MAX_ZONE_SNAPSHOT_BYTES / 1024
            )
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FindDirection {
    Next,
    Previous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeCursorAction {
    AlreadySelected,
    Step { wrap_once: bool },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BoundedMatchCount {
    count: usize,
    reached_limit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowMatchPlan {
    count: usize,
    reached_limit: bool,
    incomplete: bool,
    initial_wrap: bool,
    wrap_before: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScanPrefix<'a> {
    text: &'a str,
    incomplete: bool,
}

struct FindScanBudget {
    remaining_bytes: usize,
    started: Instant,
}

impl FindScanBudget {
    fn new() -> Self {
        Self {
            remaining_bytes: FIND_SCAN_BYTE_LIMIT,
            started: Instant::now(),
        }
    }

    fn take_prefix<'a>(&mut self, text: &'a str) -> ScanPrefix<'a> {
        if self.time_exhausted() || self.remaining_bytes == 0 {
            return ScanPrefix {
                text: "",
                incomplete: !text.is_empty(),
            };
        }
        let prefix = utf8_prefix(text, self.remaining_bytes);
        self.remaining_bytes = self.remaining_bytes.saturating_sub(prefix.len());
        ScanPrefix {
            text: prefix,
            incomplete: prefix.len() < text.len(),
        }
    }

    fn time_exhausted(&self) -> bool {
        self.started.elapsed() >= FIND_SCAN_TIME_LIMIT
    }

    fn remaining_bytes(&self) -> usize {
        self.remaining_bytes
    }

    fn consume_bytes(&mut self, bytes: usize) {
        self.remaining_bytes = self.remaining_bytes.saturating_sub(bytes);
    }
}

fn utf8_prefix(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegexConsumption {
    Consuming,
    ZeroWidth,
    Never,
}

/// Classify valid Rust regexes before installing the corresponding PCRE2 regex.
/// Patterns capable of zero-width matches are rejected: VTE and Rust's iterator
/// do not expose compatible cursor semantics for assertions such as `^`, `\b`,
/// or optional/empty repetitions.
fn regex_consumption(pattern: &str) -> Result<RegexConsumption, ()> {
    let hir = regex_syntax::parse(pattern).map_err(|_| ())?;
    match hir.properties().minimum_len() {
        Some(0) => Ok(RegexConsumption::ZeroWidth),
        Some(_) => Ok(RegexConsumption::Consuming),
        None => Ok(RegexConsumption::Never),
    }
}

fn bounded_match_count(
    regex: &regex::Regex,
    haystack: &str,
    remaining: usize,
) -> BoundedMatchCount {
    if remaining == 0 {
        return BoundedMatchCount {
            count: 0,
            reached_limit: true,
        };
    }
    let count = regex.find_iter(haystack).take(remaining).count();
    BoundedMatchCount {
        count,
        reached_limit: count == remaining,
    }
}

fn plan_matching_windows(
    regex: &regex::Regex,
    windows: &[super::BackendSearchWindow],
    remaining: usize,
) -> Option<WindowMatchPlan> {
    let exact_domain = windows.iter().all(|window| !window.incomplete);
    if !exact_domain {
        return windows.iter().find_map(|window| {
            let found = bounded_match_count(regex, &window.text, remaining);
            (found.count > 0).then_some(WindowMatchPlan {
                count: found.count,
                reached_limit: found.reached_limit,
                incomplete: true,
                initial_wrap: window.initial_wrap,
                wrap_before: None,
            })
        });
    }

    let mut count = 0usize;
    let mut reached_limit = false;
    let mut initial_wrap = false;
    let mut wrap_before = None;
    for window in windows {
        let found = bounded_match_count(regex, &window.text, remaining.saturating_sub(count));
        if found.count > 0 {
            if count == 0 {
                initial_wrap = window.initial_wrap;
            }
            if window.initial_wrap && wrap_before.is_none() {
                wrap_before = Some(count);
            }
            count += found.count;
        }
        if found.reached_limit {
            reached_limit = true;
            break;
        }
    }
    (count > 0).then_some(WindowMatchPlan {
        count,
        reached_limit,
        incomplete: reached_limit,
        initial_wrap,
        // A completely counted native domain has one cyclic boundary. If its
        // wrapped region contains no hit, that boundary precedes occurrence 0.
        wrap_before: wrap_before.or((!reached_limit).then_some(0)),
    })
}

/// Select one forward native hit from the current viewport, without wrapping.
/// Used when absolute ring rows are temporarily untrusted: the selected match
/// is real, while its total and any further navigation remain fail-closed.
fn focus_one_native_forward_match(terminal: &vte4::Terminal, regex: &vte4::Regex) -> bool {
    terminal.search_set_regex(None::<&vte4::Regex>, 0);
    terminal.unselect_all();
    terminal.search_set_regex(Some(regex), 0);
    terminal.search_set_wrap_around(false);
    let found = terminal.search_find_next();
    if !found {
        terminal.search_set_regex(None::<&vte4::Regex>, 0);
    }
    found
}

fn step_compressed_cursor(
    surfaces: &[FindSurface],
    cursor: FindCursor,
    total: usize,
    capped: bool,
    direction: FindDirection,
) -> Option<(FindCursor, bool)> {
    let current = surfaces.get(cursor.surface)?;
    if current.count == 0 || total == 0 {
        return None;
    }
    // The final retained occurrence is not necessarily the real final match
    // when scanning stopped at the cap. Do not wrap through VTE: Next would
    // select cap+1, while Previous from the first match would select the real
    // (unknown) tail and desynchronize the compressed cursor.
    if capped
        && (matches!(
            direction,
            FindDirection::Next if cursor.global + 1 == total
        ) || matches!(direction, FindDirection::Previous if cursor.global == 0))
    {
        return Some((cursor, false));
    }

    let mut next = cursor;
    let surface_changed = match direction {
        FindDirection::Next if cursor.occurrence + 1 < current.count => {
            next.occurrence += 1;
            false
        }
        FindDirection::Next => {
            next.surface = (cursor.surface + 1) % surfaces.len();
            next.occurrence = 0;
            true
        }
        FindDirection::Previous if cursor.occurrence > 0 => {
            next.occurrence -= 1;
            false
        }
        FindDirection::Previous => {
            next.surface = if cursor.surface == 0 {
                surfaces.len() - 1
            } else {
                cursor.surface - 1
            };
            next.occurrence = surfaces[next.surface].count.checked_sub(1)?;
            true
        }
    };
    next.global = match direction {
        FindDirection::Next => (cursor.global + 1) % total,
        FindDirection::Previous if cursor.global == 0 => total - 1,
        FindDirection::Previous => cursor.global - 1,
    };
    Some((next, surface_changed))
}

fn native_cursor_action(
    surface: &FindSurface,
    occurrence: usize,
    direction: FindDirection,
) -> Option<NativeCursorAction> {
    if occurrence >= surface.count {
        return None;
    }
    if surface.vte_cursor == Some(occurrence) {
        return Some(NativeCursorAction::AlreadySelected);
    }
    let wrap_once = match (surface.vte_cursor, direction) {
        (None, FindDirection::Next) if occurrence == 0 => surface.initial_wrap,
        (None, FindDirection::Previous) if occurrence + 1 == surface.count => false,
        (Some(current), FindDirection::Next)
            if current + 1 < surface.count && occurrence == current + 1 =>
        {
            surface.wrap_before == Some(occurrence)
        }
        (Some(current), FindDirection::Previous) if current > 0 && occurrence + 1 == current => {
            surface.wrap_before == Some(current)
        }
        (Some(current), FindDirection::Next)
            if surface.complete && current + 1 == surface.count && occurrence == 0 =>
        {
            surface.wrap_before == Some(0)
        }
        (Some(0), FindDirection::Previous)
            if surface.complete && occurrence + 1 == surface.count =>
        {
            surface.wrap_before == Some(0)
        }
        _ => return None,
    };
    Some(NativeCursorAction::Step { wrap_once })
}

/// Resolve a logical move only while it still names the render VTE was counted
/// against. This guard deliberately precedes `AlreadySelected`: a one-hit pass
/// can otherwise keep reporting its stale highlight forever without taking a
/// native step that would notice the card was re-fed.
fn validated_native_cursor_action(
    surface: &FindSurface,
    occurrence: usize,
    direction: FindDirection,
    current_render_stamp: Option<super::blocks::RenderStamp>,
) -> Option<NativeCursorAction> {
    let render_is_current = (surface.is_live && surface.block_id == 0)
        || current_render_stamp.is_some_and(|stamp| stamp == surface.render_stamp);
    render_is_current
        .then(|| native_cursor_action(surface, occurrence, direction))
        .flatten()
}

fn find_progress(state: &FindState) -> Option<FindProgress> {
    (!state.surfaces.is_empty() && state.total > 0).then_some(FindProgress {
        current: state.cursor.global + 1,
        total: state.total,
        capped: state.capped,
        scan_limited: state.scan_limited,
    })
}

/// Matching controls for the cross-block result picker. Keeping this as one
/// value prevents the scan and the VTE jump highlighter from interpreting the
/// same query differently.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrossBlockSearchOptions {
    pub case_sensitive: bool,
    pub regex: bool,
    pub whole_word: bool,
}

/// Text surfaces included in a cross-block scan. Scope is applied before the
/// hit cap so a command-heavy history cannot hide output-only results.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CrossBlockSearchScope {
    #[default]
    All,
    Command,
    Output,
}

impl CrossBlockSearchScope {
    pub fn includes_command(self) -> bool {
        matches!(self, Self::All | Self::Command)
    }

    pub fn includes_output(self) -> bool {
        matches!(self, Self::All | Self::Output)
    }

    pub fn from_index(index: u32) -> Self {
        match index {
            1 => Self::Command,
            2 => Self::Output,
            _ => Self::All,
        }
    }

    pub fn index(self) -> u32 {
        match self {
            Self::All => 0,
            Self::Command => 1,
            Self::Output => 2,
        }
    }

    pub fn cycled(self) -> Self {
        match self {
            Self::All => Self::Command,
            Self::Command => Self::Output,
            Self::Output => Self::All,
        }
    }
}

/// Finalized-record identity observed by an open cross-block picker. Length
/// alone is insufficient because retention can evict one old record while a
/// new one arrives in the same update.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrossBlockSearchVersion {
    len: usize,
    oldest: Option<u64>,
    newest: Option<u64>,
}

fn cross_block_search_version(ids: impl IntoIterator<Item = u64>) -> CrossBlockSearchVersion {
    ids.into_iter()
        .fold(CrossBlockSearchVersion::default(), |mut version, id| {
            if version.len == 0 {
                version.oldest = Some(id);
            }
            version.len = version.len.saturating_add(1);
            version.newest = Some(id);
            version
        })
}

fn cross_block_pattern(pattern: &str, options: CrossBlockSearchOptions) -> String {
    if options.regex {
        pattern.to_string()
    } else {
        regex::escape(pattern)
    }
}

fn vte_cross_block_pattern(pattern: &str, options: CrossBlockSearchOptions) -> String {
    let pattern = cross_block_pattern(pattern, options);
    if options.whole_word {
        format!(r"(?<![\p{{L}}\p{{N}}_])(?:{pattern})(?![\p{{L}}\p{{N}}_])")
    } else {
        pattern
    }
}

fn is_word_character(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

fn is_whole_word_match(text: &str, range: std::ops::Range<usize>) -> bool {
    let before = text[..range.start].chars().next_back();
    let after = text[range.end..].chars().next();
    before.is_none_or(|character| !is_word_character(character))
        && after.is_none_or(|character| !is_word_character(character))
}

fn cross_block_match_count(regex: &regex::Regex, line: &str, whole_word: bool) -> usize {
    regex
        .find_iter(line)
        .filter(|matched| !whole_word || is_whole_word_match(line, matched.range()))
        .count()
}

/// One result row from the built-in cross-block substring/regex scan. Carries enough
/// context for a flat result list — block id (for jump), surface flag (so
/// the per-block VTE search cursor goes to the right widget), the 1-based
/// line number inside that surface, the line snippet itself (trimmed/
/// truncated for display), and a one-line cmd preview for context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossBlockHit {
    pub block_id: u64,
    pub is_output: bool,
    pub line_no: usize,
    pub line_text: String,
    pub cmd_preview: String,
    /// Outcome of the record the hit came from. Carried on the hit so the
    /// palette can show which `cargo build` failed without making the user
    /// jump to each one to find out.
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    pub cwd: Option<String>,
    /// Zero-based index of this row's first match within its command/output
    /// surface. VTE advances by matches (not matching lines), so this is the
    /// position required to make an activated result land where its label says.
    pub occurrence: usize,
}

/// Bound native VTE cursor work performed by one palette activation.
fn bounded_occurrence_steps(occurrence: usize) -> Option<usize> {
    const MAX_JUMP_STEPS: usize = 4_096;
    occurrence
        .checked_add(1)
        .filter(|steps| *steps <= MAX_JUMP_STEPS)
}

/// Execute the complete bounded jump. `all` short-circuits on the first native
/// miss, so a surface that contains fewer matches than the scan recorded can
/// never turn a partial walk into a successful (but wrong) highlight.
fn step_to_occurrence_exact(occurrence: usize, mut step: impl FnMut() -> bool) -> bool {
    bounded_occurrence_steps(occurrence).is_some_and(|steps| (0..steps).all(|_| step()))
}

/// Trim a line to a reasonable display width — the palette row is one
/// horizontal line so an unbounded long line (think bundled JSON) would
/// just blow out the dialog width. We truncate with a leading ellipsis if
/// the match isn't near the start, but for the MVP we just hard-cap.
fn snippet(line: &str) -> String {
    const CAP: usize = 240;
    let mut chars = line.chars();
    let mut snippet: String = chars.by_ref().take(CAP).collect();
    if chars.next().is_some() {
        snippet.push('…');
    }
    snippet
}

fn command_preview(command: &str) -> String {
    snippet(command.lines().next().unwrap_or(command))
}

fn duration_matches(duration: Option<u64>, filters: &BlockFilters) -> bool {
    let needs_duration =
        filters.min_duration_ms.is_some() || filters.max_duration_ms.is_some() || filters.slow_only;
    if !needs_duration {
        return true;
    }
    let Some(duration) = duration else {
        return false;
    };
    if filters.min_duration_ms.is_some_and(|min| duration < min) {
        return false;
    }
    if filters.max_duration_ms.is_some_and(|max| duration > max) {
        return false;
    }
    !filters.slow_only || duration >= filters.slow_threshold_ms
}

fn has_command_lifecycle_filters(filters: &BlockFilters) -> bool {
    filters.exit_code.is_some()
        || filters.min_duration_ms.is_some()
        || filters.max_duration_ms.is_some()
        || filters.failed_only
        || filters.slow_only
}

fn record_matches_filters(record: BackendRecordRef<'_>, filters: &BlockFilters) -> bool {
    let is_background = record.is_background();
    (!filters.background_only || is_background)
        // A background record belongs to no command lifecycle. Its raw status
        // and duration are therefore never outcome/timing matches, even if a
        // legacy or contradictory source happened to carry either field.
        && (!is_background || !has_command_lifecycle_filters(filters))
        && record_outcome_matches_filters(record, filters)
        && duration_matches(record.duration_ms(), filters)
}

/// Classify outcome without allowing command text to override the backend's
/// record identity. Block records derive that identity from commandlessness,
/// while Unified metadata carries an explicit bit that is authoritative in
/// both directions. The sentinel is classification-only: it never becomes
/// searchable or visible in a result row.
fn record_outcome_matches_filters(record: BackendRecordRef<'_>, filters: &BlockFilters) -> bool {
    const EXPLICIT_FOREGROUND_COMMAND: &str = "<foreground command unavailable>";

    let command = record.command();
    let classification_command = if !record.is_background() && command.trim().is_empty() {
        EXPLICIT_FOREGROUND_COMMAND
    } else {
        command
    };
    outcome_matches_filters(classification_command, record.exit_code(), filters)
}

fn has_metadata_filters(filters: &BlockFilters) -> bool {
    has_command_lifecycle_filters(filters) || filters.background_only
}

fn first_meaningful_line(text: &str) -> Option<(usize, &str)> {
    text.lines()
        .enumerate()
        .find(|(_, line)| !line.trim().is_empty())
}

/// An empty text query with active metadata filters is a block browser rather
/// than an idle picker. Represent each eligible record once, choosing the
/// first meaningful line on the requested surface.
fn metadata_filter_hit(
    record: BackendRecordRef<'_>,
    scope: CrossBlockSearchScope,
) -> Option<CrossBlockHit> {
    let command = record.command();
    let command_line = first_meaningful_line(command);
    let output_line = record.output().and_then(first_meaningful_line);
    let (is_output, line_no, line) = match scope {
        CrossBlockSearchScope::All => command_line
            .map(|(index, line)| (false, index + 1, line))
            .or_else(|| output_line.map(|(index, line)| (true, index + 1, line)))?,
        CrossBlockSearchScope::Command => {
            let (index, line) = command_line?;
            (false, index + 1, line)
        }
        CrossBlockSearchScope::Output => {
            let (index, line) = output_line?;
            (true, index + 1, line)
        }
    };
    Some(CrossBlockHit {
        block_id: record.id(),
        is_output,
        line_no,
        line_text: snippet(line),
        cmd_preview: command_preview(command),
        exit_code: record.exit_code(),
        duration_ms: record.duration_ms(),
        cwd: record.cwd().map(str::to_string),
        occurrence: 0,
    })
}

fn metadata_filter_hits<'a>(
    records: impl IntoIterator<Item = BackendRecordRef<'a>>,
    scope: CrossBlockSearchScope,
    max_hits: usize,
    filters: &BlockFilters,
) -> Vec<CrossBlockHit> {
    records
        .into_iter()
        .filter(|record| record_matches_filters(*record, filters))
        .filter_map(|record| metadata_filter_hit(record, scope))
        .take(max_hits)
        .collect()
}

fn matching_record_ids<'a>(
    records: impl IntoIterator<Item = super::BackendRecordRef<'a>>,
    query: &str,
    filters: &BlockFilters,
) -> Vec<u64> {
    let q = query.to_lowercase();
    let q_bytes = q.as_bytes();
    let re = if filters.use_regex && !query.is_empty() {
        regex::RegexBuilder::new(query)
            .case_insensitive(true)
            .build()
            .ok()
    } else {
        None
    };

    records
        .into_iter()
        .filter_map(|record| {
            let prompt = record.prompt().unwrap_or("");
            let command = record.command();
            let output = record.output().unwrap_or("");
            let text_match = if q.is_empty() {
                true
            } else if let Some(ref re) = re {
                re.is_match(prompt) || re.is_match(command) || re.is_match(output)
            } else {
                contains_case_insensitive(prompt.as_bytes(), q_bytes)
                    || contains_case_insensitive(command.as_bytes(), q_bytes)
                    || contains_case_insensitive(output.as_bytes(), q_bytes)
            };
            if !text_match || !record_matches_filters(record, filters) {
                return None;
            }
            Some(record.id())
        })
        .collect()
}

fn unresolved_record_target_result<'a>(
    records: impl IntoIterator<Item = super::BackendRecordRef<'a>>,
    block_id: u64,
) -> RecordNavigationResult {
    let Some(record) = records.into_iter().find(|record| record.id() == block_id) else {
        return RecordNavigationResult::NoMatchingRecord;
    };
    // Only a metadata record falls back to its snapshot: a Block record whose
    // widget target vanished mid-operation was concurrently removed, not
    // retained without a surface.
    if record.is_metadata_only() && record.output().is_some() {
        RecordNavigationResult::SnapshotView {
            record_id: block_id,
        }
    } else {
        RecordNavigationResult::LocationUnavailable
    }
}

fn add_snapshot_jump_fallbacks<'a>(
    records: impl IntoIterator<Item = BackendRecordRef<'a>>,
    candidates: &HashSet<(u64, bool)>,
    jumpable: &mut HashSet<(u64, bool)>,
) {
    for record in records {
        if !record.is_metadata_only() || record.output().is_none() {
            continue;
        }
        for is_output in [false, true] {
            let candidate = (record.id(), is_output);
            if candidates.contains(&candidate) {
                jumpable.insert(candidate);
            }
        }
    }
}

#[allow(dead_code)]
impl TermView {
    /// Cheap version probe for an open picker. No command/output text is
    /// cloned; callers can poll this and rebuild only when finalized record
    /// identity actually changes.
    pub fn cross_block_search_version(&self) -> CrossBlockSearchVersion {
        let records = self.render_backend.records();
        cross_block_search_version(records.iter().map(|record| record.id()))
    }

    /// Search records for a query string, returning stable record ids.
    pub fn search_blocks(&self, query: &str) -> Vec<u64> {
        self.search_blocks_with_filters(query, &BlockFilters::default())
    }

    /// Search completed records with optional filters, returning stable ids.
    pub fn search_blocks_with_filters(&self, query: &str, filters: &BlockFilters) -> Vec<u64> {
        let records = self.render_backend.records();
        matching_record_ids(records.iter(), query, filters)
    }

    /// Highlight occurrences of `query` across the finished blocks and focus
    /// the first hit. Match metadata is compressed to one count per VTE surface,
    /// and scanning stops as soon as [`FIND_MATCH_LIMIT`] is reached.
    pub(crate) fn find_in_blocks(&self, query: &str, use_regex: bool) -> FindSearchResult {
        self.clear_find();
        if query.is_empty() {
            return FindSearchResult::NoMatches;
        }
        let pattern = if use_regex {
            query.to_string()
        } else {
            regex::escape(query)
        };
        match regex_consumption(&pattern) {
            Ok(RegexConsumption::Consuming) => {}
            Ok(RegexConsumption::Never) => return FindSearchResult::NoMatches,
            Ok(RegexConsumption::ZeroWidth) | Err(_) => {
                return FindSearchResult::InvalidRegex;
            }
        }
        let re = match regex::RegexBuilder::new(&pattern)
            .case_insensitive(true)
            .multi_line(true)
            .build()
        {
            Ok(re) => re,
            Err(_) => return FindSearchResult::InvalidRegex,
        };

        // Compile the same pattern for VTE (PCRE2) so its native highlighter
        // paints every hit and its search cursor can step within each block.
        let vte_re = match vte4::Regex::for_search(&pattern, VTE_SEARCH_FLAGS) {
            Ok(r) => r,
            Err(_) => return FindSearchResult::InvalidRegex,
        };

        let mut surfaces = Vec::new();
        let mut highlighted_terminals = Vec::new();
        let mut total = 0usize;
        let mut match_limited = false;
        let mut scan_limited = false;
        let mut scan_budget = FindScanBudget::new();
        let completed_batch = {
            let mut deadline_exhausted = || scan_budget.time_exhausted();
            self.render_backend
                .completed_search_surfaces(scan_budget.remaining_bytes(), &mut deadline_exhausted)
        };
        let completed_owns_live_surface = completed_batch
            .surfaces
            .iter()
            .any(|surface| surface.is_live)
            || completed_batch
                .native_fallback
                .as_ref()
                .is_some_and(|fallback| fallback.is_live);
        let completed_incomplete = completed_batch.incomplete;
        let native_fallback = completed_batch.native_fallback;
        for backend_surface in completed_batch.surfaces {
            scan_budget.consume_bytes(backend_surface.scanned_bytes);
            let selected = plan_matching_windows(
                &re,
                &backend_surface.windows,
                FIND_MATCH_LIMIT.saturating_sub(total),
            );
            if let Some(plan) = selected {
                if backend_surface.reset_cursor {
                    // A shared Unified VTE retains its selection/search anchor
                    // across queries. Clear it so the first native step begins
                    // at the current viewport, exactly like window zero.
                    backend_surface.terminal.unselect_all();
                }
                backend_surface.terminal.search_set_regex(Some(&vte_re), 0);
                backend_surface.terminal.search_set_wrap_around(false);
                if !highlighted_terminals
                    .iter()
                    .any(|terminal| terminal == &backend_surface.terminal)
                {
                    highlighted_terminals.push(backend_surface.terminal.clone());
                }
                surfaces.push(FindSurface {
                    block_id: backend_surface.block_id,
                    block_index: backend_surface.block_index,
                    is_output: backend_surface.is_output,
                    is_live: backend_surface.is_live,
                    count: plan.count,
                    vte_cursor: None,
                    complete: !plan.incomplete,
                    initial_wrap: plan.initial_wrap,
                    wrap_before: plan.wrap_before,
                    render_stamp: backend_surface.render_stamp,
                });
                total += plan.count;
                if plan.reached_limit {
                    surfaces
                        .last_mut()
                        .expect("a matching backend surface was just appended")
                        .complete = false;
                    match_limited = true;
                    break;
                }
                if plan.incomplete || scan_budget.time_exhausted() {
                    surfaces
                        .last_mut()
                        .expect("a matching backend surface was just appended")
                        .complete = false;
                    scan_limited = true;
                    break;
                }
            } else if scan_budget.time_exhausted() {
                scan_limited = true;
                break;
            }
        }
        if !match_limited && !scan_limited && completed_incomplete {
            scan_limited = true;
        }

        // If trusted rows are unavailable (or a bounded snapshot stopped
        // before finding anything), let the persistent VTE prove one visible
        // forward hit. Represent it as a capped 1+ result with no wrap.
        if surfaces.is_empty() && scan_limited {
            if let Some(fallback) = native_fallback {
                if focus_one_native_forward_match(&fallback.terminal, &vte_re) {
                    if !highlighted_terminals
                        .iter()
                        .any(|terminal| terminal == &fallback.terminal)
                    {
                        highlighted_terminals.push(fallback.terminal.clone());
                    }
                    surfaces.push(FindSurface {
                        block_id: fallback.block_id,
                        block_index: fallback.block_index,
                        is_output: fallback.is_output,
                        is_live: fallback.is_live,
                        count: 1,
                        vte_cursor: Some(0),
                        complete: false,
                        initial_wrap: false,
                        wrap_before: None,
                        render_stamp: super::blocks::NEUTRAL_RENDER_STAMP,
                    });
                    total = 1;
                }
            }
        }

        // The still-running command's output is searchable too (document
        // order: it sits below every finished block). Counted from the
        // accumulated raw capture, so only states that accumulate qualify;
        // VTE's own highlighter paints and steps the on-screen hits.
        if !match_limited
            && !scan_limited
            && !completed_owns_live_surface
            && matches!(
                self.bstate.get(),
                super::BlockState::CollectingOutput | super::BlockState::PostCommand
            )
        {
            let (live_raw, live_raw_incomplete) = self
                .active
                .borrow()
                .output_text_prefix(scan_budget.remaining_bytes());
            let live_prefix = scan_budget.take_prefix(&live_raw);
            let live_text = super::strip_ansi(live_prefix.text);
            let live = bounded_match_count(&re, &live_text, FIND_MATCH_LIMIT.saturating_sub(total));
            if live.count > 0 {
                self.active_vte.search_set_regex(Some(&vte_re), 0);
                self.active_vte.search_set_wrap_around(false);
                if !highlighted_terminals
                    .iter()
                    .any(|terminal| terminal == &self.active_vte)
                {
                    highlighted_terminals.push(self.active_vte.clone());
                }
                surfaces.push(FindSurface {
                    block_id: 0,
                    block_index: 0,
                    is_output: true,
                    is_live: true,
                    count: live.count,
                    vte_cursor: None,
                    complete: true,
                    initial_wrap: false,
                    wrap_before: Some(0),
                    render_stamp: super::blocks::NEUTRAL_RENDER_STAMP,
                });
                total += live.count;
            }
            match_limited = live.reached_limit;
            scan_limited = !match_limited
                && (live_raw_incomplete || live_prefix.incomplete || scan_budget.time_exhausted());
            if live.count > 0 && (match_limited || live_raw_incomplete || live_prefix.incomplete) {
                surfaces
                    .last_mut()
                    .expect("a matching live surface was just appended")
                    .complete = false;
            }
        }

        if surfaces.is_empty() {
            return if scan_limited {
                FindSearchResult::ScanLimit
            } else {
                FindSearchResult::NoMatches
            };
        }
        let capped = match_limited || scan_limited;
        {
            let mut st = self.find_state.borrow_mut();
            st.surfaces = surfaces;
            st.cursor = FindCursor::default();
            st.total = total;
            st.capped = capped;
            st.scan_limited = scan_limited;
            st.highlighted_terminals = highlighted_terminals;
        }
        if !self.focus_current_match() {
            self.clear_find();
            return FindSearchResult::NoMatches;
        }
        self.scroll_to_current_match();
        FindSearchResult::Matches(FindProgress {
            current: 1,
            total,
            capped,
            scan_limited,
        })
    }

    /// Step to the next match. Exact result sets wrap; capped sets stop at the
    /// known edge rather than entering uncounted VTE matches.
    pub(crate) fn find_next(&self) -> FindNavigationResult {
        self.step_find(FindDirection::Next)
    }

    /// Step to the previous match. Exact result sets wrap; capped sets stop at
    /// the known edge rather than entering the unknown real tail.
    pub(crate) fn find_prev(&self) -> FindNavigationResult {
        self.step_find(FindDirection::Previous)
    }

    fn step_find(&self, direction: FindDirection) -> FindNavigationResult {
        let (current, next, current_progress) = {
            let state = self.find_state.borrow();
            let Some(current_progress) = find_progress(&state) else {
                return FindNavigationResult::Inactive;
            };
            let step = step_compressed_cursor(
                &state.surfaces,
                state.cursor,
                state.total,
                state.capped,
                direction,
            );
            let current = state.cursor;
            drop(state);
            let Some((next, _surface_changed)) = step else {
                self.clear_find();
                return FindNavigationResult::Invalidated;
            };
            (current, next, current_progress)
        };
        if next == current {
            if !self.focus_surface_occurrence(next.surface, next.occurrence, direction) {
                self.clear_find();
                return FindNavigationResult::Invalidated;
            }
            return FindNavigationResult::Progress(current_progress);
        }

        if !self.focus_surface_occurrence(next.surface, next.occurrence, direction) {
            self.clear_find();
            return FindNavigationResult::Invalidated;
        }
        {
            let mut state = self.find_state.borrow_mut();
            if state.cursor != current {
                drop(state);
                self.clear_find();
                return FindNavigationResult::Invalidated;
            }
            state.cursor = next;
        }
        self.scroll_to_current_match();
        let progress = {
            let state = self.find_state.borrow();
            find_progress(&state)
        };
        let Some(progress) = progress else {
            self.clear_find();
            return FindNavigationResult::Invalidated;
        };
        FindNavigationResult::Progress(progress)
    }

    /// Ask VTE to select one exact compressed occurrence. The native and logical
    /// cursors advance together only after VTE confirms success. Native wrapping
    /// is enabled for one call only when the entire target surface was scanned;
    /// it is always left disabled, especially for capped prefixes.
    fn focus_surface_occurrence(
        &self,
        surface_index: usize,
        occurrence: usize,
        direction: FindDirection,
    ) -> bool {
        let surface = {
            let state = self.find_state.borrow();
            let Some(surface) = state.surfaces.get(surface_index) else {
                return false;
            };
            surface.clone()
        };
        let (vte, current_render_stamp) = if surface.is_live && surface.block_id == 0 {
            (self.active_vte.clone(), None)
        } else {
            let Some(target) = self
                .render_backend
                .record_search_target(surface.block_id, surface.is_output)
            else {
                return false;
            };
            // A resize, expand/collapse or output filter changed this card's
            // native row domain after it was counted. Stepping the old cursor
            // would silently select a different occurrence; invalidate the
            // pass so the adapter can rebuild it from the retained query.
            (target.terminal, Some(target.render_stamp))
        };
        let wrap_once = match validated_native_cursor_action(
            &surface,
            occurrence,
            direction,
            current_render_stamp,
        ) {
            Some(NativeCursorAction::AlreadySelected) => return true,
            Some(NativeCursorAction::Step { wrap_once }) => wrap_once,
            None => return false,
        };
        vte.search_set_wrap_around(wrap_once);
        let found = match direction {
            FindDirection::Next => vte.search_find_next(),
            FindDirection::Previous => vte.search_find_previous(),
        };
        vte.search_set_wrap_around(false);
        if !found {
            return false;
        }

        let mut state = self.find_state.borrow_mut();
        let Some(target) = state.surfaces.get_mut(surface_index) else {
            return false;
        };
        if target.block_id != surface.block_id
            || target.is_output != surface.is_output
            || target.is_live != surface.is_live
            || target.count != surface.count
        {
            return false;
        }
        target.vte_cursor = Some(occurrence);
        true
    }

    /// Move VTE's search cursor to the very first match of the current pass.
    fn focus_current_match(&self) -> bool {
        self.focus_surface_occurrence(0, 0, FindDirection::Next)
    }

    fn scroll_to_current_match(&self) {
        let st = self.find_state.borrow();
        let Some(surface) = st.surfaces.get(st.cursor.surface) else {
            return;
        };
        let widget: gtk::Widget = if surface.is_live && surface.block_id == 0 {
            self.active.borrow().widget().clone().upcast()
        } else {
            let Some(target) = self
                .render_backend
                .record_search_target(surface.block_id, surface.is_output)
            else {
                return;
            };
            target.widget
        };
        let scroll = self.block_scroll.clone();
        glib::idle_add_local_once(move || {
            if let Some(point) = widget.compute_point(&scroll, &gtk::graphene::Point::new(0.0, 0.0))
            {
                let adj = scroll.vadjustment();
                let max_value = (adj.upper() - adj.page_size()).max(adj.lower());
                let target = adj.value() + point.y() as f64 - adj.page_size() / 3.0;
                adj.set_value(target.clamp(adj.lower(), max_value));
            }
        });
    }

    /// Cross-block substring/regex flat-result scan over cached stripped output
    /// and command text. Caller passes a literal substring (case-insensitive)
    /// when `is_regex == false`, else a regex.
    ///
    /// Returns at most `max_hits` hits in block-list order; each hit carries
    /// enough context (line number + the raw line + cmd preview) to drive a
    /// palette UI that lets the user pick one and jump to it.
    ///
    /// Errors only on invalid regex. An empty pattern returns one representative
    /// row per eligible record when metadata filters are active, otherwise no
    /// rows.
    /// Scan every retained record for `pattern`, honoring the same outcome and
    /// duration predicates the block filters use.
    ///
    /// `filters` is applied per record, before its lines are scanned: the
    /// predicates already existed and had no surface that could reach them, so
    /// "which failing command took over two seconds" was unanswerable with the
    /// data sitting right there.
    pub fn cross_block_search(
        &self,
        pattern: &str,
        options: CrossBlockSearchOptions,
        max_hits: usize,
        filters: &BlockFilters,
    ) -> Result<Vec<CrossBlockHit>, String> {
        self.cross_block_search_in_scope(
            pattern,
            options,
            CrossBlockSearchScope::All,
            max_hits,
            filters,
        )
    }

    /// Scope-aware counterpart of [`Self::cross_block_search`].
    pub fn cross_block_search_in_scope(
        &self,
        pattern: &str,
        options: CrossBlockSearchOptions,
        scope: CrossBlockSearchScope,
        max_hits: usize,
        filters: &BlockFilters,
    ) -> Result<Vec<CrossBlockHit>, String> {
        if pattern.is_empty() {
            if !has_metadata_filters(filters) {
                return Ok(Vec::new());
            }
            let records = self.render_backend.records();
            return Ok(metadata_filter_hits(
                records.iter(),
                scope,
                max_hits,
                filters,
            ));
        }

        let compiled_pattern = cross_block_pattern(pattern, options);
        let re = regex::RegexBuilder::new(&compiled_pattern)
            .case_insensitive(!options.case_sensitive)
            .multi_line(true)
            .size_limit(CROSS_BLOCK_REGEX_SIZE_LIMIT)
            .build()
            .map_err(|e| format!("{e}"))?;

        let records = self.render_backend.records();
        let mut hits: Vec<CrossBlockHit> = Vec::new();

        for record in records.iter() {
            if hits.len() >= max_hits {
                break;
            }
            // Metadata predicates run before this record contributes any
            // command/output hit, so an excluded record cannot spend the
            // bounded result budget and starve a later eligible record.
            if !record_matches_filters(record, filters) {
                continue;
            }
            let command = record.command();
            let cmd_preview = command_preview(command);
            let exit_code = record.exit_code();
            let duration_ms = record.duration_ms();
            let cwd = record.cwd().map(str::to_string);

            // Cmd surface — usually one line, but multiline commands exist.
            // Count matches rather than matching lines because that is VTE's
            // native cursor unit.
            if scope.includes_command() {
                let mut occurrence = 0usize;
                for (ln_idx, line) in command.lines().enumerate() {
                    if hits.len() >= max_hits {
                        break;
                    }
                    let matches = cross_block_match_count(&re, line, options.whole_word);
                    if matches > 0 {
                        hits.push(CrossBlockHit {
                            block_id: record.id(),
                            is_output: false,
                            line_no: ln_idx + 1,
                            line_text: snippet(line),
                            cmd_preview: cmd_preview.clone(),
                            exit_code,
                            duration_ms,
                            cwd: cwd.clone(),
                            occurrence,
                        });
                    }
                    occurrence = occurrence.saturating_add(matches);
                }
            }

            if scope.includes_output() {
                let mut occurrence = 0usize;
                for (ln_idx, line) in record.output().unwrap_or("").lines().enumerate() {
                    if hits.len() >= max_hits {
                        break;
                    }
                    let matches = cross_block_match_count(&re, line, options.whole_word);
                    if matches > 0 {
                        hits.push(CrossBlockHit {
                            block_id: record.id(),
                            is_output: true,
                            line_no: ln_idx + 1,
                            line_text: snippet(line),
                            cmd_preview: cmd_preview.clone(),
                            exit_code,
                            duration_ms,
                            cwd: cwd.clone(),
                            occurrence,
                        });
                    }
                    occurrence = occurrence.saturating_add(matches);
                }
            }
        }
        Ok(hits)
    }

    /// Whether activating this hit would show the user anything: a per-record
    /// surface, an exact proven scroll, or the record's retained snapshot.
    /// Every rung `navigate_to_record_id` can reach is one here — a hit
    /// labelled reachable must lead somewhere, and a hit labelled unavailable
    /// must have nothing left to offer.
    pub fn can_jump_to_record(&self, block_id: u64, is_output: bool) -> bool {
        if self
            .render_backend
            .record_search_target(block_id, is_output)
            .is_some()
            || self.render_backend.can_scroll_to_record(block_id)
        {
            return true;
        }
        let records = self.render_backend.records();
        matches!(
            unresolved_record_target_result(records.iter(), block_id),
            RecordNavigationResult::SnapshotView { .. }
        )
    }

    /// Resolve reachability for one rendered result page at once. Block mode
    /// intersects candidate ids with mounted cards in one document pass;
    /// metadata-only records then gain the same retained-snapshot fallback as
    /// [`Self::can_jump_to_record`]. Unified's backend default still asks the
    /// exact proven-scroll predicate for every candidate, preserving its row-
    /// authority semantics.
    pub(crate) fn jumpable_search_hits(&self, hits: &[CrossBlockHit]) -> HashSet<(u64, bool)> {
        let candidates: HashSet<_> = hits
            .iter()
            .map(|hit| (hit.block_id, hit.is_output))
            .collect();
        if candidates.is_empty() {
            return HashSet::new();
        }

        let mut jumpable = self.render_backend.jumpable_records(&candidates);
        if jumpable.len() == candidates.len() {
            return jumpable;
        }

        let records = self.render_backend.records();
        add_snapshot_jump_fallbacks(records.iter(), &candidates, &mut jumpable);
        jumpable
    }

    pub(crate) fn navigate_to_record_id(
        &self,
        block_id: u64,
        is_output: bool,
    ) -> RecordNavigationResult {
        let Some(target) = self
            .render_backend
            .record_search_target(block_id, is_output)
        else {
            // No per-record surface: an exact proven scroll is still allowed,
            // then the retained snapshot, then the honest notice.
            let result = if self.render_backend.scroll_to_record(block_id) {
                RecordNavigationResult::Navigated
            } else {
                let records = self.render_backend.records();
                unresolved_record_target_result(records.iter(), block_id)
            };
            // A backend with no per-record widget never writes
            // `selected_block_id`, so stepping has no other cursor to read:
            // record wherever the user was last sent, including a record that
            // could only be reported, or next/previous re-open one record
            // forever.
            if result != RecordNavigationResult::NoMatchingRecord {
                self.navigated_record_id.set(Some(block_id));
            }
            return result;
        };
        if target.uses_live_surface {
            target.terminal.grab_focus();
            return RecordNavigationResult::Navigated;
        }
        self.cross_selection.clear_all();
        {
            let finished = self.finished_blocks.borrow();
            if !finished.iter().any(|block| block.id == block_id) {
                return RecordNavigationResult::NoMatchingRecord;
            }
            replace_finished_block_selection(
                &finished,
                &self.selected_block_ids,
                &self.selected_block_id,
                &self.selection_anchor_id,
                Some(block_id),
            );
        }
        // The selection this just wrote is the stepping cursor for a backend
        // that mounts widgets; the fallback must not shadow it.
        self.navigated_record_id.set(None);
        target.widget.grab_focus();
        scroll_widget_to_block_scroller_top(&target.widget, &self.block_scroll);
        RecordNavigationResult::Navigated
    }

    /// Snapshot-view payload for one metadata record, `None` when the record
    /// is gone, is not metadata-only, or no longer retains a snapshot (the
    /// budget may have evicted it between navigation and presentation).
    pub(crate) fn record_snapshot_view(&self, record_id: u64) -> Option<RecordSnapshotView> {
        let records = self.render_backend.records();
        let record = records.iter().find(|record| record.id() == record_id)?;
        let BackendRecordRef::Metadata {
            record,
            snapshot: Some(snapshot),
        } = record
        else {
            return None;
        };
        Some(RecordSnapshotView {
            cmd: record.cmd.clone(),
            exit_code: record.exit_code,
            duration_ms: record.duration_ms,
            is_background: record.is_background,
            output: snapshot.plain.clone(),
            truncated: snapshot.truncated,
        })
    }

    /// Light up the chosen block's command/output VTE with a PCRE2 search
    /// for `pattern` and advance its internal search cursor to the first
    /// hit. Other blocks keep whatever highlight state they had — this is
    /// the "jump to this hit" companion for `cross_block_search`. Returns
    /// `false` when the id is unknown or the pattern can't compile.
    pub fn focus_match_in_block(
        &self,
        block_id: u64,
        pattern: &str,
        options: CrossBlockSearchOptions,
        is_output: bool,
        occurrence: usize,
    ) -> bool {
        if pattern.is_empty() {
            return false;
        }
        let compiled = vte_cross_block_pattern(pattern, options);
        let flags = if options.case_sensitive {
            VTE_SEARCH_FLAGS & !pcre2_sys::PCRE2_CASELESS
        } else {
            VTE_SEARCH_FLAGS
        };
        let Ok(vte_re) = vte4::Regex::for_search(&compiled, flags) else {
            return false;
        };
        let records = self.render_backend.records();
        let Some(block_index) = records.iter().position(|record| record.id() == block_id) else {
            return false;
        };
        drop(records);
        let Some(target) = self
            .render_backend
            .record_search_target(block_id, is_output)
        else {
            return false;
        };
        let vte = target.terminal;
        // Re-establish the cursor from the top of this surface. Reusing VTE's
        // previous selection made repeated activation walk forward and made a
        // row labelled L482 land on an unrelated next hit.
        vte.unselect_all();
        vte.search_set_regex(Some(&vte_re), 0);
        vte.search_set_wrap_around(false);
        if !step_to_occurrence_exact(occurrence, || vte.search_find_next()) {
            vte.search_set_regex(None::<&vte4::Regex>, 0);
            return false;
        }
        let highlight = FindHighlight {
            block_id,
            block_index,
            is_output,
        };
        let mut state = self.find_state.borrow_mut();
        if !state
            .highlighted_terminals
            .iter()
            .any(|terminal| terminal == &vte)
        {
            state.highlighted_terminals.push(vte);
        }
        if !state.extra_highlights.contains(&highlight) {
            state.extra_highlights.push(highlight);
        }
        true
    }

    /// Remove all find highlights and reset the find cursor (call on close).
    pub fn clear_find(&self) {
        clear_find_state(self.find_state.as_ref(), &self.active_vte);
    }

    /// Stable ids of failed completed records.
    pub fn get_failed_blocks(&self) -> Vec<u64> {
        let filters = BlockFilters {
            failed_only: true,
            ..Default::default()
        };
        self.search_blocks_with_filters("", &filters)
    }

    /// Stable ids of slow completed records.
    pub fn get_slow_blocks(&self, threshold_ms: u64) -> Vec<u64> {
        let filters = BlockFilters {
            slow_only: true,
            slow_threshold_ms: threshold_ms,
            ..Default::default()
        };
        self.search_blocks_with_filters("", &filters)
    }
}

/// Scroll the outer Block scroller so `widget`'s top edge lands at the
/// viewport top (clamped to the scroll range). Shared by record navigation and
/// the Block backend's `scroll_to_record` seam so both jumps land the same way.
pub(super) fn scroll_widget_to_block_scroller_top(
    widget: &gtk::Widget,
    block_scroll: &gtk::ScrolledWindow,
) {
    let adj = block_scroll.vadjustment();
    if let Some(value) = widget.compute_point(block_scroll, &gtk::graphene::Point::new(0.0, 0.0)) {
        let max_value = (adj.upper() - adj.page_size()).max(adj.lower());
        let target_value = adj.value() + value.y() as f64;
        adj.set_value(target_value.clamp(adj.lower(), max_value));
    }
}

/// Reset a pane's search before its finished-block structure changes. Resolve
/// the highlighted terminals while the block list is borrowed, then release
/// that borrow before calling into GTK so a synchronous signal cannot re-enter
/// a structural path and panic on the `RefCell`.
pub(super) fn clear_find_state(
    find_state: &std::cell::RefCell<FindState>,
    active_vte: &vte4::Terminal,
) {
    let highlighted_terminals = {
        let state = std::mem::take(&mut *find_state.borrow_mut());
        state.highlighted_terminals
    };
    for vte in highlighted_terminals {
        vte.search_set_regex(None::<&vte4::Regex>, 0);
        // VTE retains its last match as the next search anchor even after the
        // regex is removed. Clear only terminals highlighted by this pass so a
        // fresh query can reach a match above the old one.
        vte.unselect_all();
    }
    // The UI's no-record fallback installs a regex directly on the live VTE,
    // outside `FindState`; always clear it before a new structured pass too.
    active_vte.search_set_regex(None::<&vte4::Regex>, 0);
}

#[cfg(test)]
mod tests {
    use super::{
        add_snapshot_jump_fallbacks, bounded_match_count, command_preview, cross_block_match_count,
        cross_block_pattern, cross_block_search_version, duration_matches,
        focus_one_native_forward_match, has_metadata_filters, matching_record_ids,
        metadata_filter_hits, native_cursor_action, outcome_matches_filters, plan_matching_windows,
        record_matches_filters, regex_consumption, snippet, step_compressed_cursor,
        unresolved_record_target_result, utf8_prefix, vte_cross_block_pattern,
        CrossBlockSearchOptions, CrossBlockSearchScope, FindCursor, FindDirection, FindScanBudget,
        FindSurface, NativeCursorAction, RecordNavigationResult, RecordSnapshotView,
        RegexConsumption, VTE_SEARCH_FLAGS,
    };
    use crate::block_view::{
        BackendRecordRef, BackendSearchWindow, BlockData, BlockFilters, CompletedCommandRecord,
        ZoneOutputSnapshot,
    };
    use std::collections::HashSet;
    use std::time::Instant;

    fn surface(count: usize, complete: bool) -> FindSurface {
        FindSurface {
            block_id: 1,
            block_index: 0,
            is_output: false,
            is_live: false,
            count,
            vte_cursor: None,
            complete,
            initial_wrap: false,
            wrap_before: complete.then_some(0),
            render_stamp: crate::block_view::blocks::NEUTRAL_RENDER_STAMP,
        }
    }

    #[test]
    fn cross_block_version_detects_same_length_retention_rotation() {
        let empty = cross_block_search_version([]);
        assert_eq!(empty.len, 0);
        assert_eq!(empty.oldest, None);
        assert_eq!(empty.newest, None);

        let before = cross_block_search_version([7, 8, 9]);
        let after = cross_block_search_version([8, 9, 10]);
        assert_eq!(before.len, after.len);
        assert_eq!(before.oldest, Some(7));
        assert_eq!(before.newest, Some(9));
        assert_eq!(after.oldest, Some(8));
        assert_eq!(after.newest, Some(10));
        assert_ne!(before, after);
    }

    #[test]
    fn a_surface_remembers_the_render_it_was_counted_against() {
        let scanned = surface(3, true);
        assert_eq!(
            scanned.render_stamp,
            crate::block_view::blocks::NEUTRAL_RENDER_STAMP,
            "a persistent/live surface is neutral"
        );

        let at_scan = crate::block_view::blocks::output_render_stamp_for_test(80, 40, 24, 7);
        for moved in [
            crate::block_view::blocks::output_render_stamp_for_test(100, 40, 24, 7),
            crate::block_view::blocks::output_render_stamp_for_test(80, 40, 5_000, 7),
            crate::block_view::blocks::output_render_stamp_for_test(80, 12, 24, 8),
        ] {
            assert_ne!(at_scan, moved);
            assert_ne!(moved, crate::block_view::blocks::NEUTRAL_RENDER_STAMP);
        }
    }

    #[test]
    fn cross_block_options_keep_scan_and_vte_whole_word_semantics_aligned() {
        let options = CrossBlockSearchOptions {
            whole_word: true,
            ..CrossBlockSearchOptions::default()
        };
        let pattern = cross_block_pattern("test", options);
        let regex = regex::RegexBuilder::new(&pattern)
            .case_insensitive(true)
            .build()
            .unwrap();
        assert_eq!(
            cross_block_match_count(&regex, "contest TEST test_testing (test)", true),
            2
        );
        let vte_pattern = vte_cross_block_pattern("test", options);
        assert_eq!(vte_pattern, r"(?<![\p{L}\p{N}_])(?:test)(?![\p{L}\p{N}_])");
        assert!(vte4::Regex::for_search(&vte_pattern, VTE_SEARCH_FLAGS).is_ok());

        let unicode_pattern = cross_block_pattern("测试", options);
        let unicode = regex::Regex::new(&unicode_pattern).unwrap();
        assert_eq!(
            cross_block_match_count(&unicode, "测试 测试版 (测试)", true),
            2
        );

        assert!(CrossBlockSearchScope::Command.includes_command());
        assert!(!CrossBlockSearchScope::Command.includes_output());
        assert!(CrossBlockSearchScope::Output.includes_output());
        assert!(!CrossBlockSearchScope::Output.includes_command());
        assert_eq!(
            CrossBlockSearchScope::from_index(99),
            CrossBlockSearchScope::All
        );
        assert_eq!(
            CrossBlockSearchScope::All.cycled(),
            CrossBlockSearchScope::Command
        );
        assert_eq!(CrossBlockSearchScope::Output.cycled().index(), 0);
    }

    #[test]
    fn an_already_selected_one_hit_surface_still_invalidates_after_refeed() {
        let at_scan = crate::block_view::blocks::output_render_stamp_for_test(80, 40, 24, 7);
        let after_refeed = crate::block_view::blocks::output_render_stamp_for_test(100, 40, 24, 7);
        let mut one_hit = surface(1, true);
        one_hit.render_stamp = at_scan;
        one_hit.vte_cursor = Some(0);

        assert_eq!(
            super::validated_native_cursor_action(&one_hit, 0, FindDirection::Next, Some(at_scan),),
            Some(NativeCursorAction::AlreadySelected)
        );
        assert_eq!(
            super::validated_native_cursor_action(
                &one_hit,
                0,
                FindDirection::Next,
                Some(after_refeed),
            ),
            None,
            "the unchanged logical edge must not hide a stale native cursor"
        );
    }

    #[test]
    fn a_cross_block_position_counts_matches_not_matching_lines() {
        let re = regex::Regex::new("ab").unwrap();
        let mut occurrence = 0usize;
        let hits = "ab\nnothing\nab ab ab\ntail ab"
            .lines()
            .enumerate()
            .filter_map(|(index, line)| {
                let matches = re.find_iter(line).count();
                let first = (matches > 0).then_some((index + 1, occurrence));
                occurrence = occurrence.saturating_add(matches);
                first
            })
            .collect::<Vec<_>>();

        assert_eq!(hits, [(1, 0), (3, 1), (4, 4)]);
        assert_eq!(occurrence, 5);
    }

    #[test]
    fn a_palette_jump_bounds_native_cursor_work() {
        assert_eq!(super::bounded_occurrence_steps(0), Some(1));
        assert_eq!(super::bounded_occurrence_steps(41), Some(42));
        assert_eq!(super::bounded_occurrence_steps(4_095), Some(4_096));
        assert_eq!(super::bounded_occurrence_steps(4_096), None);
        assert_eq!(super::bounded_occurrence_steps(usize::MAX), None);
    }

    #[test]
    fn a_palette_jump_fails_when_any_native_step_is_exhausted() {
        let mut outcomes = [true, false, true].into_iter();
        assert!(!super::step_to_occurrence_exact(2, || outcomes
            .next()
            .unwrap_or(false)));
        assert_eq!(
            outcomes.next(),
            Some(true),
            "the exact jump stops at the first miss instead of claiming success"
        );
    }

    #[test]
    fn unified_window_count_prefers_viewport_before_matching_old_history() {
        let regex = regex::Regex::new("needle").unwrap();
        let windows = [
            BackendSearchWindow {
                text: "visible needle\n".to_string(),
                incomplete: true,
                initial_wrap: false,
            },
            BackendSearchWindow {
                text: format!("old needle\n{}", "old filler\n".repeat(100_000)),
                incomplete: true,
                initial_wrap: true,
            },
        ];
        let plan = plan_matching_windows(&regex, &windows, super::FIND_MATCH_LIMIT).unwrap();
        assert_eq!(plan.count, 1, "old history must not consume the scan");
        assert!(plan.incomplete);
        assert!(!plan.initial_wrap);
        assert_eq!(plan.wrap_before, None);
    }

    #[test]
    fn unified_complete_windows_restore_exact_whole_domain_navigation() {
        let regex = regex::Regex::new("needle").unwrap();
        let windows = [
            BackendSearchWindow {
                text: "visible needle\n".to_string(),
                incomplete: false,
                initial_wrap: false,
            },
            BackendSearchWindow {
                text: "old needle\n".to_string(),
                incomplete: false,
                initial_wrap: true,
            },
        ];
        let plan = plan_matching_windows(&regex, &windows, super::FIND_MATCH_LIMIT).unwrap();
        assert_eq!(plan.count, 2);
        assert!(!plan.incomplete);
        assert!(!plan.initial_wrap);
        assert_eq!(plan.wrap_before, Some(1));
    }

    #[test]
    fn native_cursor_wraps_only_at_the_unified_viewport_history_boundary() {
        let mut unified = surface(2, true);
        unified.is_live = true;
        unified.block_id = 0;
        unified.wrap_before = Some(1);

        assert_eq!(
            native_cursor_action(&unified, 0, FindDirection::Next),
            Some(NativeCursorAction::Step { wrap_once: false })
        );
        unified.vte_cursor = Some(0);
        assert_eq!(
            native_cursor_action(&unified, 1, FindDirection::Next),
            Some(NativeCursorAction::Step { wrap_once: true })
        );
        unified.vte_cursor = Some(1);
        assert_eq!(
            native_cursor_action(&unified, 0, FindDirection::Next),
            Some(NativeCursorAction::Step { wrap_once: false })
        );
        assert_eq!(
            native_cursor_action(&unified, 0, FindDirection::Previous),
            Some(NativeCursorAction::Step { wrap_once: true })
        );
    }

    fn step(
        surfaces: &[FindSurface],
        cursor: FindCursor,
        total: usize,
        capped: bool,
        direction: FindDirection,
    ) -> FindCursor {
        step_compressed_cursor(surfaces, cursor, total, capped, direction)
            .expect("valid compressed cursor")
            .0
    }

    #[test]
    fn bounded_counter_stops_at_the_match_limit() {
        let regex = regex::Regex::new(".").unwrap();
        let counted = bounded_match_count(&regex, &"x".repeat(20_000), 10_000);
        assert_eq!(counted.count, 10_000);
        assert!(counted.reached_limit);
    }

    #[test]
    fn vte_search_uses_unicode_properties_like_the_rust_counter() {
        assert_ne!(VTE_SEARCH_FLAGS & pcre2_sys::PCRE2_UTF, 0);
        assert_ne!(VTE_SEARCH_FLAGS & pcre2_sys::PCRE2_UCP, 0);

        // Arabic-Indic digits are the common regression: Rust's default `\d`
        // counts them, while bare PCRE2 shorthand classes are ASCII-only.
        assert!(regex::Regex::new(r"\d").unwrap().is_match("١"));
        assert!(vte4::Regex::for_search(r"\d", VTE_SEARCH_FLAGS).is_ok());
    }

    #[test]
    fn zero_width_regexes_are_rejected_before_vte_and_consuming_anchors_are_allowed() {
        for pattern in [r"^", r"$", r"\b", r"a*", r"(?:x)?"] {
            assert_eq!(
                regex_consumption(pattern).unwrap(),
                RegexConsumption::ZeroWidth,
                "{pattern}"
            );
        }
        assert_eq!(
            regex_consumption(r"^foo").unwrap(),
            RegexConsumption::Consuming
        );
    }

    #[test]
    fn utf8_scan_prefix_never_splits_a_code_point() {
        assert_eq!(utf8_prefix("ab界cd", 4), "ab");
        assert_eq!(utf8_prefix("ab界cd", 5), "ab界");
        assert_eq!(utf8_prefix("ab界cd", usize::MAX), "ab界cd");
    }

    #[test]
    fn aggregate_scan_budget_reports_an_incomplete_utf8_safe_prefix() {
        let mut budget = FindScanBudget {
            remaining_bytes: 5,
            started: Instant::now(),
        };
        let first = budget.take_prefix("abc");
        assert_eq!(first.text, "abc");
        assert!(!first.incomplete);

        let second = budget.take_prefix("界z");
        assert_eq!(second.text, "");
        assert!(second.incomplete);
        assert_eq!(budget.remaining_bytes(), 2);
    }

    #[test]
    fn native_cursor_plan_tracks_boundaries_without_resetting_regex() {
        let mut complete = surface(3, true);
        assert_eq!(
            native_cursor_action(&complete, 0, FindDirection::Next),
            Some(NativeCursorAction::Step { wrap_once: false })
        );
        complete.vte_cursor = Some(0);
        assert_eq!(
            native_cursor_action(&complete, 0, FindDirection::Previous),
            Some(NativeCursorAction::AlreadySelected)
        );
        assert_eq!(
            native_cursor_action(&complete, 2, FindDirection::Previous),
            Some(NativeCursorAction::Step { wrap_once: true })
        );

        let mut incomplete = surface(3, false);
        incomplete.vte_cursor = Some(2);
        assert_eq!(
            native_cursor_action(&incomplete, 0, FindDirection::Next),
            None
        );
    }

    #[test]
    fn compressed_navigation_preserves_surface_order_and_direction_reversal() {
        let surfaces = [surface(2, true), surface(3, true)];
        let mut cursor = FindCursor::default();

        cursor = step(&surfaces, cursor, 5, false, FindDirection::Next);
        assert_eq!(
            (cursor.surface, cursor.occurrence, cursor.global),
            (0, 1, 1)
        );
        cursor = step(&surfaces, cursor, 5, false, FindDirection::Next);
        assert_eq!(
            (cursor.surface, cursor.occurrence, cursor.global),
            (1, 0, 2)
        );
        cursor = step(&surfaces, cursor, 5, false, FindDirection::Previous);
        assert_eq!(
            (cursor.surface, cursor.occurrence, cursor.global),
            (0, 1, 1)
        );
        cursor = step(&surfaces, cursor, 5, false, FindDirection::Next);
        assert_eq!(
            (cursor.surface, cursor.occurrence, cursor.global),
            (1, 0, 2)
        );
    }

    #[test]
    fn exact_navigation_wraps_but_capped_navigation_stops_at_both_edges() {
        let exact = [surface(2, true)];
        let last = FindCursor {
            surface: 0,
            occurrence: 1,
            global: 1,
        };
        assert_eq!(
            step(&exact, last, 2, false, FindDirection::Next),
            FindCursor::default()
        );
        assert_eq!(
            step(
                &exact,
                FindCursor::default(),
                2,
                false,
                FindDirection::Previous,
            ),
            last
        );

        let capped = [surface(2, true), surface(2, false)];
        let capped_last = FindCursor {
            surface: 1,
            occurrence: 1,
            global: 3,
        };
        assert_eq!(
            step(&capped, capped_last, 4, true, FindDirection::Next),
            capped_last
        );
        assert_eq!(
            step(
                &capped,
                FindCursor::default(),
                4,
                true,
                FindDirection::Previous,
            ),
            FindCursor::default()
        );
    }

    #[test]
    fn unified_whole_surface_cursor_steps_across_record_boundaries() {
        // Unified paints all zones into one VTE, hence one native cursor
        // domain. Model three chronological records whose query appears only
        // in the latter two; splitting them into two pseudo-surfaces would
        // reset/rewrap the same native VTE cursor at the artificial boundary.
        let screen = "first record\nlater record: needle\nlatest record: needle\n";
        let regex = regex::RegexBuilder::new("needle")
            .case_insensitive(true)
            .build()
            .unwrap();
        let count = bounded_match_count(&regex, screen, super::FIND_MATCH_LIMIT).count;
        assert_eq!(count, 2);

        let surfaces = [surface(count, true)];
        let second = step(
            &surfaces,
            FindCursor::default(),
            count,
            false,
            FindDirection::Next,
        );
        assert_eq!(
            (second.surface, second.occurrence, second.global),
            (0, 1, 1)
        );
        assert_eq!(
            step(&surfaces, second, count, false, FindDirection::Previous,),
            FindCursor::default()
        );
    }

    #[test]
    fn unified_first_native_step_wraps_from_the_reset_live_cursor() {
        let mut unified = surface(2, true);
        unified.is_live = true;
        unified.block_id = 0;
        unified.initial_wrap = true;
        assert_eq!(
            native_cursor_action(&unified, 0, FindDirection::Next),
            Some(NativeCursorAction::Step { wrap_once: true })
        );

        let block = surface(2, true);
        assert_eq!(
            native_cursor_action(&block, 0, FindDirection::Next),
            Some(NativeCursorAction::Step { wrap_once: false })
        );
    }

    /// VTE keeps its search anchor/selection across regex changes. Unified
    /// counts from the oldest retained row, so a fresh query must clear that
    /// state and wrap once from the live cursor at the bottom. This display-
    /// backed regression exercises the real native cursor rather than the
    /// compressed model above.
    #[test]
    #[ignore = "requires DISPLAY"]
    fn unified_vte_fresh_query_reaches_scrollback_before_a_prior_match() {
        use gtk::prelude::*;
        use relm4::gtk;
        use std::time::Duration;
        use vte4::TerminalExt;

        gtk::init().expect("gtk init");
        let terminal = vte4::Terminal::new();
        terminal.set_size(24, 4);
        terminal.set_scrollback_lines(256);
        let window = gtk::Window::new();
        window.set_child(Some(&terminal));
        window.present();
        terminal.feed(b"bar-oldest\r\n");
        for index in 0..32 {
            terminal.feed(format!("filler-{index:02}\r\n").as_bytes());
        }
        terminal.feed(b"foo-latest\r\n");
        let context = gtk::glib::MainContext::default();
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(100) {
            while context.iteration(false) {}
            std::thread::sleep(Duration::from_millis(2));
        }

        let foo = vte4::Regex::for_search("foo-latest", VTE_SEARCH_FLAGS).unwrap();
        terminal.unselect_all();
        terminal.search_set_regex(Some(&foo), 0);
        terminal.search_set_wrap_around(true);
        assert!(terminal.search_find_next());

        let bar = vte4::Regex::for_search("bar-oldest", VTE_SEARCH_FLAGS).unwrap();
        terminal.search_set_regex(None::<&vte4::Regex>, 0);
        terminal.unselect_all();
        terminal.search_set_regex(Some(&bar), 0);
        terminal.search_set_wrap_around(true);
        assert!(
            terminal.search_find_next(),
            "the fresh query must wrap from the bottom into retained scrollback"
        );
        let selected = terminal
            .text_selected(vte4::Format::Text)
            .map(|text| text.to_string())
            .unwrap_or_default();
        assert_eq!(selected, "bar-oldest");
        window.close();
        while context.iteration(false) {}
    }

    /// A huge old scrollback must not consume the structured search budget
    /// before a visible hit, and unknown row projection uses the same
    /// viewport-forward native fallback with a single capped result.
    #[test]
    #[ignore = "requires DISPLAY"]
    fn unified_bounded_and_native_fallback_prefer_visible_match_with_huge_old_scrollback() {
        use gtk::prelude::*;
        use relm4::gtk;
        use std::time::Duration;
        use vte4::TerminalExt;

        gtk::init().expect("gtk init");
        let terminal = vte4::Terminal::new();
        terminal.set_size(64, 4);
        terminal.set_scrollback_lines(80_000);
        let window = gtk::Window::new();
        window.set_child(Some(&terminal));
        window.present();

        let mut transcript = Vec::with_capacity(1_500_000);
        transcript.extend_from_slice(b"needle-old\r\n");
        for _ in 0..70_000 {
            transcript.extend_from_slice(b"filler-history-row\r\n");
        }
        transcript.extend_from_slice(b"needle-visible");
        terminal.feed(&transcript);
        let context = gtk::glib::MainContext::default();
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(250) {
            while context.iteration(false) {}
            std::thread::sleep(Duration::from_millis(2));
        }

        let regex = vte4::Regex::for_search("needle-(?:old|visible)", VTE_SEARCH_FLAGS).unwrap();
        terminal.unselect_all();
        terminal.search_set_regex(Some(&regex), 0);
        let mut bounded_surface = surface(1, false);
        bounded_surface.is_live = true;
        bounded_surface.block_id = 0;
        let Some(NativeCursorAction::Step { wrap_once }) =
            native_cursor_action(&bounded_surface, 0, FindDirection::Next)
        else {
            panic!("the first bounded native action must step")
        };
        assert!(!wrap_once);
        terminal.search_set_wrap_around(wrap_once);
        assert!(terminal.search_find_next());
        let selected = terminal
            .text_selected(vte4::Format::Text)
            .map(|text| text.to_string())
            .unwrap_or_default();
        assert_eq!(selected, "needle-visible");

        assert!(focus_one_native_forward_match(&terminal, &regex));
        let selected = terminal
            .text_selected(vte4::Format::Text)
            .map(|text| text.to_string())
            .unwrap_or_default();
        assert_eq!(selected, "needle-visible");
        window.close();
        while context.iteration(false) {}
    }

    #[test]
    #[ignore = "requires DISPLAY"]
    fn unified_complete_windows_step_visible_then_wrapped_history_on_real_vte() {
        use gtk::prelude::*;
        use relm4::gtk;
        use std::time::Duration;
        use vte4::TerminalExt;

        gtk::init().expect("gtk init");
        let terminal = vte4::Terminal::new();
        terminal.set_size(32, 4);
        terminal.set_scrollback_lines(256);
        let window = gtk::Window::new();
        window.set_child(Some(&terminal));
        window.present();
        terminal.feed(b"needle-old\r\n");
        for _ in 0..32 {
            terminal.feed(b"filler\r\n");
        }
        terminal.feed(b"needle-visible");
        let context = gtk::glib::MainContext::default();
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(100) {
            while context.iteration(false) {}
            std::thread::sleep(Duration::from_millis(2));
        }

        let regex = vte4::Regex::for_search("needle-(?:old|visible)", VTE_SEARCH_FLAGS).unwrap();
        terminal.unselect_all();
        terminal.search_set_regex(Some(&regex), 0);
        let mut surface = surface(2, true);
        surface.is_live = true;
        surface.block_id = 0;
        surface.wrap_before = Some(1);

        for (occurrence, expected, expected_wrap) in
            [(0, "needle-visible", false), (1, "needle-old", true)]
        {
            let Some(NativeCursorAction::Step { wrap_once }) =
                native_cursor_action(&surface, occurrence, FindDirection::Next)
            else {
                panic!("native occurrence {occurrence} must step")
            };
            assert_eq!(wrap_once, expected_wrap);
            terminal.search_set_wrap_around(wrap_once);
            assert!(terminal.search_find_next());
            let selected = terminal
                .text_selected(vte4::Format::Text)
                .map(|text| text.to_string())
                .unwrap_or_default();
            assert_eq!(selected, expected);
            surface.vte_cursor = Some(occurrence);
        }
        window.close();
        while context.iteration(false) {}
    }

    #[test]
    fn unknown_duration_does_not_match_duration_filters() {
        let filters = BlockFilters {
            slow_only: true,
            slow_threshold_ms: 1_000,
            ..Default::default()
        };
        assert!(has_metadata_filters(&filters));
        assert!(has_metadata_filters(&BlockFilters {
            background_only: true,
            ..Default::default()
        }));
        assert!(!has_metadata_filters(&BlockFilters::default()));
        assert!(!duration_matches(None, &filters));
    }

    #[test]
    fn duration_boundaries_are_inclusive() {
        let filters = BlockFilters {
            min_duration_ms: Some(500),
            max_duration_ms: Some(1_500),
            ..Default::default()
        };
        assert!(duration_matches(Some(500), &filters));
        assert!(duration_matches(Some(1_500), &filters));
        assert!(!duration_matches(Some(499), &filters));
        assert!(!duration_matches(Some(1_501), &filters));
    }

    #[test]
    fn duration_is_irrelevant_without_duration_predicates() {
        assert!(duration_matches(None, &BlockFilters::default()));
    }

    #[test]
    fn background_classification_is_backend_neutral_and_ignores_raw_failure_status() {
        let block = BlockData {
            id: 1,
            prompt: String::new(),
            cmd: " \t".to_string(),
            cmd_markup: None,
            output: "block background".to_string(),
            exit_code: Some(7),
            lifecycle_schema: crate::block_view::blocks::BLOCK_LIFECYCLE_SCHEMA,
            completion_provenance: super::super::CompletionProvenance::Unknown.into(),
            start_mark_seen: false,
            estimated_height: 1,
            line_count: 1,
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: Some(2_000),
            cwd: None,
            cols: 80,
        };
        let metadata = CompletedCommandRecord {
            id: 2,
            // Metadata's explicit bit is authoritative even for a defensive
            // contradictory fixture; search must not relabel it from text.
            cmd: "legacy payload".to_string(),
            exit_code: Some(9),
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: Some(2_000),
            cwd: None,
            is_background: true,
            completion_provenance: super::super::CompletionProvenance::Unknown,
            start_mark_seen: false,
        };
        let records = [
            BackendRecordRef::Block(&block),
            BackendRecordRef::Metadata {
                record: &metadata,
                snapshot: None,
            },
        ];
        let background = BlockFilters {
            background_only: true,
            ..Default::default()
        };
        let failed = BlockFilters {
            failed_only: true,
            ..Default::default()
        };
        let exact = BlockFilters {
            exit_code: Some(7),
            ..Default::default()
        };
        let slow_background = BlockFilters {
            slow_only: true,
            slow_threshold_ms: 1_000,
            background_only: true,
            ..Default::default()
        };
        let bounded_background = BlockFilters {
            min_duration_ms: Some(1),
            max_duration_ms: Some(3_000),
            background_only: true,
            ..Default::default()
        };

        for record in records {
            assert!(record.is_background());
            assert_eq!(record.command(), "");
            assert_eq!(record.exit_code(), None);
            assert_eq!(record.duration_ms(), None);
            assert!(record_matches_filters(record, &background));
            assert!(!record_matches_filters(record, &failed));
            assert!(!record_matches_filters(record, &exact));
            assert!(!record_matches_filters(record, &slow_background));
            assert!(!record_matches_filters(record, &bounded_background));
        }

        let retained = ZoneOutputSnapshot {
            plain: "\nactual background output".to_string(),
            truncated: false,
        };
        let metadata_record = || {
            [BackendRecordRef::Metadata {
                record: &metadata,
                snapshot: Some(&retained),
            }]
        };
        assert!(metadata_filter_hits(
            metadata_record(),
            CrossBlockSearchScope::Command,
            1,
            &background,
        )
        .is_empty());
        let hits = metadata_filter_hits(
            metadata_record(),
            CrossBlockSearchScope::All,
            1,
            &background,
        );
        assert_eq!(hits.len(), 1);
        assert!(hits[0].is_output);
        assert_eq!(hits[0].line_text, "actual background output");
        assert_eq!(hits[0].cmd_preview, "");
        assert_eq!(hits[0].exit_code, None);
        assert_eq!(hits[0].duration_ms, None);
    }

    #[test]
    fn explicit_foreground_metadata_identity_survives_an_empty_legacy_command() {
        let metadata = CompletedCommandRecord {
            id: 8,
            // The explicit Unified identity is authoritative in this direction
            // too: missing legacy command text must not erase a real lifecycle.
            cmd: String::new(),
            exit_code: Some(7),
            start_time_ms: Some(10),
            end_time_ms: Some(2_010),
            duration_ms: Some(2_000),
            cwd: Some("/srv/legacy".to_string()),
            is_background: false,
            completion_provenance: super::super::CompletionProvenance::ShellReported,
            start_mark_seen: true,
        };
        let retained = ZoneOutputSnapshot {
            plain: "\nlegacy foreground output".to_string(),
            truncated: false,
        };
        let record = BackendRecordRef::Metadata {
            record: &metadata,
            snapshot: Some(&retained),
        };
        let background = BlockFilters {
            background_only: true,
            ..Default::default()
        };
        let exact = BlockFilters {
            exit_code: Some(7),
            ..Default::default()
        };
        let failed = BlockFilters {
            failed_only: true,
            ..Default::default()
        };
        let slow = BlockFilters {
            slow_only: true,
            slow_threshold_ms: 1_000,
            ..Default::default()
        };

        assert!(!record.is_background());
        assert_eq!(record.command(), "");
        assert_eq!(record.exit_code(), Some(7));
        assert_eq!(record.duration_ms(), Some(2_000));
        assert!(!record_matches_filters(record, &background));
        assert!(record_matches_filters(record, &exact));
        assert!(record_matches_filters(record, &failed));
        assert!(record_matches_filters(record, &slow));

        assert!(
            metadata_filter_hits([record], CrossBlockSearchScope::Command, 1, &failed,).is_empty()
        );
        for scope in [CrossBlockSearchScope::All, CrossBlockSearchScope::Output] {
            let hits = metadata_filter_hits([record], scope, 1, &failed);
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].block_id, 8);
            assert!(hits[0].is_output);
            assert_eq!(hits[0].line_no, 2);
            assert_eq!(hits[0].line_text, "legacy foreground output");
            assert_eq!(hits[0].cmd_preview, "");
            assert_eq!(hits[0].exit_code, Some(7));
            assert_eq!(hits[0].duration_ms, Some(2_000));
            assert_eq!(hits[0].cwd.as_deref(), Some("/srv/legacy"));
        }
    }

    #[test]
    fn metadata_filters_return_stable_ids_and_unresolved_targets_fail_closed() {
        let metadata = [
            CompletedCommandRecord {
                id: 91,
                cmd: "false".to_string(),
                exit_code: Some(1),
                start_time_ms: None,
                end_time_ms: None,
                duration_ms: Some(50),
                cwd: None,
                is_background: false,
                completion_provenance: super::super::CompletionProvenance::ShellReported,
                start_mark_seen: true,
            },
            CompletedCommandRecord {
                id: 7,
                cmd: "sleep 2".to_string(),
                exit_code: Some(0),
                start_time_ms: None,
                end_time_ms: None,
                duration_ms: Some(2_000),
                cwd: None,
                is_background: false,
                completion_provenance: super::super::CompletionProvenance::ShellReported,
                start_mark_seen: true,
            },
        ];
        let records = || {
            metadata.iter().map(|record| BackendRecordRef::Metadata {
                record,
                snapshot: None,
            })
        };
        let failed = BlockFilters {
            failed_only: true,
            ..Default::default()
        };
        let slow = BlockFilters {
            slow_only: true,
            slow_threshold_ms: 1_000,
            ..Default::default()
        };

        assert_eq!(matching_record_ids(records(), "", &failed), [91]);
        assert_eq!(matching_record_ids(records(), "", &slow), [7]);
        assert_eq!(
            unresolved_record_target_result(records(), 91),
            RecordNavigationResult::LocationUnavailable
        );
        assert_eq!(
            unresolved_record_target_result(records(), 999),
            RecordNavigationResult::NoMatchingRecord
        );
    }

    #[test]
    fn cross_block_metadata_filters_compose_before_the_result_cap() {
        let record = |id, exit_code, duration_ms| CompletedCommandRecord {
            id,
            cmd: "needle".to_string(),
            exit_code,
            start_time_ms: None,
            end_time_ms: None,
            duration_ms,
            cwd: Some("/tmp".to_string()),
            is_background: false,
            completion_provenance: super::super::CompletionProvenance::ShellReported,
            start_mark_seen: true,
        };
        let metadata = [
            record(1, Some(0), Some(2_000)),
            record(2, Some(7), Some(20)),
            record(3, Some(9), Some(2_000)),
        ];
        let filters = BlockFilters {
            failed_only: true,
            slow_only: true,
            slow_threshold_ms: 1_000,
            ..Default::default()
        };

        let records = || {
            metadata.iter().map(|record| BackendRecordRef::Metadata {
                record,
                snapshot: None,
            })
        };
        let hits = metadata_filter_hits(records(), CrossBlockSearchScope::All, 1, &filters);
        assert_eq!(
            hits.iter().map(|hit| hit.block_id).collect::<Vec<_>>(),
            [3],
            "ineligible records cannot spend a one-hit search budget"
        );
        let hit = &hits[0];
        assert_eq!(hit.block_id, 3);
        assert_eq!(hit.line_text, "needle");
        assert_eq!(hit.exit_code, Some(9));
        assert_eq!(hit.duration_ms, Some(2_000));
        assert_eq!(hit.cwd.as_deref(), Some("/tmp"));
        assert!(
            metadata_filter_hits(records(), CrossBlockSearchScope::Output, 1, &filters).is_empty()
        );
    }

    #[test]
    fn background_filter_composes_before_cap_and_uses_only_real_scoped_text() {
        let record = |id, cmd: &str, duration_ms, is_background| CompletedCommandRecord {
            id,
            cmd: cmd.to_string(),
            exit_code: None,
            start_time_ms: None,
            end_time_ms: None,
            duration_ms,
            cwd: None,
            is_background,
            completion_provenance: super::super::CompletionProvenance::Unknown,
            start_mark_seen: !is_background,
        };
        let metadata = [
            record(1, "foreground one", Some(2_000), false),
            record(2, "foreground two", Some(20), false),
            record(3, "", None, true),
            record(4, "", None, true),
        ];
        let foreground_output = ZoneOutputSnapshot {
            plain: "foreground output".to_string(),
            truncated: false,
        };
        let second_foreground_output = ZoneOutputSnapshot {
            plain: "second foreground output".to_string(),
            truncated: false,
        };
        let retained_output = ZoneOutputSnapshot {
            plain: "\nretained background".to_string(),
            truncated: false,
        };
        let records = || {
            [
                BackendRecordRef::Metadata {
                    record: &metadata[0],
                    snapshot: Some(&foreground_output),
                },
                BackendRecordRef::Metadata {
                    record: &metadata[1],
                    snapshot: Some(&second_foreground_output),
                },
                BackendRecordRef::Metadata {
                    record: &metadata[2],
                    snapshot: None,
                },
                BackendRecordRef::Metadata {
                    record: &metadata[3],
                    snapshot: Some(&retained_output),
                },
            ]
            .into_iter()
        };
        let filters = BlockFilters {
            background_only: true,
            ..Default::default()
        };

        for scope in [CrossBlockSearchScope::All, CrossBlockSearchScope::Output] {
            let hits = metadata_filter_hits(records(), scope, 1, &filters);
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].block_id, 4);
            assert!(hits[0].is_output);
            assert_eq!(hits[0].line_no, 2);
            assert_eq!(hits[0].line_text, "retained background");
        }
        assert!(
            metadata_filter_hits(records(), CrossBlockSearchScope::Command, 10, &filters)
                .is_empty()
        );
        assert!(metadata_filter_hits(
            [BackendRecordRef::Metadata {
                record: &metadata[2],
                snapshot: None,
            }],
            CrossBlockSearchScope::All,
            10,
            &filters,
        )
        .is_empty());

        let block = BlockData {
            id: 5,
            prompt: String::new(),
            cmd: String::new(),
            cmd_markup: None,
            output: "retained block background".to_string(),
            exit_code: None,
            lifecycle_schema: crate::block_view::blocks::BLOCK_LIFECYCLE_SCHEMA,
            completion_provenance: super::super::CompletionProvenance::Unknown.into(),
            start_mark_seen: false,
            estimated_height: 1,
            line_count: 1,
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: None,
            cwd: None,
            cols: 80,
        };
        let block_record = || [BackendRecordRef::Block(&block)];
        let hits = metadata_filter_hits(block_record(), CrossBlockSearchScope::All, 1, &filters);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].is_output);
        assert_eq!(hits[0].line_text, "retained block background");
        let command_hits =
            metadata_filter_hits(block_record(), CrossBlockSearchScope::Command, 1, &filters);
        assert!(command_hits.is_empty());

        let background_and_slow = BlockFilters {
            background_only: true,
            slow_only: true,
            slow_threshold_ms: 1_000,
            ..Default::default()
        };
        assert!(metadata_filter_hits(
            block_record(),
            CrossBlockSearchScope::All,
            1,
            &background_and_slow,
        )
        .is_empty());
    }

    /// A retained snapshot makes a metadata record searchable by its output;
    /// budget eviction demotes the same record to command-only matching, and
    /// navigation falls back from the snapshot view to the honest notice.
    #[test]
    fn metadata_records_match_by_snapshot_output_until_it_is_evicted() {
        let record = |id: u64, cmd: &str| CompletedCommandRecord {
            id,
            cmd: cmd.to_string(),
            exit_code: Some(0),
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: None,
            cwd: None,
            is_background: false,
            completion_provenance: super::super::CompletionProvenance::ShellReported,
            start_mark_seen: true,
        };
        let with_snapshot = record(1, "cargo test");
        let evicted = record(2, "rg needle src");
        let snapshot = ZoneOutputSnapshot {
            plain: "error: found needle in haystack".to_string(),
            truncated: false,
        };
        let records = || {
            [
                BackendRecordRef::Metadata {
                    record: &with_snapshot,
                    snapshot: Some(&snapshot),
                },
                BackendRecordRef::Metadata {
                    record: &evicted,
                    snapshot: None,
                },
            ]
            .into_iter()
        };

        assert_eq!(
            matching_record_ids(records(), "needle", &BlockFilters::default()),
            [1, 2],
            "id 1 matches by snapshot output, id 2 by command only"
        );
        assert_eq!(
            matching_record_ids(records(), "haystack", &BlockFilters::default()),
            [1],
            "the evicted record no longer matches by output content"
        );

        assert_eq!(
            unresolved_record_target_result(records(), 1),
            RecordNavigationResult::SnapshotView { record_id: 1 }
        );
        assert_eq!(
            unresolved_record_target_result(records(), 2),
            RecordNavigationResult::LocationUnavailable
        );
    }

    #[test]
    fn batched_jumpability_preserves_only_retained_snapshot_fallbacks() {
        let with_snapshot = CompletedCommandRecord {
            id: 1,
            cmd: "cargo test".to_string(),
            exit_code: Some(0),
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: None,
            cwd: None,
            is_background: false,
            completion_provenance: super::super::CompletionProvenance::ShellReported,
            start_mark_seen: true,
        };
        let evicted = CompletedCommandRecord {
            id: 2,
            cmd: "rg needle src".to_string(),
            ..with_snapshot.clone()
        };
        let snapshot = ZoneOutputSnapshot {
            plain: "retained output".to_string(),
            truncated: false,
        };
        let records = [
            BackendRecordRef::Metadata {
                record: &with_snapshot,
                snapshot: Some(&snapshot),
            },
            BackendRecordRef::Metadata {
                record: &evicted,
                snapshot: None,
            },
        ];
        let candidates = HashSet::from([(1, false), (1, true), (2, true), (9, false)]);
        let mut jumpable = HashSet::new();

        add_snapshot_jump_fallbacks(records, &candidates, &mut jumpable);

        assert_eq!(jumpable, HashSet::from([(1, false), (1, true)]));
    }

    #[test]
    fn snapshot_view_truncation_note_states_the_per_zone_bound() {
        let view = RecordSnapshotView {
            cmd: "cat big.log".to_string(),
            exit_code: Some(0),
            duration_ms: None,
            is_background: false,
            output: "tail".to_string(),
            truncated: true,
        };
        assert_eq!(
            view.truncation_note().as_deref(),
            Some("Output truncated to the last 64 KiB.")
        );
        assert_eq!(
            RecordSnapshotView {
                truncated: false,
                ..view
            }
            .truncation_note(),
            None
        );
    }

    /// The snapshot view's header states the record's own outcome; an
    /// unreported status must never read as a success.
    #[test]
    fn snapshot_view_status_line_states_the_recorded_outcome() {
        let view = |exit_code, duration_ms, is_background| RecordSnapshotView {
            cmd: "cargo test".to_string(),
            exit_code,
            duration_ms,
            is_background,
            output: "ok".to_string(),
            truncated: false,
        };
        assert_eq!(
            view(Some(0), Some(1_500), false).status_line(),
            "Exit code 0 · 1.5s"
        );
        assert_eq!(
            view(None, None, false).status_line(),
            "Exit code unknown (the shell reported none)"
        );
        assert_eq!(
            view(None, Some(250), true).status_line(),
            "Background output · 250ms"
        );
    }

    #[test]
    fn outcome_filters_ignore_raw_status_on_background_output() {
        let exact = BlockFilters {
            exit_code: Some(7),
            ..Default::default()
        };
        let failed = BlockFilters {
            failed_only: true,
            ..Default::default()
        };

        assert!(!outcome_matches_filters("", Some(7), &exact));
        assert!(!outcome_matches_filters("\t ", Some(7), &failed));
        assert!(outcome_matches_filters("false", Some(7), &exact));
        assert!(outcome_matches_filters("false", Some(7), &failed));
    }

    #[test]
    fn command_without_a_reported_status_matches_neither_exit_filter() {
        let exact_success = BlockFilters {
            exit_code: Some(0),
            ..Default::default()
        };
        let failed = BlockFilters {
            failed_only: true,
            ..Default::default()
        };

        assert!(!outcome_matches_filters("cargo test", None, &exact_success));
        assert!(!outcome_matches_filters("cargo test", None, &failed));
    }

    #[test]
    fn snippet_passes_through_short_line() {
        assert_eq!(snippet("hello world"), "hello world");
    }

    #[test]
    fn snippet_truncates_long_line_with_ellipsis() {
        let long: String = "a".repeat(500);
        let out = snippet(&long);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().filter(|&c| c == 'a').count(), 240);
    }

    #[test]
    fn snippet_truncates_cjk_and_emoji_on_char_boundaries() {
        for line in [
            format!("a{}", "界".repeat(240)),
            format!("a{}", "🙂".repeat(240)),
        ] {
            let out = snippet(&line);
            assert!(out.ends_with('…'));
            assert_eq!(out.chars().count(), 241);
            assert_eq!(
                out.chars().take(240).collect::<String>(),
                line.chars().take(240).collect::<String>()
            );
        }
    }

    #[test]
    fn command_preview_bounds_long_first_line_before_hits_clone_it() {
        let command = format!("{}\nignored second line", "x".repeat(256 * 1024));
        let preview = command_preview(&command);

        assert_eq!(preview.chars().count(), 241);
        assert!(preview.ends_with('…'));
        assert!(!preview.contains("ignored second line"));
    }
}
