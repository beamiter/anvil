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

fn find_progress(state: &FindState) -> Option<FindProgress> {
    (!state.surfaces.is_empty() && state.total > 0).then_some(FindProgress {
        current: state.cursor.global + 1,
        total: state.total,
        capped: state.capped,
        scan_limited: state.scan_limited,
    })
}

/// One result row from the built-in cross-block substring/regex scan. Carries enough
/// context for a flat result list — block id (for jump), surface flag (so
/// the per-block VTE search cursor goes to the right widget), the 1-based
/// line number inside that surface, the line snippet itself (trimmed/
/// truncated for display), and a one-line cmd preview for context.
#[derive(Clone, Debug)]
pub struct CrossBlockHit {
    pub block_id: u64,
    pub is_output: bool,
    pub line_no: usize,
    pub line_text: String,
    pub cmd_preview: String,
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
            if !text_match
                || !outcome_matches_filters(command, record.exit_code(), filters)
                || !duration_matches(record.duration_ms(), filters)
            {
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
        let wrap_once = match native_cursor_action(&surface, occurrence, direction) {
            Some(NativeCursorAction::AlreadySelected) => return true,
            Some(NativeCursorAction::Step { wrap_once }) => wrap_once,
            None => return false,
        };

        let vte = if surface.is_live && surface.block_id == 0 {
            self.active_vte.clone()
        } else {
            let Some(target) = self
                .render_backend
                .record_search_target(surface.block_id, surface.is_output)
            else {
                return false;
            };
            target.terminal
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
    /// Errors only on invalid regex; an empty pattern returns `Ok(vec![])`
    /// so the caller can clear results without a special branch.
    pub fn cross_block_search(
        &self,
        pattern: &str,
        is_regex: bool,
        max_hits: usize,
    ) -> Result<Vec<CrossBlockHit>, String> {
        if pattern.is_empty() {
            return Ok(Vec::new());
        }
        let compiled_pattern = if is_regex {
            pattern.to_string()
        } else {
            regex::escape(pattern)
        };
        let re = regex::RegexBuilder::new(&compiled_pattern)
            .case_insensitive(true)
            .multi_line(true)
            .build()
            .map_err(|e| format!("{e}"))?;

        let records = self.render_backend.records();
        let mut hits: Vec<CrossBlockHit> = Vec::new();

        for record in records.iter() {
            if hits.len() >= max_hits {
                break;
            }
            let command = record.command();
            let cmd_preview = command_preview(command);

            // Cmd surface — usually 1 line, but multiline commands exist.
            for (ln_idx, line) in command.lines().enumerate() {
                if hits.len() >= max_hits {
                    break;
                }
                if re.is_match(line) {
                    hits.push(CrossBlockHit {
                        block_id: record.id(),
                        is_output: false,
                        line_no: ln_idx + 1,
                        line_text: snippet(line),
                        cmd_preview: cmd_preview.clone(),
                    });
                }
            }

            for (ln_idx, line) in record.output().unwrap_or("").lines().enumerate() {
                if hits.len() >= max_hits {
                    break;
                }
                if re.is_match(line) {
                    hits.push(CrossBlockHit {
                        block_id: record.id(),
                        is_output: true,
                        line_no: ln_idx + 1,
                        line_text: snippet(line),
                        cmd_preview: cmd_preview.clone(),
                    });
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
        is_regex: bool,
        is_output: bool,
    ) -> bool {
        if pattern.is_empty() {
            return false;
        }
        let compiled = if is_regex {
            pattern.to_string()
        } else {
            regex::escape(pattern)
        };
        let Ok(vte_re) = vte4::Regex::for_search(&compiled, VTE_SEARCH_FLAGS) else {
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
        vte.search_set_regex(Some(&vte_re), 0);
        vte.search_set_wrap_around(true);
        if !vte.search_find_next() {
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
    }
    // The UI's no-record fallback installs a regex directly on the live VTE,
    // outside `FindState`; always clear it before a new structured pass too.
    active_vte.search_set_regex(None::<&vte4::Regex>, 0);
}

#[cfg(test)]
mod tests {
    use super::{
        add_snapshot_jump_fallbacks, bounded_match_count, command_preview, duration_matches,
        focus_one_native_forward_match, matching_record_ids, native_cursor_action,
        outcome_matches_filters, plan_matching_windows, regex_consumption, snippet,
        step_compressed_cursor, unresolved_record_target_result, utf8_prefix, FindCursor,
        FindDirection, FindScanBudget, FindSurface, NativeCursorAction, RecordNavigationResult,
        RecordSnapshotView, RegexConsumption, VTE_SEARCH_FLAGS,
    };
    use crate::block_view::{
        BackendRecordRef, BackendSearchWindow, BlockFilters, CompletedCommandRecord,
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
        }
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
