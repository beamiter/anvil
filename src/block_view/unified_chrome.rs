//! Probe-driven block chrome for Unified's single persistent VTE.
//!
//! OSC 8 is only an untrusted row locator. A URI becomes authoritative after
//! its nonce and id match this pane's ordered active-marker table. Keeping that
//! table separate from the command-record document is intentional: the later
//! marker-eviction increment can retire an id without deleting history search
//! or export metadata, and a stale URI can never resurrect it.

use gtk::prelude::*;
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ops::Bound::{Excluded, Included};
use std::rc::Rc;
use vte4::{Terminal, TerminalExt};

const URI_PREFIX: &str = "block://";
const NONCE_HEX_LEN: usize = 32;
const GUTTER_X: f64 = 3.0;
const GUTTER_WIDTH: f64 = 2.0;
const BADGE_PAD_X: f64 = 5.0;
const BADGE_PAD_Y: f64 = 2.0;
const MAX_CACHED_HEADS: usize = 512;
/// Marker-bearing zones are a short-lived addressing index, not history.
/// Include the currently-open prompt in this hard cap so a shell that keeps
/// opening zones cannot grow the authority table independently of records.
const MAX_ACTIVE_ZONE_MARKERS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ZoneRowSpan {
    row_epoch: u64,
    head: i64,
    end_exclusive: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CalibrationAnchor {
    zone_id: u64,
    ring_row: i64,
    row_epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ZoneChromeRecord {
    pub(super) id: u64,
    pub(super) exit_code: Option<i32>,
    pub(super) duration_ms: Option<u64>,
    pub(super) is_background: bool,
}

impl ZoneChromeRecord {
    fn badge(&self) -> String {
        let status = if self.is_background {
            "background".to_owned()
        } else {
            match self.exit_code {
                Some(0) => "✓".to_owned(),
                Some(code) => format!("exit:{code}"),
                None => "exit:?".to_owned(),
            }
        };
        self.duration_ms.map_or(status.clone(), |duration| {
            format!("{status} · {}", format_block_duration(duration))
        })
    }
}

pub(super) fn format_block_duration(dur_ms: u64) -> String {
    if dur_ms < 1000 {
        format!("{dur_ms}ms")
    } else if dur_ms < 60_000 {
        format!("{:.1}s", dur_ms as f64 / 1000.0)
    } else if dur_ms < 3_600_000 {
        let minutes = dur_ms / 60_000;
        let seconds = (dur_ms % 60_000) / 1000;
        if seconds == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m{seconds:02}s")
        }
    } else {
        let hours = dur_ms / 3_600_000;
        let minutes = (dur_ms % 3_600_000) / 60_000;
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{minutes:02}m")
        }
    }
}

#[derive(Clone, Copy)]
struct ChromeColors {
    success: gtk::gdk::RGBA,
    error: gtk::gdk::RGBA,
    warning: gtk::gdk::RGBA,
    accent: gtk::gdk::RGBA,
    info: gtk::gdk::RGBA,
}

impl ChromeColors {
    fn from_config(config: &super::Config) -> Self {
        Self {
            success: config.palette[2],
            error: config.palette[1],
            warning: config.palette[3],
            accent: config.palette[6],
            info: config.palette[4],
        }
    }
}

/// Marker authority for one pane. Records/history do not own this lifetime.
#[derive(Debug)]
pub(super) struct ZoneChromeAuthority {
    nonce: Option<[u8; 16]>,
    active: VecDeque<ZoneChromeRecord>,
    order_by_id: HashMap<u64, usize>,
    /// Monotonic watermark survives retirement/clear, so an old OSC 8 id can
    /// never regain authority after its table entry is gone.
    max_seen_id: Option<u64>,
    retirement_generation: u64,
    completed: HashSet<u64>,
    row_spans: HashMap<u64, ZoneRowSpan>,
    /// Trusted head rows ordered independently of lifecycle id. This bounded
    /// predecessor index lets a viewport start inside a long zone without a
    /// history-sized scan.
    heads_by_row: BTreeMap<(u64, i64), u64>,
    /// Set when bounded-ring capacity makes row survival unknowable until the
    /// next exact marker calibration supplies the retained floor.
    eviction_quarantine: bool,
}

impl ZoneChromeAuthority {
    pub(super) fn new(nonce: Option<[u8; 16]>) -> Self {
        Self {
            nonce,
            active: VecDeque::new(),
            order_by_id: HashMap::new(),
            max_seen_id: None,
            retirement_generation: 0,
            completed: HashSet::new(),
            row_spans: HashMap::new(),
            heads_by_row: BTreeMap::new(),
            eviction_quarantine: false,
        }
    }

    pub(super) fn begin_zone(&mut self, id: u64) {
        // IDs are append-only in the lifecycle. A duplicate or regression is
        // evidence that producer/table state drifted; refusing it is safer
        // than making an attacker-controlled old marker authoritative again.
        if self.max_seen_id.is_some_and(|max_seen| id <= max_seen) {
            return;
        }
        self.max_seen_id = Some(id);
        self.order_by_id.insert(id, self.active.len());
        self.active.push_back(ZoneChromeRecord {
            id,
            exit_code: None,
            duration_ms: None,
            is_background: false,
        });
        if self.active.len() > MAX_ACTIVE_ZONE_MARKERS {
            self.retire_prefix(self.active.len() - MAX_ACTIVE_ZONE_MARKERS);
        }
    }

    pub(super) fn record_completed(&mut self, record: ZoneChromeRecord) {
        // Completion only enriches authority established at PromptStart. The
        // entry may already have fallen out of the hard marker cap (or been
        // invalidated by reset); never recreate it from a late completion.
        if let Some(index) = self.order_by_id.get(&record.id).copied() {
            self.completed.insert(record.id);
            self.active[index] = record;
        }
    }

    /// Eviction seam for the next increment. Retiring marker authority does
    /// not remove the corresponding metadata record from find/export/history.
    #[allow(dead_code)]
    pub(super) fn retire_prefix(&mut self, count: usize) {
        let count = count.min(self.active.len());
        if count != 0 {
            let retired = self
                .active
                .drain(..count)
                .map(|record| record.id)
                .collect::<Vec<_>>();
            self.finish_retirement(&retired);
        }
    }

    fn finish_retirement(&mut self, retired: &[u64]) {
        if retired.is_empty() {
            return;
        }
        self.order_by_id = self
            .active
            .iter()
            .enumerate()
            .map(|(index, record)| (record.id, index))
            .collect();
        for id in retired {
            self.completed.remove(id);
            self.remove_row_span(*id);
        }
        self.retirement_generation = self.retirement_generation.wrapping_add(1);
    }

    pub(super) fn retire_ids(&mut self, retired: &[u64]) {
        if retired.is_empty() {
            return;
        }
        let retired_set = retired.iter().copied().collect::<HashSet<_>>();
        let before = self.active.len();
        self.active
            .retain(|record| !retired_set.contains(&record.id));
        if self.active.len() != before {
            self.finish_retirement(retired);
        }
    }

    pub(super) fn enforce_limit(&mut self, limit: usize) {
        let limit = limit.clamp(1, MAX_ACTIVE_ZONE_MARKERS);
        if self.active.len() > limit {
            self.retire_prefix(self.active.len() - limit);
        }
    }

    #[allow(dead_code)]
    pub(super) fn clear(&mut self) {
        let had_authority = !self.active.is_empty()
            || !self.order_by_id.is_empty()
            || !self.completed.is_empty()
            || !self.row_spans.is_empty()
            || !self.heads_by_row.is_empty();
        self.active.clear();
        self.order_by_id.clear();
        self.completed.clear();
        self.row_spans.clear();
        self.heads_by_row.clear();
        self.eviction_quarantine = false;
        if had_authority {
            self.retirement_generation = self.retirement_generation.wrapping_add(1);
        }
    }

    fn remove_row_span(&mut self, id: u64) {
        if let Some(span) = self.row_spans.remove(&id) {
            self.heads_by_row.remove(&(span.row_epoch, span.head));
        }
    }

    fn head_matches(&self, id: u64, row_epoch: u64, row: i64) -> bool {
        self.order_by_id.contains_key(&id)
            && self
                .row_spans
                .get(&id)
                .is_some_and(|span| span.row_epoch == row_epoch && span.head == row)
    }

    fn observe_head(&mut self, id: u64, row_epoch: u64, row: i64) {
        if !self.order_by_id.contains_key(&id) {
            return;
        }

        // Re-observation after cache loss replaces this id's old coordinate;
        // never leave two predecessor entries pointing at the same authority.
        self.remove_row_span(id);

        let predecessor = self
            .heads_by_row
            .range((Included((row_epoch, i64::MIN)), Excluded((row_epoch, row))))
            .next_back()
            .map(|(_, id)| *id);
        if let Some(predecessor) = predecessor {
            if let Some(span) = self.row_spans.get_mut(&predecessor) {
                if span.row_epoch == row_epoch && span.head < row {
                    span.end_exclusive = Some(row);
                }
            }
        }

        // Heads can be discovered out of order as the user scrolls. A known
        // successor still closes the newly-observed span exactly.
        let successor = self
            .heads_by_row
            .range((Excluded((row_epoch, row)), Included((row_epoch, i64::MAX))))
            .next()
            .map(|((_, head), _)| *head);

        if let Some(displaced) = self.heads_by_row.insert((row_epoch, row), id) {
            if displaced != id {
                self.row_spans.remove(&displaced);
            }
        }
        self.row_spans.insert(
            id,
            ZoneRowSpan {
                row_epoch,
                head: row,
                end_exclusive: successor,
            },
        );
    }

    fn zone_containing_row(&self, row_epoch: u64, row: i64) -> Option<u64> {
        let (&(epoch, head), &id) = self.heads_by_row.range(..=(row_epoch, row)).next_back()?;
        if epoch != row_epoch || !self.order_by_id.contains_key(&id) {
            return None;
        }
        let span = self.row_spans.get(&id)?;
        if span.row_epoch != row_epoch || span.head != head || row < head {
            return None;
        }
        match span.end_exclusive {
            Some(end) if row < end => Some(id),
            None if self.active.back().is_some_and(|record| record.id == id) => Some(id),
            _ => None,
        }
    }

    fn retire_completed_before(&mut self, row_epoch: u64, cutoff: i64) -> Vec<u64> {
        let retired = self
            .active
            .iter()
            .filter(|record| self.completed.contains(&record.id))
            .filter_map(|record| {
                self.row_spans.get(&record.id).and_then(|span| {
                    (span.row_epoch == row_epoch
                        && span.end_exclusive.is_some_and(|end| end <= cutoff))
                    .then_some(record.id)
                })
            })
            .collect::<Vec<_>>();
        self.retire_ids(&retired);
        retired
    }

    fn retire_heads_before(&mut self, row_epoch: u64, retained_floor: i64) -> Vec<u64> {
        let retired = self
            .active
            .iter()
            .filter_map(|record| {
                self.row_spans.get(&record.id).and_then(|span| {
                    (span.row_epoch == row_epoch && span.head < retained_floor).then_some(record.id)
                })
            })
            .collect::<Vec<_>>();
        self.retire_ids(&retired);
        retired
    }

    fn quarantine_eviction_drift(&mut self) {
        // Survive an intervening rewrap epoch. The next exact calibration must
        // still reject completed authority whose row survival was unproven.
        self.eviction_quarantine = true;
    }

    fn reconcile_calibrated_floor(&mut self, row_epoch: u64, retained_floor: i64) -> Vec<u64> {
        let quarantined = self.eviction_quarantine;
        let retired = self
            .active
            .iter()
            .filter(|record| {
                if !self.completed.contains(&record.id) {
                    return false;
                }
                match self.row_spans.get(&record.id) {
                    Some(span) if span.row_epoch == row_epoch => span.head < retained_floor,
                    // After unobservable capacity trim, a completed id with no
                    // current-epoch trusted head cannot prove a marker survived.
                    None | Some(_) => quarantined,
                }
            })
            .map(|record| record.id)
            .collect::<Vec<_>>();
        self.retire_ids(&retired);
        if quarantined {
            self.eviction_quarantine = false;
        }
        retired
    }

    fn record(&self, id: u64) -> Option<&ZoneChromeRecord> {
        self.order_by_id
            .get(&id)
            .and_then(|index| self.active.get(*index))
    }

    fn is_completed(&self, id: u64) -> bool {
        self.completed.contains(&id)
    }

    fn order(&self, id: u64) -> Option<usize> {
        self.order_by_id.get(&id).copied()
    }

    /// The zone's trusted head span, only while the id itself still holds
    /// marker authority. Retirement — including the quarantine reconciliation
    /// that refuses an unproven completed id — removes both together, so a
    /// stale span can never answer for a retired id.
    fn trusted_span(&self, id: u64) -> Option<ZoneRowSpan> {
        self.order_by_id.contains_key(&id).then_some(())?;
        self.row_spans.get(&id).copied()
    }
}

/// Parse only the one canonical spelling emitted by `ZoneMarkerInjector`.
/// Uppercase/non-hex nonces, leading-zero ids, suffixes and alternate URI
/// spellings all fail closed before the authority lookup.
fn parse_canonical_zone_uri(uri: &str) -> Option<([u8; 16], u64)> {
    let rest = uri.strip_prefix(URI_PREFIX)?;
    let bytes = rest.as_bytes();
    if bytes.len() < NONCE_HEX_LEN + 2 || bytes.get(NONCE_HEX_LEN) != Some(&b'/') {
        return None;
    }
    let nonce_hex = &bytes[..NONCE_HEX_LEN];
    let id_text = std::str::from_utf8(&bytes[NONCE_HEX_LEN + 1..]).ok()?;
    if id_text.is_empty()
        || (id_text.len() > 1 && id_text.starts_with('0'))
        || !id_text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let mut nonce = [0_u8; 16];
    for (index, pair) in nonce_hex.chunks_exact(2).enumerate() {
        let high = lowercase_hex_value(pair[0])?;
        let low = lowercase_hex_value(pair[1])?;
        nonce[index] = (high << 4) | low;
    }
    Some((nonce, id_text.parse().ok()?))
}

fn lowercase_hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScanLayoutKey {
    columns: i64,
    ring_lower: i64,
    row_epoch: u64,
}

/// Trusted mapping from visible probe rows to VTE ring rows. The drawing
/// layer never guesses this from cursor-to-grid slack: after VTE reset/rebase
/// the adjustment can live in a different coordinate space, and a prompt
/// cursor need not occupy the grid's physical last row. A canonical marker
/// probe ties one visible adjustment row to its pre-feed ring-row anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ViewportProjection {
    /// `ring row - adjustment row`, proven by a canonical visible marker.
    pub(super) row_offset: i64,
    pub(super) ring_lower: i64,
    pub(super) row_epoch: u64,
}

fn projection_from_calibration_anchor<'a>(
    anchor: CalibrationAnchor,
    expected_epoch: u64,
    adjustment_lower: f64,
    adjustment_upper: f64,
    adjustment_value: f64,
    probe_uris: impl IntoIterator<Item = Option<&'a str>>,
    authority: &ZoneChromeAuthority,
) -> Option<ViewportProjection> {
    if anchor.row_epoch != expected_epoch
        || authority.nonce.is_none()
        || authority.order(anchor.zone_id).is_none()
        || authority
            .active
            .back()
            .is_none_or(|record| record.id != anchor.zone_id)
    {
        return None;
    }
    let expected_nonce = authority.nonce?;
    let lower = finite_floor_i64(adjustment_lower)?;
    let upper = finite_ceil_i64(adjustment_upper)?;
    let viewport_top = finite_floor_i64(adjustment_value)?;
    if lower > viewport_top || viewport_top > upper {
        return None;
    }
    let visible_row = probe_uris
        .into_iter()
        .enumerate()
        .find_map(|(visible_row, uri)| {
            let (nonce, id) = parse_canonical_zone_uri(uri?)?;
            (nonce == expected_nonce && id == anchor.zone_id).then_some(visible_row)
        })?;
    let visible_row = i64::try_from(visible_row).ok()?;
    let marker_adjustment_row = viewport_top.checked_add(visible_row)?;
    if marker_adjustment_row < lower || marker_adjustment_row >= upper {
        return None;
    }
    let row_offset = anchor.ring_row.checked_sub(marker_adjustment_row)?;
    if row_offset < 0 {
        return None;
    }
    let ring_lower = lower.checked_add(row_offset)?;
    let ring_upper = upper.checked_add(row_offset)?;
    if anchor.ring_row < ring_lower
        || anchor.ring_row >= ring_upper
        || marker_adjustment_row.checked_add(row_offset)? != anchor.ring_row
    {
        return None;
    }
    Some(ViewportProjection {
        row_offset,
        ring_lower,
        row_epoch: expected_epoch,
    })
}

#[derive(Default, Debug)]
struct KnownHeadCache {
    layout: Option<ScanLayoutKey>,
    retirement_generation: u64,
    heads: BTreeMap<i64, u64>,
}

impl KnownHeadCache {
    fn prepare(&mut self, layout: ScanLayoutKey, authority: &ZoneChromeAuthority) {
        if self.layout != Some(layout) {
            // Absolute row attribution cannot survive a column/rewrap change;
            // VTE can move every marker without emitting a new OSC sequence.
            self.heads.clear();
            self.layout = Some(layout);
        }
        if self.retirement_generation != authority.retirement_generation {
            self.heads.clear();
            self.retirement_generation = authority.retirement_generation;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VisibleZoneLine {
    visible_row: usize,
    absolute_row: i64,
    zone_id: u64,
    is_head: bool,
}

/// A single O(viewport rows) scan: exactly one column-0 probe per input row,
/// plus at most one bounded widening query per zone whose first row starts
/// past column 0. No history-sized fallback or retry is permitted on the
/// drawing path.
fn scan_visible_rows<'a>(
    layout: ScanLayoutKey,
    top_absolute_row: i64,
    probe_uris: impl IntoIterator<Item = Option<&'a str>>,
    authority: &mut ZoneChromeAuthority,
    cache: &mut KnownHeadCache,
) -> Vec<VisibleZoneLine> {
    scan_visible_rows_with(
        layout,
        top_absolute_row,
        probe_uris,
        authority,
        cache,
        &mut |_, _| false,
    )
}

/// `row_carries_zone(visible_row, zone_id)` answers whether that row carries
/// this zone's canonical marker somewhere past column 0. It is consulted only
/// for the row immediately above an observed transition, and only when that
/// row's own column-0 probe found nothing — at most one extra query per new
/// zone in steady state.
fn scan_visible_rows_with<'a>(
    layout: ScanLayoutKey,
    top_absolute_row: i64,
    probe_uris: impl IntoIterator<Item = Option<&'a str>>,
    authority: &mut ZoneChromeAuthority,
    cache: &mut KnownHeadCache,
    row_carries_zone: &mut dyn FnMut(usize, u64) -> bool,
) -> Vec<VisibleZoneLine> {
    cache.prepare(layout, authority);
    let Some(expected_nonce) = authority.nonce else {
        return Vec::new();
    };

    let mut result: Vec<VisibleZoneLine> = Vec::new();
    let mut current_id = authority.zone_containing_row(layout.row_epoch, top_absolute_row);
    let mut highest_order = current_id.and_then(|id| authority.order(id));
    // Whether the row pushed last failed its own column-0 parse, and whether
    // this scan has already spent its one widening query.
    let mut previous_row_unowned = false;
    let mut widened = false;
    for (visible_row, uri) in probe_uris.into_iter().enumerate() {
        let absolute_row = top_absolute_row.saturating_add(visible_row as i64);
        let cached = cache
            .heads
            .get(&absolute_row)
            .copied()
            .filter(|id| authority.head_matches(*id, layout.row_epoch, absolute_row));
        let parsed = uri
            .and_then(parse_canonical_zone_uri)
            .filter(|(nonce, _)| *nonce == expected_nonce)
            .map(|(_, id)| id)
            .filter(|id| authority.order(*id).is_some());

        // A once-validated head belongs to the original zone even if a guest
        // PS1 later overwrites column 0 with its own OSC 8. Cache authority is
        // still rechecked above whenever the active-marker table changes.
        // OSC 8 persists until another marker appears. Once a visible row
        // proves a zone, propagate it through markerless output rows in this
        // scan. A bounded trusted predecessor span seeds a viewport that begins
        // mid-zone; without one, `current_id` stays None and fails closed.
        let id = cached.or(parsed).or(current_id);
        let Some(id) = id else {
            continue;
        };
        let Some(order) = authority.order(id) else {
            continue;
        };
        if highest_order.is_some_and(|highest| order < highest) {
            // Marker order can only advance down the VTE ring. A regression is
            // forged/stale input, never a reason to jump chrome backwards.
            continue;
        }
        highest_order = Some(highest_order.map_or(order, |highest: usize| highest.max(order)));
        // A viewport starting in the middle of a zone must not synthesize a
        // separator/badge on its first row. Once a real transition has been
        // seen below row zero, remember its absolute row for future scrolls.
        let indexed_head = authority.head_matches(id, layout.row_epoch, absolute_row);
        let observed_transition = parsed == Some(id) && current_id != Some(id) && !indexed_head;
        // A zone's first row begins wherever the shell resumed writing. When
        // that is not column 0 — output with no trailing newline, or the tty's
        // own `^C` echo — column 0 of that row was written while the marker was
        // closed, so its probe finds nothing and propagation hands the row to
        // the previous zone. The separator and badge then sit one row low.
        //
        // Widen by exactly one row, and only when every one of these holds: the
        // row is the immediately preceding scanned row, its own column-0 parse
        // failed this scan, it is not already some zone's validated head (a
        // validated head survives a guest OSC 8 at column 0 and must never be
        // stolen — displacing it would delete its span outright), and the ring
        // proves it carries this zone's canonical marker past column 0.
        let mut widened_head = false;
        if observed_transition && !widened {
            let widen_onto = result.last().filter(|previous| {
                previous_row_unowned
                    && !previous.is_head
                    && previous.visible_row + 1 == visible_row
                    && previous.absolute_row + 1 == absolute_row
            });
            if let Some((prev_visible, prev_absolute)) =
                widen_onto.map(|previous| (previous.visible_row, previous.absolute_row))
            {
                if row_carries_zone(prev_visible, id) {
                    widened = true;
                    widened_head = true;
                    authority.observe_head(id, layout.row_epoch, prev_absolute);
                    cache.heads.insert(prev_absolute, id);
                    if let Some(previous) = result.last_mut() {
                        previous.zone_id = id;
                        previous.is_head = true;
                    }
                }
            }
        }
        if observed_transition && !widened_head {
            authority.observe_head(id, layout.row_epoch, absolute_row);
            cache.heads.insert(absolute_row, id);
        }
        if observed_transition {
            while cache.heads.len() > MAX_CACHED_HEADS {
                let Some(oldest) = cache.heads.first_key_value().map(|(row, _)| *row) else {
                    break;
                };
                cache.heads.remove(&oldest);
            }
        }
        previous_row_unowned = parsed.is_none();
        // A canonical marker at row zero is an observed head, not an inferred
        // mid-zone continuation, and therefore earns its real separator. A row
        // whose head moved up one is a continuation of that head, not a second.
        let is_head = (cached == Some(id) || indexed_head || observed_transition) && !widened_head;
        result.push(VisibleZoneLine {
            visible_row,
            absolute_row,
            zone_id: id,
            is_head,
        });
        current_id = Some(id);
    }
    result
}

fn badge_lands_on_blank_cells(row_tail: &str) -> bool {
    row_tail
        .chars()
        .all(|character| matches!(character, ' ' | '\t' | '\r' | '\n'))
}

fn badge_probe_start_col(start_col: i64) -> i64 {
    // VTE may expose the second cell of a wide glyph as an empty range when a
    // query begins on that continuation cell. Include its left neighbour and
    // accept the harmless false-negative for an adjacent narrow glyph rather
    // than painting over terminal content.
    start_col.saturating_sub(1).max(0)
}

fn finite_floor_i64(value: f64) -> Option<i64> {
    (value.is_finite() && value >= i64::MIN as f64 && value <= i64::MAX as f64)
        .then(|| value.floor() as i64)
}

fn finite_ceil_i64(value: f64) -> Option<i64> {
    (value.is_finite() && value >= i64::MIN as f64 && value <= i64::MAX as f64)
        .then(|| value.ceil() as i64)
}

fn invalidate_row_projection(
    projection: &Cell<Option<ViewportProjection>>,
    calibration_anchor: &Cell<Option<CalibrationAnchor>>,
    row_epoch: &Cell<u64>,
    cache: &RefCell<KnownHeadCache>,
) {
    row_epoch.set(row_epoch.get().wrapping_add(1));
    projection.set(None);
    calibration_anchor.set(None);
    *cache.borrow_mut() = KnownHeadCache::default();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TrustedRingBounds {
    pub(super) row_epoch: u64,
    pub(super) retained_start: i64,
    pub(super) retained_end_exclusive: i64,
    pub(super) viewport_top: i64,
}

fn trusted_ring_bounds_from(
    projection: ViewportProjection,
    lower: f64,
    upper: f64,
    value: f64,
) -> Option<TrustedRingBounds> {
    let retained_start = finite_floor_i64(lower)?.checked_add(projection.row_offset)?;
    if retained_start != projection.ring_lower {
        return None;
    }
    let retained_end_exclusive = finite_ceil_i64(upper)?.checked_add(projection.row_offset)?;
    let viewport_top = finite_floor_i64(value)?.checked_add(projection.row_offset)?;
    (retained_start <= viewport_top && viewport_top <= retained_end_exclusive).then_some(
        TrustedRingBounds {
            row_epoch: projection.row_epoch,
            retained_start,
            retained_end_exclusive,
            viewport_top,
        },
    )
}

/// Pure gate behind [`UnifiedChrome::proven_zone_scroll_value`]. Every input
/// must independently prove itself at `current_epoch` — the projection, the
/// ring bounds derived from it, and the zone's recorded head — and the head
/// must lie inside the retained ring. Any doubt is `None`: a record jump is
/// exact or it does not happen.
fn proven_zone_scroll_value_from(
    projection: ViewportProjection,
    current_epoch: u64,
    span: Option<ZoneRowSpan>,
    lower: f64,
    upper: f64,
    value: f64,
    page_size: f64,
) -> Option<f64> {
    (projection.row_epoch == current_epoch).then_some(())?;
    let bounds = trusted_ring_bounds_from(projection, lower, upper, value)?;
    let span = span?;
    (span.row_epoch == current_epoch).then_some(())?;
    (bounds.retained_start <= span.head && span.head < bounds.retained_end_exclusive)
        .then_some(())?;
    if !page_size.is_finite() || page_size < 0.0 {
        return None;
    }
    let adjustment_row = span.head.checked_sub(projection.row_offset)?;
    // A retained head near the ring tail cannot be scrolled to the viewport
    // top; clamping to the scroll range still shows the row.
    let max_value = (upper - page_size).max(lower);
    Some((adjustment_row as f64).clamp(lower, max_value))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectionDrift {
    Stable,
    ForwardTrim {
        retained_floor: i64,
    },
    /// At bounded-ring capacity, a coalesced contents snapshot cannot
    /// distinguish cursor motion from trim followed by cursor motion.
    UnprovenAtCapacity {
        proven_floor: Option<i64>,
    },
    Invalidate,
}

fn adjustment_may_be_at_scrollback_capacity(upper: f64, scrollback_lines: i64) -> Option<bool> {
    let upper = finite_ceil_i64(upper)?;
    Some(scrollback_lines >= 0 && upper >= scrollback_lines)
}

fn projection_drift_after_contents(
    projection: ViewportProjection,
    cursor_row: i64,
    adjustment_lower: f64,
    adjustment_upper: f64,
    scrollback_lines: i64,
) -> ProjectionDrift {
    let Some(candidate_offset) = cursor_row
        .checked_add(1)
        .and_then(|cursor_end| {
            finite_ceil_i64(adjustment_upper).and_then(|upper| cursor_end.checked_sub(upper))
        })
        .map(|offset| offset.max(0))
    else {
        return ProjectionDrift::Invalidate;
    };
    let Some(at_capacity) =
        adjustment_may_be_at_scrollback_capacity(adjustment_upper, scrollback_lines)
    else {
        return ProjectionDrift::Invalidate;
    };
    if at_capacity {
        let proven_floor = (candidate_offset > projection.row_offset)
            .then(|| {
                finite_floor_i64(adjustment_lower)
                    .and_then(|lower| lower.checked_add(candidate_offset))
            })
            .flatten();
        return ProjectionDrift::UnprovenAtCapacity { proven_floor };
    }
    match candidate_offset.cmp(&projection.row_offset) {
        // Cursor-to-upper omits any unused physical rows below the cursor, so
        // it is only a lower bound on the exact marker-calibrated offset.
        std::cmp::Ordering::Less | std::cmp::Ordering::Equal => ProjectionDrift::Stable,
        std::cmp::Ordering::Greater => finite_floor_i64(adjustment_lower)
            .and_then(|lower| lower.checked_add(candidate_offset))
            .map_or(ProjectionDrift::Invalidate, |retained_floor| {
                ProjectionDrift::ForwardTrim { retained_floor }
            }),
    }
}

#[derive(Clone)]
pub(super) struct UnifiedChrome {
    surface: gtk::DrawingArea,
    authority: Rc<RefCell<ZoneChromeAuthority>>,
    projection: Rc<Cell<Option<ViewportProjection>>>,
    calibration_anchor: Rc<Cell<Option<CalibrationAnchor>>>,
    row_epoch: Rc<Cell<u64>>,
    cache: Rc<RefCell<KnownHeadCache>>,
}

impl UnifiedChrome {
    pub(super) fn new(
        terminal: &Terminal,
        surface: &gtk::DrawingArea,
        authority: Rc<RefCell<ZoneChromeAuthority>>,
        config: Rc<RefCell<super::Config>>,
    ) -> Self {
        let cache = Rc::new(RefCell::new(KnownHeadCache::default()));
        let projection: Rc<Cell<Option<ViewportProjection>>> = Rc::new(Cell::new(None));
        let calibration_anchor: Rc<Cell<Option<CalibrationAnchor>>> = Rc::new(Cell::new(None));
        let row_epoch = Rc::new(Cell::new(1_u64));
        let terminal_for_draw = terminal.clone();
        let authority_for_draw = authority.clone();
        let config_for_draw = config;
        let cache_for_draw = cache.clone();
        let projection_for_draw = projection.clone();
        let calibration_anchor_for_draw = calibration_anchor.clone();
        let row_epoch_for_draw = row_epoch.clone();
        surface.set_draw_func(move |_area, cr, width, height| {
            if width <= 0 || height <= 0 {
                return;
            }
            let cell_width = terminal_for_draw.char_width().max(1) as f64;
            let cell_height = terminal_for_draw.char_height().max(1) as f64;
            let columns = terminal_for_draw.column_count().max(1);
            let border = gtk::prelude::ScrollableExt::border(&terminal_for_draw);
            let (content_x, content_y) = border.as_ref().map_or((0.0, 0.0), |border| {
                (f64::from(border.left()), f64::from(border.top()))
            });
            let visible_rows = terminal_for_draw
                .row_count()
                .max(0)
                .min(((f64::from(height) - content_y).max(0.0) / cell_height).ceil() as i64)
                as usize;
            let Some(adjustment) = terminal_for_draw.vadjustment() else {
                return;
            };
            // VTE 0.78.2's `Terminal::rowcol_at` subtracts `m_border.left/top`
            // inside `check_hyperlink_at`. The public API therefore expects
            // widget coordinates. Pass padding + cell centre here; subtracting
            // padding ourselves would select the previous/outside row. Drawing
            // uses the same content origin, so probe and paint stay aligned.
            let probe_uris: Vec<_> = (0..visible_rows)
                .map(|row| {
                    terminal_for_draw.check_hyperlink_at(
                        content_x + cell_width * 0.5,
                        content_y + (row as f64 + 0.5) * cell_height,
                    )
                })
                .collect();
            let mut authority = authority_for_draw.borrow_mut();
            let current_epoch = row_epoch_for_draw.get();
            let projection = match projection_for_draw.get() {
                Some(projection) if projection.row_epoch == current_epoch => projection,
                Some(_) => return,
                None => {
                    let Some(anchor) = calibration_anchor_for_draw.get() else {
                        return;
                    };
                    let Some(projection) = projection_from_calibration_anchor(
                        anchor,
                        current_epoch,
                        adjustment.lower(),
                        adjustment.upper(),
                        adjustment.value(),
                        probe_uris.iter().map(Option::as_deref),
                        &authority,
                    ) else {
                        return;
                    };
                    authority
                        .reconcile_calibrated_floor(projection.row_epoch, projection.ring_lower);
                    projection_for_draw.set(Some(projection));
                    calibration_anchor_for_draw.set(None);
                    projection
                }
            };
            let current_ring_lower = finite_floor_i64(adjustment.lower())
                .and_then(|lower| lower.checked_add(projection.row_offset));
            if current_ring_lower != Some(projection.ring_lower) {
                // Ring trim/reset changed the coordinate relation since the
                // marker probe established it.
                return;
            }
            let Some(top_ring_row) = finite_floor_i64(adjustment.value())
                .and_then(|top| top.checked_add(projection.row_offset))
            else {
                return;
            };
            let layout = ScanLayoutKey {
                columns,
                ring_lower: projection.ring_lower,
                row_epoch: projection.row_epoch,
            };
            // The widening probe walks a row rightwards looking for this
            // pane's canonical marker past column 0. `check_hyperlink_at`
            // takes widget coordinates, the same convention as the column-0
            // pass above.
            let pane_nonce = authority.nonce;
            let mut row_carries_zone = |visible_row: usize, zone_id: u64| {
                let y = content_y + (visible_row as f64 + 0.5) * cell_height;
                (1..columns).any(|column| {
                    terminal_for_draw
                        .check_hyperlink_at(content_x + (column as f64 + 0.5) * cell_width, y)
                        .as_deref()
                        .and_then(parse_canonical_zone_uri)
                        .is_some_and(|(nonce, id)| Some(nonce) == pane_nonce && id == zone_id)
                })
            };
            let lines = scan_visible_rows_with(
                layout,
                top_ring_row,
                probe_uris.iter().map(Option::as_deref),
                &mut authority,
                &mut cache_for_draw.borrow_mut(),
                &mut row_carries_zone,
            );
            if lines.is_empty() {
                return;
            }

            let colors = ChromeColors::from_config(&config_for_draw.borrow());
            let accent = colors.accent;
            cr.set_source_rgba(
                f64::from(accent.red()),
                f64::from(accent.green()),
                f64::from(accent.blue()),
                0.72,
            );
            for line in &lines {
                let y = content_y + line.visible_row as f64 * cell_height;
                cr.rectangle(content_x + GUTTER_X, y, GUTTER_WIDTH, cell_height + 0.5);
            }
            let _ = cr.fill();

            for line in lines.iter().filter(|line| line.is_head) {
                let Some(record) = authority.record(line.zone_id) else {
                    continue;
                };
                let y = content_y + line.visible_row as f64 * cell_height;
                cr.set_source_rgba(
                    f64::from(accent.red()),
                    f64::from(accent.green()),
                    f64::from(accent.blue()),
                    0.45,
                );
                cr.rectangle(
                    content_x + GUTTER_X,
                    y,
                    (f64::from(width) - content_x).max(0.0),
                    1.0,
                );
                let _ = cr.fill();

                // The open prompt participates in gutter/separator geometry,
                // but has no outcome yet and must not render a fabricated
                // `exit:?` badge.
                if !authority.is_completed(line.zone_id) {
                    continue;
                }

                let badge = record.badge();
                cr.select_font_face(
                    "sans-serif",
                    gtk::cairo::FontSlant::Normal,
                    gtk::cairo::FontWeight::Bold,
                );
                cr.set_font_size((cell_height * 0.62).clamp(8.0, 13.0));
                let Ok(extents) = cr.text_extents(&badge) else {
                    continue;
                };
                let badge_width = extents.width() + BADGE_PAD_X * 2.0;
                let badge_height = (extents.height() + BADGE_PAD_Y * 2.0).min(cell_height - 1.0);
                let badge_x = (f64::from(width) - badge_width - 6.0).max(content_x + cell_width);
                let start_col =
                    (((badge_x - content_x) / cell_width).floor() as i64).clamp(0, columns);
                let probe_start_col = badge_probe_start_col(start_col);
                let row_tail = terminal_for_draw
                    .text_range_format(
                        vte4::Format::Text,
                        line.absolute_row,
                        probe_start_col,
                        line.absolute_row,
                        columns,
                    )
                    .0;
                let Some(row_tail) = row_tail else {
                    // An unavailable range (including a wide-cell boundary
                    // VTE cannot represent) is unknown, never blank.
                    continue;
                };
                if !badge_lands_on_blank_cells(row_tail.as_str()) {
                    continue;
                }

                let badge_y = y + (cell_height - badge_height) / 2.0;
                let color = if record.is_background {
                    colors.info
                } else {
                    match record.exit_code {
                        Some(0) => colors.success,
                        Some(_) => colors.error,
                        None => colors.warning,
                    }
                };
                cr.set_source_rgba(
                    f64::from(color.red()),
                    f64::from(color.green()),
                    f64::from(color.blue()),
                    0.20,
                );
                cr.rectangle(badge_x, badge_y, badge_width, badge_height);
                let _ = cr.fill();
                cr.set_source_rgba(
                    f64::from(color.red()),
                    f64::from(color.green()),
                    f64::from(color.blue()),
                    1.0,
                );
                cr.move_to(
                    badge_x + BADGE_PAD_X - extents.x_bearing(),
                    badge_y + (badge_height - extents.height()) / 2.0 - extents.y_bearing(),
                );
                let _ = cr.show_text(&badge);
            }
        });

        let queue = surface.downgrade();
        let projection_for_contents = projection.clone();
        let calibration_anchor_for_contents = calibration_anchor.clone();
        let authority_for_contents = authority.clone();
        let cache_for_contents = cache.clone();
        let row_epoch_for_contents = row_epoch.clone();
        terminal.connect_contents_changed(move |terminal| {
            if let (Some(published), Some(adjustment)) =
                (projection_for_contents.get(), terminal.vadjustment())
            {
                match projection_drift_after_contents(
                    published,
                    terminal.cursor_position().1,
                    adjustment.lower(),
                    adjustment.upper(),
                    terminal.scrollback_lines(),
                ) {
                    ProjectionDrift::Stable => {}
                    ProjectionDrift::ForwardTrim { retained_floor } => {
                        let mut authority = authority_for_contents.borrow_mut();
                        authority.retire_heads_before(published.row_epoch, retained_floor);
                        authority.quarantine_eviction_drift();
                        drop(authority);
                        invalidate_row_projection(
                            &projection_for_contents,
                            &calibration_anchor_for_contents,
                            &row_epoch_for_contents,
                            &cache_for_contents,
                        );
                    }
                    ProjectionDrift::UnprovenAtCapacity { proven_floor } => {
                        if let Some(retained_floor) = proven_floor {
                            authority_for_contents
                                .borrow_mut()
                                .retire_heads_before(published.row_epoch, retained_floor);
                        }
                        // Natural trim does not move surviving ring rows. Keep
                        // epoch/index, but drop the adjustment mapping until a
                        // later A plus canonical visible marker recalibrates it.
                        authority_for_contents
                            .borrow_mut()
                            .quarantine_eviction_drift();
                        projection_for_contents.set(None);
                    }
                    ProjectionDrift::Invalidate => invalidate_row_projection(
                        &projection_for_contents,
                        &calibration_anchor_for_contents,
                        &row_epoch_for_contents,
                        &cache_for_contents,
                    ),
                }
            }
            if let Some(surface) = queue.upgrade() {
                surface.queue_draw();
            }
        });
        if let Some(adjustment) = terminal.vadjustment() {
            let queue = surface.downgrade();
            adjustment.connect_value_changed(move |_| {
                if let Some(surface) = queue.upgrade() {
                    surface.queue_draw();
                }
            });
            let queue = surface.downgrade();
            let projection = projection.clone();
            let authority = authority.clone();
            let cache = cache.clone();
            let row_epoch = row_epoch.clone();
            let calibration_anchor = calibration_anchor.clone();
            adjustment.connect_changed(move |adjustment| {
                if let Some(published) = projection.get() {
                    let current_floor = finite_floor_i64(adjustment.lower())
                        .and_then(|lower| lower.checked_add(published.row_offset));
                    if current_floor != Some(published.ring_lower) {
                        if let Some(floor) =
                            current_floor.filter(|floor| *floor > published.ring_lower)
                        {
                            let mut authority = authority.borrow_mut();
                            authority.retire_heads_before(published.row_epoch, floor);
                            authority.quarantine_eviction_drift();
                        }
                        invalidate_row_projection(
                            &projection,
                            &calibration_anchor,
                            &row_epoch,
                            &cache,
                        );
                    }
                }
                if let Some(surface) = queue.upgrade() {
                    surface.queue_draw();
                }
            });
        }
        let queue = surface.downgrade();
        let projection_for_columns = projection.clone();
        let calibration_anchor_for_columns = calibration_anchor.clone();
        let row_epoch_for_columns = row_epoch.clone();
        let cache_for_columns = cache.clone();
        terminal.connect_notify_local(Some("column-count"), move |terminal, _| {
            // A column-count change moves retained rows only when VTE rewraps
            // on resize. Height changes and toggling the future rewrap policy
            // do not invalidate existing absolute row coordinates.
            if terminal.property::<bool>("rewrap-on-resize") {
                invalidate_row_projection(
                    &projection_for_columns,
                    &calibration_anchor_for_columns,
                    &row_epoch_for_columns,
                    &cache_for_columns,
                );
            }
            if let Some(surface) = queue.upgrade() {
                surface.queue_draw();
            }
        });
        let queue = surface.downgrade();
        let authority_for_scrollback = authority.clone();
        let projection_for_scrollback = projection.clone();
        let calibration_anchor_for_scrollback = calibration_anchor.clone();
        let row_epoch_for_scrollback = row_epoch.clone();
        let cache_for_scrollback = cache.clone();
        terminal.connect_notify_local(Some("scrollback-lines"), move |_, _| {
            authority_for_scrollback
                .borrow_mut()
                .quarantine_eviction_drift();
            invalidate_row_projection(
                &projection_for_scrollback,
                &calibration_anchor_for_scrollback,
                &row_epoch_for_scrollback,
                &cache_for_scrollback,
            );
            if let Some(surface) = queue.upgrade() {
                surface.queue_draw();
            }
        });

        Self {
            surface: surface.clone(),
            authority,
            projection,
            calibration_anchor,
            row_epoch,
            cache,
        }
    }

    pub(super) fn record_completed(&self, record: ZoneChromeRecord) {
        self.authority.borrow_mut().record_completed(record);
        self.surface.queue_draw();
    }

    pub(super) fn begin_zone(&self, id: u64, terminal: &Terminal) {
        let authoritative = {
            let mut authority = self.authority.borrow_mut();
            authority.begin_zone(id);
            authority
                .active
                .back()
                .is_some_and(|record| record.id == id)
        };
        if authoritative {
            // Capture before the OSC 8 opener is fed. Draw will trust this
            // ring row only after the exact pane nonce/id is visible in VTE.
            self.calibration_anchor.set(Some(CalibrationAnchor {
                zone_id: id,
                ring_row: terminal.cursor_position().1,
                row_epoch: self.row_epoch.get(),
            }));
        }
        self.surface.queue_draw();
    }

    pub(super) fn retire_ids(&self, ids: &[u64]) {
        self.authority.borrow_mut().retire_ids(ids);
        self.surface.queue_draw();
    }

    pub(super) fn enforce_limit(&self, limit: usize) {
        self.authority.borrow_mut().enforce_limit(limit);
        self.surface.queue_draw();
    }

    pub(super) fn clear_authority(&self) {
        self.authority.borrow_mut().clear();
        invalidate_row_projection(
            &self.projection,
            &self.calibration_anchor,
            &self.row_epoch,
            &self.cache,
        );
        self.surface.queue_draw();
    }

    pub(super) fn erase_scrollback(&self, terminal: &Terminal) {
        self.authority.borrow_mut().quarantine_eviction_drift();
        let Some(projection) = self.projection.get() else {
            invalidate_row_projection(
                &self.projection,
                &self.calibration_anchor,
                &self.row_epoch,
                &self.cache,
            );
            self.surface.queue_draw();
            return;
        };
        if !self.surface.is_visible() {
            invalidate_row_projection(
                &self.projection,
                &self.calibration_anchor,
                &self.row_epoch,
                &self.cache,
            );
            self.surface.queue_draw();
            return;
        }
        if let Some(adjustment) = terminal.vadjustment() {
            let bounds = trusted_ring_bounds_from(
                projection,
                adjustment.lower(),
                adjustment.upper(),
                adjustment.value(),
            );
            let cutoff = finite_floor_i64(adjustment.upper() - adjustment.page_size())
                .and_then(|top| top.checked_add(projection.row_offset));
            if bounds.is_some() {
                if let Some(cutoff) = cutoff {
                    self.authority
                        .borrow_mut()
                        .retire_completed_before(projection.row_epoch, cutoff);
                }
            }
        }
        // ED3 mutates the primary ring immediately after this pre-feed hook.
        // No old coordinate/cache may survive even when no span was complete
        // enough for selective retirement.
        invalidate_row_projection(
            &self.projection,
            &self.calibration_anchor,
            &self.row_epoch,
            &self.cache,
        );
        self.surface.queue_draw();
    }

    #[allow(dead_code)]
    pub(super) fn trusted_ring_bounds(&self, terminal: &Terminal) -> Option<TrustedRingBounds> {
        let projection = self.projection.get()?;
        (projection.row_epoch == self.row_epoch.get()).then_some(())?;
        let adjustment = terminal.vadjustment()?;
        trusted_ring_bounds_from(
            projection,
            adjustment.lower(),
            adjustment.upper(),
            adjustment.value(),
        )
    }

    /// Adjustment value that provably shows the zone's head row, or `None`
    /// when any link in the proof chain (projection epoch, ring bounds, the
    /// zone's recorded span, retention of the head row) is missing. The caller
    /// must treat `None` as "no exact location", never guess one.
    pub(super) fn proven_zone_scroll_value(
        &self,
        terminal: &Terminal,
        zone_id: u64,
    ) -> Option<f64> {
        let projection = self.projection.get()?;
        let adjustment = terminal.vadjustment()?;
        let span = self.authority.borrow().trusted_span(zone_id);
        proven_zone_scroll_value_from(
            projection,
            self.row_epoch.get(),
            span,
            adjustment.lower(),
            adjustment.upper(),
            adjustment.value(),
            adjustment.page_size(),
        )
    }

    pub(super) fn set_alt_screen(&self, alt_screen: bool) {
        self.surface.set_visible(!alt_screen);
        if alt_screen {
            invalidate_row_projection(
                &self.projection,
                &self.calibration_anchor,
                &self.row_epoch,
                &self.cache,
            );
        } else {
            self.surface.queue_draw();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: [u8; 16] = [0xab; 16];

    fn uri(id: u64) -> String {
        format!("block://abababababababababababababababab/{id}")
    }

    fn record(id: u64) -> ZoneChromeRecord {
        ZoneChromeRecord {
            id,
            exit_code: Some(0),
            duration_ms: None,
            is_background: false,
        }
    }

    fn make_authority(ids: &[u64]) -> ZoneChromeAuthority {
        let mut authority = ZoneChromeAuthority::new(Some(NONCE));
        for id in ids {
            authority.begin_zone(*id);
            authority.record_completed(record(*id));
        }
        authority
    }

    fn layout(columns: i64, generation: u64) -> ScanLayoutKey {
        ScanLayoutKey {
            columns,
            ring_lower: 0,
            row_epoch: generation,
        }
    }

    #[test]
    fn canonical_uri_parser_rejects_every_alternate_spelling() {
        assert_eq!(parse_canonical_zone_uri(&uri(42)), Some((NONCE, 42)));
        for invalid in [
            "http://abababababababababababababababab/42",
            "block://ABABABABABABABABABABABABABABABAB/42",
            "block://abababababababababababababababa/42",
            "block://abababababababababababababababab/042",
            "block://abababababababababababababababab/+42",
            "block://abababababababababababababababab/42/",
            "block://abababababababababababababababag/42",
            "block://éééééééééééééééé/42",
        ] {
            assert_eq!(parse_canonical_zone_uri(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn authority_itself_rejects_duplicate_and_regressing_ids() {
        let mut authority = make_authority(&[10, 20]);
        authority.record_completed(record(20));
        authority.record_completed(record(15));
        authority.retire_prefix(2);
        authority.begin_zone(15);
        assert_eq!(
            authority
                .active
                .iter()
                .map(|record| record.id)
                .collect::<Vec<_>>(),
            Vec::<u64>::new()
        );
    }

    #[test]
    fn pending_zone_is_authoritative_before_finalize_and_completion_updates_it() {
        let mut authority = ZoneChromeAuthority::new(Some(NONCE));
        authority.begin_zone(7);
        assert_eq!(authority.record(7).unwrap().exit_code, None);
        authority.record_completed(ZoneChromeRecord {
            id: 7,
            exit_code: Some(23),
            duration_ms: Some(1200),
            is_background: false,
        });
        assert_eq!(authority.record(7).unwrap().exit_code, Some(23));
    }

    #[test]
    fn marker_authority_hard_cap_includes_pending_zone() {
        let mut authority = ZoneChromeAuthority::new(Some(NONCE));
        for id in 0..=MAX_ACTIVE_ZONE_MARKERS as u64 {
            authority.begin_zone(id);
        }

        assert_eq!(authority.active.len(), MAX_ACTIVE_ZONE_MARKERS);
        assert_eq!(authority.active.front().map(|record| record.id), Some(1));
        assert_eq!(
            authority.active.back().map(|record| record.id),
            Some(MAX_ACTIVE_ZONE_MARKERS as u64)
        );
        assert!(authority
            .active
            .iter()
            .all(|record| record.exit_code.is_none()));
    }

    #[test]
    fn completion_after_cap_retirement_is_ignored_instead_of_revived() {
        let mut authority = ZoneChromeAuthority::new(Some(NONCE));
        for id in 0..=MAX_ACTIVE_ZONE_MARKERS as u64 {
            authority.begin_zone(id);
        }
        let ids_before = authority
            .active
            .iter()
            .map(|record| record.id)
            .collect::<Vec<_>>();

        authority.record_completed(record(0));

        assert!(authority.record(0).is_none());
        assert_eq!(
            authority
                .active
                .iter()
                .map(|record| record.id)
                .collect::<Vec<_>>(),
            ids_before
        );
        assert_eq!(authority.active.len(), MAX_ACTIVE_ZONE_MARKERS);
    }

    #[test]
    fn max_seen_watermark_survives_retire_and_clear() {
        let mut authority = ZoneChromeAuthority::new(Some(NONCE));
        authority.begin_zone(40);
        authority.retire_prefix(1);
        authority.begin_zone(40);
        authority.begin_zone(39);
        assert!(authority.active.is_empty());
        assert_eq!(authority.max_seen_id, Some(40));

        authority.begin_zone(41);
        authority.clear();
        authority.begin_zone(41);
        authority.begin_zone(40);
        assert!(authority.active.is_empty());
        assert_eq!(authority.max_seen_id, Some(41));

        authority.begin_zone(42);
        assert_eq!(authority.active.back().map(|record| record.id), Some(42));
        assert_eq!(authority.max_seen_id, Some(42));
    }

    #[test]
    fn configured_limit_keeps_newest_pending_inside_the_same_bound() {
        let mut authority = ZoneChromeAuthority::new(Some(NONCE));
        for id in 0..100 {
            authority.begin_zone(id);
            authority.record_completed(record(id));
        }
        authority.begin_zone(100);
        authority.enforce_limit(100);

        assert_eq!(authority.active.len(), 100);
        assert!(authority.record(0).is_none());
        assert_eq!(authority.active.front().map(|record| record.id), Some(1));
        assert_eq!(authority.active.back().map(|record| record.id), Some(100));
        assert!(
            !authority.is_completed(100),
            "the pending prompt is retained"
        );
    }

    #[test]
    fn config_100_keeps_100_records_but_99_completed_markers_plus_pending() {
        let mut records = super::super::UnifiedZoneStore::new();
        let mut authority = ZoneChromeAuthority::new(Some(NONCE));
        for id in 0..100 {
            authority.begin_zone(id);
            authority.record_completed(record(id));
            let evicted = super::super::record_unified_zone(
                &mut records,
                super::super::CompletedCommandRecord {
                    id,
                    cmd: format!("command-{id}"),
                    exit_code: Some(0),
                    start_time_ms: None,
                    end_time_ms: None,
                    duration_ms: None,
                    cwd: None,
                    is_background: false,
                },
                100,
            );
            authority.retire_ids(&evicted);
        }
        authority.begin_zone(100);
        authority.enforce_limit(100);

        assert_eq!(records.records.len(), 100);
        assert_eq!(authority.active.len(), 100);
        assert_eq!(records.records.front().map(|record| record.id), Some(0));
        assert!(authority.record(0).is_none());
        assert_eq!(authority.active.back().map(|record| record.id), Some(100));
        assert!(!authority.is_completed(100));
    }

    #[test]
    fn ed3_retires_only_fully_known_completed_spans_above_cutoff() {
        let mut authority = make_authority(&[1, 2, 3, 4]);
        authority.row_spans.insert(
            1,
            ZoneRowSpan {
                row_epoch: 7,
                head: 0,
                end_exclusive: Some(100),
            },
        );
        authority.row_spans.insert(
            2,
            ZoneRowSpan {
                row_epoch: 7,
                head: 99,
                end_exclusive: Some(100),
            },
        );
        authority.row_spans.insert(
            3,
            ZoneRowSpan {
                row_epoch: 7,
                head: 100,
                end_exclusive: Some(101),
            },
        );
        authority.row_spans.insert(
            4,
            ZoneRowSpan {
                row_epoch: 7,
                head: 99,
                end_exclusive: Some(101),
            },
        );

        assert_eq!(authority.retire_completed_before(7, 100), [1, 2]);
        assert!(authority.record(1).is_none());
        assert!(authority.record(2).is_none());
        assert!(authority.record(3).is_some());
        assert!(authority.record(4).is_some());
    }

    #[test]
    fn ed3_preserves_unknown_stale_epoch_and_pending_spans() {
        let mut authority = make_authority(&[1, 2]);
        authority.begin_zone(3);
        authority.row_spans.insert(
            1,
            ZoneRowSpan {
                row_epoch: 8,
                head: 0,
                end_exclusive: None,
            },
        );
        authority.row_spans.insert(
            2,
            ZoneRowSpan {
                row_epoch: 7,
                head: 0,
                end_exclusive: Some(5),
            },
        );
        authority.row_spans.insert(
            3,
            ZoneRowSpan {
                row_epoch: 8,
                head: 0,
                end_exclusive: Some(5),
            },
        );

        assert!(authority.retire_completed_before(8, 10).is_empty());
        assert!(authority.record(1).is_some(), "unknown end is conservative");
        assert!(authority.record(2).is_some(), "stale epoch is unusable");
        assert!(
            authority.record(3).is_some(),
            "pending is never ED3-retired"
        );
    }

    #[test]
    fn natural_ring_floor_retires_known_heads_strictly_below_it() {
        let mut authority = make_authority(&[1, 2]);
        authority.begin_zone(3);
        for (id, head) in [(1, 9), (2, 10), (3, 8)] {
            authority.row_spans.insert(
                id,
                ZoneRowSpan {
                    row_epoch: 4,
                    head,
                    end_exclusive: None,
                },
            );
        }

        assert_eq!(authority.retire_heads_before(4, 10), [1, 3]);
        assert!(authority.record(1).is_none());
        assert!(authority.record(2).is_some(), "head at the floor survives");
        assert!(
            authority.record(3).is_none(),
            "evicted pending heads fail closed"
        );
    }

    #[test]
    fn calibrated_floor_retires_evicted_heads_and_quarantined_unknown_completed_ids() {
        let mut authority = make_authority(&[1, 2, 3, 4]);
        authority.begin_zone(5);
        authority.observe_head(1, 7, 9);
        authority.observe_head(2, 7, 10);
        authority.observe_head(4, 6, 100);
        authority.quarantine_eviction_drift();

        assert_eq!(authority.reconcile_calibrated_floor(7, 10), [1, 3, 4]);
        assert!(authority.record(1).is_none());
        assert!(authority.record(2).is_some());
        assert!(authority.record(3).is_none());
        assert!(
            authority.record(4).is_none(),
            "stale-epoch authority is unknown"
        );
        assert!(authority.record(5).is_some(), "the new pending A survives");

        let stale_three = uri(3);
        assert!(scan_visible_rows(
            layout(80, 7),
            20,
            [Some(stale_three.as_str())],
            &mut authority,
            &mut KnownHeadCache::default(),
        )
        .is_empty());
    }

    #[test]
    fn new_pending_anchor_still_calibrates_after_capacity_quarantine() {
        let mut authority = make_authority(&[1]);
        authority.begin_zone(2);
        authority.quarantine_eviction_drift();
        let two = uri(2);
        let projection = projection_from_calibration_anchor(
            CalibrationAnchor {
                zone_id: 2,
                ring_row: 410,
                row_epoch: 5,
            },
            5,
            0.0,
            100.0,
            4.0,
            [None, Some(two.as_str())],
            &authority,
        )
        .unwrap();
        authority.reconcile_calibrated_floor(5, projection.ring_lower);

        assert_eq!(projection.row_offset, 405);
        assert!(authority.record(1).is_none());
        assert!(authority.record(2).is_some());
    }

    #[test]
    fn canonical_transitions_stamp_current_epoch_and_close_prior_span() {
        let mut authority = make_authority(&[1, 2]);
        let one = uri(1);
        let two = uri(2);
        let mut cache = KnownHeadCache::default();
        scan_visible_rows(
            layout(80, 9),
            50,
            [None, Some(one.as_str()), None, Some(two.as_str())],
            &mut authority,
            &mut cache,
        );

        assert_eq!(
            authority.row_spans.get(&1),
            Some(&ZoneRowSpan {
                row_epoch: 9,
                head: 51,
                end_exclusive: Some(53),
            })
        );
        assert_eq!(
            authority.row_spans.get(&2),
            Some(&ZoneRowSpan {
                row_epoch: 9,
                head: 53,
                end_exclusive: None,
            })
        );
    }

    #[test]
    fn predecessor_index_seeds_a_long_zone_mid_viewport_without_history_scan() {
        let mut authority = make_authority(&[1, 2]);
        authority.observe_head(1, 6, 10);
        authority.observe_head(2, 6, 10_000);

        let lines = scan_visible_rows(
            layout(80, 6),
            5_000,
            [None, None, None],
            &mut authority,
            &mut KnownHeadCache::default(),
        );
        assert_eq!(
            lines
                .iter()
                .map(|line| (line.absolute_row, line.zone_id, line.is_head))
                .collect::<Vec<_>>(),
            [(5_000, 1, false), (5_001, 1, false), (5_002, 1, false)]
        );
        assert_eq!(authority.row_spans[&1].end_exclusive, Some(10_000));
        assert_eq!(authority.zone_containing_row(6, 10_000), Some(2));
        assert_eq!(authority.zone_containing_row(6, 9_999), Some(1));
    }

    #[test]
    fn out_of_order_head_observation_closes_both_neighbor_spans() {
        let mut authority = make_authority(&[1, 2, 3]);
        authority.observe_head(1, 4, 10);
        authority.observe_head(3, 4, 30);
        authority.observe_head(2, 4, 20);

        assert_eq!(authority.row_spans[&1].end_exclusive, Some(20));
        assert_eq!(authority.row_spans[&2].end_exclusive, Some(30));
        assert_eq!(authority.row_spans[&3].end_exclusive, None);
    }

    #[test]
    fn trusted_bounds_reject_changed_floor_and_map_ring_range() {
        let projection = ViewportProjection {
            row_offset: 35,
            ring_lower: 40,
            row_epoch: 3,
        };
        assert_eq!(
            trusted_ring_bounds_from(projection, 5.0, 105.0, 81.0),
            Some(TrustedRingBounds {
                row_epoch: 3,
                retained_start: 40,
                retained_end_exclusive: 140,
                viewport_top: 116,
            })
        );
        assert_eq!(trusted_ring_bounds_from(projection, 6.0, 105.0, 81.0), None);
    }

    #[test]
    fn proven_zone_scroll_maps_a_current_epoch_head_into_the_scroll_range() {
        let projection = ViewportProjection {
            row_offset: 35,
            ring_lower: 40,
            row_epoch: 3,
        };
        let span = |row_epoch, head| {
            Some(ZoneRowSpan {
                row_epoch,
                head,
                end_exclusive: None,
            })
        };

        assert_eq!(
            proven_zone_scroll_value_from(projection, 3, span(3, 90), 5.0, 105.0, 81.0, 24.0),
            Some(55.0)
        );
        // The retained floor is inclusive: the oldest row the ring still holds
        // is a provable target, not a rejected one.
        assert_eq!(
            proven_zone_scroll_value_from(projection, 3, span(3, 40), 5.0, 105.0, 81.0, 24.0),
            Some(5.0)
        );
        // A head near the ring tail clamps to the maximum scroll position.
        assert_eq!(
            proven_zone_scroll_value_from(projection, 3, span(3, 139), 5.0, 105.0, 81.0, 24.0),
            Some(81.0)
        );
    }

    /// Every broken link in the proof chain must refuse the jump: no scroll
    /// target is ever guessed from a stale epoch, a missing span, or a head
    /// outside the retained ring.
    #[test]
    fn proven_zone_scroll_fails_closed_on_every_unproven_input() {
        let projection = ViewportProjection {
            row_offset: 35,
            ring_lower: 40,
            row_epoch: 3,
        };
        let span = |row_epoch, head| {
            Some(ZoneRowSpan {
                row_epoch,
                head,
                end_exclusive: None,
            })
        };

        for (current_epoch, span, lower, page_size) in [
            // Projection from a retired epoch. The span is recorded at the
            // current epoch, so only the projection's own epoch check can
            // refuse this row.
            (4, span(4, 90), 5.0, 24.0),
            // No recorded span for the zone.
            (3, None, 5.0, 24.0),
            // Span recorded at a stale epoch.
            (3, span(2, 90), 5.0, 24.0),
            // Head below the retained floor / at the exclusive end.
            (3, span(3, 39), 5.0, 24.0),
            (3, span(3, 140), 5.0, 24.0),
            // Ring floor moved since the projection was proven.
            (3, span(3, 90), 6.0, 24.0),
            // Unusable page size.
            (3, span(3, 90), 5.0, f64::NAN),
        ] {
            assert_eq!(
                proven_zone_scroll_value_from(
                    projection,
                    current_epoch,
                    span,
                    lower,
                    105.0,
                    81.0,
                    page_size
                ),
                None
            );
        }
    }

    /// The span index answers only for an id that still holds authority, so a
    /// retired zone cannot be scrolled to through a span left behind.
    #[test]
    fn trusted_span_follows_marker_authority_not_the_span_table() {
        let mut authority = make_authority(&[7]);
        authority.row_spans.insert(
            7,
            ZoneRowSpan {
                row_epoch: 3,
                head: 90,
                end_exclusive: None,
            },
        );
        assert_eq!(
            authority.trusted_span(7).map(|span| span.head),
            Some(90),
            "an id with authority answers with its recorded head"
        );
        assert_eq!(authority.trusted_span(8), None, "unknown id");

        // A span whose id never held (or no longer holds) marker authority is
        // not a scroll target, whichever side of the pair went missing.
        authority.row_spans.insert(
            9,
            ZoneRowSpan {
                row_epoch: 3,
                head: 120,
                end_exclusive: None,
            },
        );
        assert_eq!(authority.trusted_span(9), None);

        authority.retire_ids(&[7]);
        assert_eq!(authority.trusted_span(7), None);
    }

    #[test]
    fn contents_drift_treats_cursor_formula_as_lower_bound_until_forward_trim() {
        let projection = ViewportProjection {
            row_offset: 302,
            ring_lower: 302,
            row_epoch: 5,
        };
        for candidate in 293..=302 {
            let cursor = candidate + 36 - 1;
            assert_eq!(
                projection_drift_after_contents(projection, cursor, 0.0, 36.0, 5_000),
                ProjectionDrift::Stable,
                "candidate {candidate} is only a lower bound"
            );
        }
        assert_eq!(
            projection_drift_after_contents(projection, 303 + 36 - 1, 0.0, 36.0, 5_000),
            ProjectionDrift::ForwardTrim {
                retained_floor: 303
            }
        );

        let initial = ViewportProjection {
            row_offset: 0,
            ring_lower: 0,
            row_epoch: 1,
        };
        assert_eq!(
            projection_drift_after_contents(initial, 0, 0.0, 36.0, 5_000),
            ProjectionDrift::Stable,
            "unused rows in the initial grid are zero-offset slack, not drift"
        );
    }

    #[test]
    fn contents_drift_fails_closed_at_bounded_scrollback_capacity() {
        let projection = ViewportProjection {
            row_offset: 302,
            ring_lower: 302,
            row_epoch: 5,
        };
        for candidate in [293, 302] {
            assert_eq!(
                projection_drift_after_contents(projection, candidate + 100 - 1, 0.0, 100.0, 100),
                ProjectionDrift::UnprovenAtCapacity { proven_floor: None },
            );
        }
        assert_eq!(
            projection_drift_after_contents(projection, 303 + 100 - 1, 0.0, 100.0, 100),
            ProjectionDrift::UnprovenAtCapacity {
                proven_floor: Some(303)
            }
        );
        assert_eq!(
            projection_drift_after_contents(projection, 302 + 100 - 1, 0.0, 100.0, -1),
            ProjectionDrift::Stable,
            "an unlimited ring is never classified as saturated"
        );
    }

    #[test]
    fn wrong_nonce_unknown_id_and_order_regression_fail_closed() {
        let mut authority = make_authority(&[10, 20]);
        let wrong_nonce = "block://cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd/10";
        let unknown = uri(99);
        let twenty = uri(20);
        let ten = uri(10);
        let probes = [
            Some(wrong_nonce),
            Some(unknown.as_str()),
            Some(twenty.as_str()),
            Some(ten.as_str()),
        ];
        let lines = scan_visible_rows(
            layout(80, 0),
            0,
            probes,
            &mut authority,
            &mut KnownHeadCache::default(),
        );
        assert_eq!(
            lines.iter().map(|line| line.zone_id).collect::<Vec<_>>(),
            [20]
        );
    }

    #[test]
    fn cached_head_survives_guest_mismatch_but_not_authority_retirement() {
        let mut authority = make_authority(&[7]);
        let seven = uri(7);
        let mut cache = KnownHeadCache::default();
        let first = scan_visible_rows(
            layout(80, 0),
            40,
            [None, Some(seven.as_str())],
            &mut authority,
            &mut cache,
        );
        assert!(first[0].is_head);
        assert_eq!(first[0].absolute_row, 41);
        let guest = "https://guest.invalid/prompt";
        let second = scan_visible_rows(
            layout(80, 0),
            40,
            [None, Some(guest)],
            &mut authority,
            &mut cache,
        );
        assert_eq!(second[0].zone_id, 7);
        assert!(second[0].is_head);
        let mut retired = make_authority(&[7]);
        retired.retire_prefix(1);
        assert!(scan_visible_rows(
            layout(80, 0),
            40,
            [None, Some(guest)],
            &mut retired,
            &mut cache
        )
        .is_empty());
    }

    #[test]
    fn columns_and_rewrap_generation_invalidate_cached_rows() {
        let mut authority = make_authority(&[7]);
        let seven = uri(7);
        let mut cache = KnownHeadCache::default();
        assert_eq!(
            scan_visible_rows(
                layout(80, 0),
                9,
                [None, Some(seven.as_str())],
                &mut authority,
                &mut cache
            )
            .len(),
            1
        );
        assert!(
            scan_visible_rows(layout(79, 0), 9, [None, None], &mut authority, &mut cache)
                .is_empty()
        );
        assert_eq!(
            scan_visible_rows(
                layout(79, 0),
                9,
                [None, Some(seven.as_str())],
                &mut authority,
                &mut cache
            )
            .len(),
            1
        );
        assert!(
            scan_visible_rows(layout(79, 1), 9, [None, None], &mut authority, &mut cache)
                .is_empty()
        );
    }

    #[test]
    fn markerless_output_rows_continue_the_proven_visible_zone() {
        let mut authority = make_authority(&[7]);
        let seven = uri(7);
        let lines = scan_visible_rows(
            layout(80, 0),
            50,
            [None, Some(seven.as_str()), None, None],
            &mut authority,
            &mut KnownHeadCache::default(),
        );
        assert_eq!(
            lines
                .iter()
                .map(|line| (line.visible_row, line.zone_id, line.is_head))
                .collect::<Vec<_>>(),
            [(1, 7, true), (2, 7, false), (3, 7, false)]
        );
    }

    #[test]
    fn canonical_head_at_viewport_row_zero_is_real_head_not_mid_zone() {
        let mut authority = make_authority(&[7]);
        let seven = uri(7);
        let mut cache = KnownHeadCache::default();
        let lines = scan_visible_rows(
            layout(80, 3),
            50,
            [Some(seven.as_str()), None],
            &mut authority,
            &mut cache,
        );

        assert!(lines[0].is_head);
        assert_eq!(cache.heads.get(&50), Some(&7));
        assert_eq!(authority.row_spans.get(&7).map(|span| span.head), Some(50));
    }

    #[test]
    fn visible_canonical_anchor_calibrates_exact_non_bottom_projection() {
        let authority = make_authority(&[7]);
        let seven = uri(7);
        let probes = [None, None, Some(seven.as_str()), None];
        let anchor = CalibrationAnchor {
            zone_id: 7,
            ring_row: 307,
            row_epoch: 4,
        };
        assert_eq!(
            projection_from_calibration_anchor(anchor, 4, 0.0, 36.0, 3.0, probes, &authority,),
            Some(ViewportProjection {
                row_offset: 302,
                ring_lower: 302,
                row_epoch: 4,
            })
        );
    }

    #[test]
    fn calibration_rejects_stale_epoch_wrong_nonce_and_impossible_bounds() {
        let authority = make_authority(&[7]);
        let seven = uri(7);
        let wrong = "block://cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd/7";
        let anchor = CalibrationAnchor {
            zone_id: 7,
            ring_row: 3,
            row_epoch: 4,
        };
        let calibrate = |epoch, lower, upper, value, probe| {
            projection_from_calibration_anchor(
                anchor,
                epoch,
                lower,
                upper,
                value,
                [probe],
                &authority,
            )
        };
        assert_eq!(
            calibrate(4, 0.0, 6.0, 3.0, Some(seven.as_str())),
            Some(ViewportProjection {
                row_offset: 0,
                ring_lower: 0,
                row_epoch: 4,
            })
        );
        assert_eq!(calibrate(5, 0.0, 6.0, 3.0, Some(seven.as_str())), None);
        assert_eq!(calibrate(4, 0.0, 6.0, 3.0, Some(wrong)), None);
        assert_eq!(calibrate(4, 0.0, 6.0, 4.0, Some(seven.as_str())), None);
        assert_eq!(calibrate(4, f64::NAN, 6.0, 3.0, Some(seven.as_str())), None);
    }

    #[test]
    fn badge_landing_requires_an_entirely_blank_target_range() {
        assert!(badge_lands_on_blank_cells("   \t"));
        assert!(badge_lands_on_blank_cells(""));
        assert!(!badge_lands_on_blank_cells("  x  "));
        assert!(!badge_lands_on_blank_cells("\u{00a0}"));
        assert_eq!(badge_probe_start_col(0), 0);
        assert_eq!(badge_probe_start_col(1), 0);
        assert_eq!(badge_probe_start_col(30), 29);
    }

    #[test]
    fn scan_budget_is_exactly_one_probe_per_visible_row() {
        let mut authority = make_authority(&[1]);
        let one = uri(1);
        let mut probes = 0;
        let inputs = (0..73).map(|_| {
            probes += 1;
            Some(one.as_str())
        });
        let lines = scan_visible_rows(
            layout(120, 0),
            1000,
            inputs,
            &mut authority,
            &mut KnownHeadCache::default(),
        );
        assert_eq!(probes, 73);
        assert_eq!(lines.len(), 73);
    }

    /// The widening query is the only work permitted beyond one probe per row.
    /// It is asked at most once per scan, only where a transition follows a row
    /// that failed its own column-0 parse, and a `false` answer changes nothing.
    #[test]
    fn the_widening_query_is_bounded_and_a_refusal_changes_nothing() {
        let mut authority = make_authority(&[1, 2, 3]);
        let (one, two, three) = (uri(1), uri(2), uri(3));
        // Two transitions, each preceded by an unowned output row.
        let inputs = vec![
            Some(one.as_str()),
            None,
            Some(two.as_str()),
            None,
            Some(three.as_str()),
        ];
        let mut widening_queries = 0;
        let lines = scan_visible_rows_with(
            layout(120, 0),
            1000,
            inputs,
            &mut authority,
            &mut KnownHeadCache::default(),
            &mut |_, _| {
                widening_queries += 1;
                false
            },
        );
        assert_eq!(
            widening_queries, 2,
            "one query per transition that follows an unowned row, never per row"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.is_head)
                .map(|line| (line.zone_id, line.absolute_row))
                .collect::<Vec<_>>(),
            vec![(1, 1000), (2, 1002), (3, 1004)],
            "a refused widening leaves every head exactly where column 0 proved it"
        );
    }

    /// The loop skips rows whose marker order regresses, and the unowned flag
    /// is only updated on rows it actually pushes. Widening must therefore
    /// check that the last pushed row really is the row above this one, or a
    /// stale predecessor gets stolen across the gap.
    #[test]
    fn widening_refuses_a_predecessor_that_is_not_the_row_directly_above() {
        let mut authority = make_authority(&[1, 2, 3]);
        let (one, two, three) = (uri(1), uri(2), uri(3));
        let inputs = vec![
            // 1000: zone 2 opens.
            Some(two.as_str()),
            // 1001: zone 2 output, unowned column 0.
            None,
            // 1002: an older zone's marker — an order regression, skipped
            // entirely, so it never updates the unowned flag.
            Some(one.as_str()),
            // 1003: zone 3 opens. Its predecessor in `result` is 1001.
            Some(three.as_str()),
        ];
        let lines = scan_visible_rows_with(
            layout(120, 0),
            1000,
            inputs,
            &mut authority,
            &mut KnownHeadCache::default(),
            &mut |_, _| true,
        );
        assert!(
            authority.head_matches(3, 0, 1003),
            "zone 3 keeps the row its own marker proved"
        );
        assert!(
            authority.head_matches(2, 0, 1000),
            "zone 2 keeps its head and its continuation row"
        );
        assert_eq!(
            lines
                .iter()
                .find(|line| line.absolute_row == 1001)
                .map(|line| line.zone_id),
            Some(2)
        );
    }

    /// A transition whose predecessor owns its own column 0 has nothing
    /// unowned to widen onto, so no query is spent at all.
    #[test]
    fn an_adjacent_pair_of_column_zero_zones_spends_no_widening_query() {
        let mut authority = make_authority(&[1, 2]);
        let (one, two) = (uri(1), uri(2));
        let inputs = vec![Some(one.as_str()), Some(two.as_str())];
        let mut widening_queries = 0;
        let lines = scan_visible_rows_with(
            layout(120, 0),
            1000,
            inputs,
            &mut authority,
            &mut KnownHeadCache::default(),
            &mut |_, _| {
                widening_queries += 1;
                true
            },
        );
        assert_eq!(widening_queries, 0);
        assert_eq!(lines.iter().filter(|line| line.is_head).count(), 2);
    }

    /// A zone whose first row starts past column 0 has no canonical marker
    /// there, so the column-0 pass hands that row to the previous zone. The
    /// widening query moves the head back up exactly one row.
    #[test]
    fn a_head_widens_onto_the_unowned_row_the_zone_really_starts_on() {
        let mut authority = make_authority(&[1, 2]);
        let (one, two) = (uri(1), uri(2));
        // Row 0: zone 1. Row 1: zone 2's real first row, written past column 0
        // so its own probe finds nothing. Row 2: zone 2's marker at column 0.
        let inputs = vec![Some(one.as_str()), None, Some(two.as_str())];
        let lines = scan_visible_rows_with(
            layout(120, 0),
            1000,
            inputs,
            &mut authority,
            &mut KnownHeadCache::default(),
            &mut |visible_row, zone_id| visible_row == 1 && zone_id == 2,
        );
        assert_eq!(lines.len(), 3);
        assert_eq!((lines[1].zone_id, lines[1].is_head), (2, true));
        assert_eq!(
            (lines[2].zone_id, lines[2].is_head),
            (2, false),
            "the row whose head moved up is a continuation, not a second head"
        );
        assert!(authority.head_matches(2, 0, 1001));
    }

    /// A validated head survives a guest OSC 8 overwriting its column 0, so
    /// its own probe fails too. Widening onto it would displace it and delete
    /// its span outright — no separator, no badge for a zone that has both.
    #[test]
    fn widening_never_steals_a_row_that_is_already_a_validated_head() {
        let mut authority = make_authority(&[1, 2]);
        let two = uri(2);
        // Zone 1's head is validated at row 1001 and cached, but its column 0
        // now parses as nothing (a guest marker overwrote it).
        authority.observe_head(1, 0, 1001);
        let mut cache = KnownHeadCache::default();
        cache.prepare(layout(120, 0), &authority);
        cache.heads.insert(1001, 1);
        let inputs = vec![None, None, Some(two.as_str())];
        let lines = scan_visible_rows_with(
            layout(120, 0),
            1000,
            inputs,
            &mut authority,
            &mut cache,
            &mut |_, _| true,
        );
        assert!(
            authority.head_matches(1, 0, 1001),
            "zone 1 keeps the head it had already proved"
        );
        assert_eq!(
            lines
                .iter()
                .find(|line| line.absolute_row == 1001)
                .map(|line| (line.zone_id, line.is_head)),
            Some((1, true))
        );
        assert!(
            authority.head_matches(2, 0, 1002),
            "zone 2 keeps its own row"
        );
    }

    #[test]
    #[ignore = "requires a mapped GTK/VTE surface; run under Xvfb"]
    fn real_vte_osc8_row_probe_and_rewrap_smoke() {
        if gtk::init().is_err() {
            return;
        }
        let terminal = Terminal::new();
        terminal.set_allow_hyperlink(true);
        terminal.set_scrollback_lines(100);
        terminal.set_size(20, 4);
        let window = gtk::Window::builder()
            .default_width(240)
            .default_height(120)
            .child(&terminal)
            .build();
        window.present();
        let settle = || {
            let context = gtk::glib::MainContext::default();
            let started = std::time::Instant::now();
            while started.elapsed() < std::time::Duration::from_millis(100) {
                while context.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        settle();
        let marker = format!(
            "\x1b]8;;{}\x1b\\marked row with enough linked text to rewrap\x1b]8;;\x1b\\\r\n",
            uri(4)
        );
        terminal.feed(marker.as_bytes());
        settle();
        let border = gtk::prelude::ScrollableExt::border(&terminal);
        let x = border.as_ref().map_or(0.0, |b| f64::from(b.left()))
            + terminal.char_width().max(1) as f64 * 0.5;
        let y = border.as_ref().map_or(0.0, |b| f64::from(b.top()))
            + terminal.char_height().max(1) as f64 * 0.5;
        assert_eq!(
            terminal.check_hyperlink_at(x, y).as_deref(),
            Some(uri(4).as_str())
        );
        terminal.set_size(10, 4);
        assert_eq!(terminal.column_count(), 10);
        settle();
        assert_eq!(
            terminal.check_hyperlink_at(x, y).as_deref(),
            Some(uri(4).as_str())
        );
        window.close();
    }

    #[test]
    #[ignore = "requires a mapped GTK/VTE surface; run under Xvfb"]
    fn real_vte_non_bottom_anchor_calibrates_and_wide_badge_probe_fails_closed() {
        if gtk::init().is_err() {
            return;
        }
        let terminal = Terminal::new();
        terminal.set_allow_hyperlink(true);
        terminal.set_scrollback_lines(100);
        terminal.set_size(20, 6);
        let window = gtk::Window::builder()
            .default_width(240)
            .default_height(140)
            .child(&terminal)
            .build();
        window.present();
        let settle = || {
            let context = gtk::glib::MainContext::default();
            let started = std::time::Instant::now();
            while started.elapsed() < std::time::Duration::from_millis(100) {
                while context.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        settle();

        for row in 0..300 {
            terminal.feed(format!("pre-reset-{row:03}\r\n").as_bytes());
        }
        settle();
        terminal.reset(true, true);
        terminal.feed(b"\x1b[H\x1b[2Jone\r\ntwo\r\n");
        settle();

        let mut authority = ZoneChromeAuthority::new(Some(NONCE));
        authority.begin_zone(7);
        let anchor = CalibrationAnchor {
            zone_id: 7,
            ring_row: terminal.cursor_position().1,
            row_epoch: 1,
        };
        terminal.feed(format!("\x1b]8;;{}\x1b\\CALIBRATE\x1b]8;;\x1b\\", uri(7)).as_bytes());
        settle();

        let border = gtk::prelude::ScrollableExt::border(&terminal);
        let content_x = border
            .as_ref()
            .map_or(0.0, |border| f64::from(border.left()));
        let content_y = border
            .as_ref()
            .map_or(0.0, |border| f64::from(border.top()));
        let cell_width = terminal.char_width().max(1) as f64;
        let cell_height = terminal.char_height().max(1) as f64;
        let probes = (0..terminal.row_count() as usize)
            .map(|row| {
                terminal.check_hyperlink_at(
                    content_x + cell_width * 0.5,
                    content_y + (row as f64 + 0.5) * cell_height,
                )
            })
            .collect::<Vec<_>>();
        let adjustment = terminal.vadjustment().unwrap();
        let projection = projection_from_calibration_anchor(
            anchor,
            1,
            adjustment.lower(),
            adjustment.upper(),
            adjustment.value(),
            probes.iter().map(Option::as_deref),
            &authority,
        )
        .expect("the visible canonical marker proves a projection");
        let cursor_guess = (anchor.ring_row + 1 - adjustment.upper().ceil() as i64).max(0);
        assert_ne!(
            projection.row_offset, cursor_guess,
            "the non-bottom cursor exposes the old cursor+1-upper bug"
        );
        assert!(adjustment.upper() < terminal.scrollback_lines() as f64);
        assert_eq!(
            projection_drift_after_contents(
                projection,
                terminal.cursor_position().1,
                adjustment.lower(),
                adjustment.upper(),
                terminal.scrollback_lines(),
            ),
            ProjectionDrift::Stable,
            "an unfilled ring must not hide chrome for ordinary cursor slack"
        );
        let marker_visible_row = probes
            .iter()
            .position(|probe| probe.as_deref() == Some(uri(7).as_str()))
            .unwrap() as i64;
        assert_eq!(
            finite_floor_i64(adjustment.value()).unwrap()
                + marker_visible_row
                + projection.row_offset,
            anchor.ring_row
        );

        // At bounded capacity a coalesced trim+CUP snapshot can underestimate
        // the exact new offset. Production must drop the old projection and
        // wait for another canonical A anchor rather than publishing the
        // cursor-derived lower bound.
        let mut trim_then_cup = String::new();
        trim_then_cup.push_str("\r\n");
        for row in 0..180 {
            trim_then_cup.push_str(&format!("trim-{row:03}\r\n"));
        }
        trim_then_cup.push_str("\x1b[2;1H");
        terminal.feed(trim_then_cup.as_bytes());
        settle();
        let final_adjustment = terminal.vadjustment().unwrap();
        let final_candidate =
            (terminal.cursor_position().1 + 1 - final_adjustment.upper().ceil() as i64).max(0);
        assert_eq!(
            projection_drift_after_contents(
                projection,
                terminal.cursor_position().1,
                final_adjustment.lower(),
                final_adjustment.upper(),
                terminal.scrollback_lines(),
            ),
            ProjectionDrift::UnprovenAtCapacity {
                proven_floor: Some(final_candidate)
            }
        );

        authority.quarantine_eviction_drift();
        authority.begin_zone(8);
        let post_trim_anchor = CalibrationAnchor {
            zone_id: 8,
            ring_row: terminal.cursor_position().1,
            row_epoch: 1,
        };
        terminal.feed(format!("\x1b]8;;{}\x1b\\POSTTRIM\x1b]8;;\x1b\\", uri(8)).as_bytes());
        settle();
        let post_trim_probes = (0..terminal.row_count() as usize)
            .map(|row| {
                terminal.check_hyperlink_at(
                    content_x + cell_width * 0.5,
                    content_y + (row as f64 + 0.5) * cell_height,
                )
            })
            .collect::<Vec<_>>();
        let post_trim_adjustment = terminal.vadjustment().unwrap();
        let post_trim_projection = projection_from_calibration_anchor(
            post_trim_anchor,
            1,
            post_trim_adjustment.lower(),
            post_trim_adjustment.upper(),
            post_trim_adjustment.value(),
            post_trim_probes.iter().map(Option::as_deref),
            &authority,
        )
        .expect("a new canonical marker recalibrates after trim");
        authority.reconcile_calibrated_floor(1, post_trim_projection.ring_lower);
        assert!(post_trim_projection.row_offset > projection.row_offset);
        assert!(authority.record(8).is_some(), "the new pending A survives");

        terminal.feed(b"\r\n\xe7\x95\x8c");
        settle();
        let wide_row = terminal.cursor_position().1;
        let columns = terminal.column_count();
        let conservative = terminal
            .text_range_format(
                vte4::Format::Text,
                wide_row,
                badge_probe_start_col(1),
                wide_row,
                columns,
            )
            .0
            .expect("the wide-glyph row is retained");
        assert!(!badge_lands_on_blank_cells(conservative.as_str()));
        window.close();
    }
}
