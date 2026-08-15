//! Bounded, repo-scoped long-term memory for the native ASCII organism.
//!
//! Only structured counters, a short bounded transition-ordering window, and
//! a quantized life-state snapshot are stored. Transition ids are opaque and
//! carry no PID; command text and output never enter this file. Every durable
//! update is a cross-process transaction performed by Anvil's persistence
//! worker: lock, bounded reread, apply one delta, and private atomic replacement.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::organism::{CommandKind, LifeState, RepoWorkState};

const SCHEMA_VERSION: u32 = 1;
const MAX_MEMORY_BYTES: u64 = 512 * 1024;
const MAX_DAILY_RECORDS: usize = 64;
const MAX_OBSERVATIONS: usize = 256;
const MAX_OBSERVATIONS_PER_RECORD: usize = 64;
/// Individual build durations are capped before aggregation so a clock jump
/// or pathological command can never distort the per-repo baseline.
const MAX_TRACKED_BUILD_MS: u64 = 21_600_000;
const MAX_RECENT_EVENT_IDS: usize = 512;
const MAX_EVENT_ID_BYTES: usize = 96;
const MAX_PENDING_MEMORY_EVENTS: usize = 256;
const MAX_ACKNOWLEDGED_MEMORY_EVENTS: usize = MAX_PENDING_MEMORY_EVENTS * 4;
const MAX_REPO_BYTES: usize = 2 * 1024;
const MAX_GIT_POINTER_BYTES: u64 = 16 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const LOCK_POLL: Duration = Duration::from_millis(20);
const LIFE_SCALE: f32 = 1_000.0;
const CIRCADIAN_BUCKET_COUNT: usize = 8;
const CIRCADIAN_LOOKBACK_DAYS: i64 = 28;
const MIN_CIRCADIAN_ACTIVE_DAYS: usize = 3;
const MIN_CIRCADIAN_SAMPLES: u64 = 6;
const MAX_RECENT_GROWTH_DAYS: usize = 64;
const ADULT_MIN_DAYS: u32 = 7;
const SEASONED_MIN_DAYS: u32 = 60;
const SEASONED_MIN_RECOVERIES: u32 = 12;

/// One local wall-clock instant reduced to the only two scalars the organism
/// needs. The bucket is one of eight three-hour spans beginning at midnight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LocalCircadianTime {
    pub(crate) day: i64,
    pub(crate) bucket: u8,
}

/// A learned nine-hour working window. The mask always contains one winning
/// three-hour bucket and its two circular neighbours, so night-shift windows
/// can naturally wrap across midnight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CircadianProfile {
    mask: u8,
}

impl CircadianProfile {
    pub(crate) fn contains(self, bucket: u8) -> bool {
        bucket < CIRCADIAN_BUCKET_COUNT as u8 && self.mask & (1_u8 << bucket) != 0
    }

    /// Returns a stable local-day key for the learned work session containing
    /// `local`. A window that begins before midnight and wraps into the next
    /// civil day therefore keeps one key across all three of its buckets.
    pub(crate) fn session_day(self, local: LocalCircadianTime) -> i64 {
        let start_bucket = (0..CIRCADIAN_BUCKET_COUNT as u8)
            .find(|&bucket| {
                let previous =
                    (bucket + CIRCADIAN_BUCKET_COUNT as u8 - 1) % CIRCADIAN_BUCKET_COUNT as u8;
                self.contains(bucket) && !self.contains(previous)
            })
            .expect("a circadian profile always has one window start");

        if start_bucket > local.bucket {
            local.day.saturating_sub(1)
        } else {
            local.day
        }
    }

    #[cfg(test)]
    const fn mask(self) -> u8 {
        self.mask
    }

    #[cfg(test)]
    pub(crate) const fn from_mask(mask: u8) -> Self {
        Self { mask }
    }
}

/// A coarse lifetime stage derived only from bounded, content-free counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GrowthStage {
    Juvenile,
    Adult,
    Seasoned,
}

impl GrowthStage {
    pub(crate) const fn from_counts(days_seen: u32, lifetime_recoveries: u32) -> Self {
        if days_seen < ADULT_MIN_DAYS {
            Self::Juvenile
        } else if days_seen >= SEASONED_MIN_DAYS && lifetime_recoveries >= SEASONED_MIN_RECOVERIES {
            Self::Seasoned
        } else {
            Self::Adult
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GrowthProgress {
    pub(crate) days_seen: u32,
    pub(crate) lifetime_recoveries: u32,
}

impl GrowthProgress {
    pub(crate) const fn stage(self) -> GrowthStage {
        GrowthStage::from_counts(self.days_seen, self.lifetime_recoveries)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LifeSnapshot {
    energy: u16,
    mood: u16,
    curiosity: u16,
    boredom: u16,
    stress: u16,
    social_need: u16,
    attachment: u16,
    confidence: u16,
}

impl Default for LifeSnapshot {
    fn default() -> Self {
        Self::from_state(LifeState::default())
    }
}

impl LifeSnapshot {
    fn from_state(state: LifeState) -> Self {
        Self {
            energy: quantize(state.energy),
            mood: quantize(state.mood),
            curiosity: quantize(state.curiosity),
            boredom: quantize(state.boredom),
            stress: quantize(state.stress),
            social_need: quantize(state.social_need),
            attachment: quantize(state.attachment),
            confidence: quantize(state.confidence),
        }
    }

    fn state(self) -> LifeState {
        LifeState {
            energy: dequantize(self.energy),
            mood: dequantize(self.mood),
            curiosity: dequantize(self.curiosity),
            boredom: dequantize(self.boredom),
            stress: dequantize(self.stress),
            social_need: dequantize(self.social_need),
            attachment: dequantize(self.attachment),
            confidence: dequantize(self.confidence),
        }
    }

    fn valid(self) -> bool {
        [
            self.energy,
            self.mood,
            self.curiosity,
            self.boredom,
            self.stress,
            self.social_need,
            self.attachment,
            self.confidence,
        ]
        .into_iter()
        .all(|value| value <= LIFE_SCALE as u16)
    }
}

fn quantize(value: f32) -> u16 {
    let value = if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.5
    };
    (value * LIFE_SCALE).round() as u16
}

fn dequantize(value: u16) -> f32 {
    f32::from(value.min(LIFE_SCALE as u16)) / LIFE_SCALE
}

/// A v1 scalar that remembers whether it was decoded from disk or supplied by
/// `#[serde(default)]`. New files always serialize a plain integer; the bit is
/// only an in-memory migration aid for older v1 records whose ordered suffix
/// can still reconstruct the newly introduced aggregate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MigratingCounter {
    value: u32,
    present: bool,
}

impl MigratingCounter {
    const fn new(value: u32) -> Self {
        Self {
            value,
            present: true,
        }
    }

    fn saturating_increment(&mut self) {
        self.value = self.value.saturating_add(1);
        self.present = true;
    }

    fn saturating_add(&mut self, amount: u32) {
        self.value = self.value.saturating_add(amount);
        self.present = true;
    }

    fn mark_present(&mut self) {
        self.present = true;
    }
}

impl Serialize for MigratingCounter {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u32(self.value)
    }
}

impl<'de> Deserialize<'de> for MigratingCounter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        u32::deserialize(deserializer).map(Self::new)
    }
}

/// The bounded exact set behind `days_seen`. Once a day falls at or before
/// `compacted_through` (or below the oldest entry of a full window), later
/// events for it remain valid memory events but are deliberately growth-neutral.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct GrowthDayLedger {
    compacted_through: Option<i64>,
    recent: Vec<i64>,
}

impl<'de> Deserialize<'de> for GrowthDayLedger {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const FIELDS: &[&str] = &["compacted_through", "recent"];

        struct LedgerVisitor;

        impl<'de> serde::de::Visitor<'de> for LedgerVisitor {
            type Value = GrowthDayLedger;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a complete growth-day ledger")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                // The outer option is field presence; the inner option is the
                // explicit JSON null used before the first compaction.
                let mut compacted_through: Option<Option<i64>> = None;
                let mut recent = None;
                while let Some(field) = map.next_key::<String>()? {
                    match field.as_str() {
                        "compacted_through" => {
                            if compacted_through.is_some() {
                                return Err(serde::de::Error::duplicate_field("compacted_through"));
                            }
                            compacted_through = Some(map.next_value()?);
                        }
                        "recent" => {
                            if recent.is_some() {
                                return Err(serde::de::Error::duplicate_field("recent"));
                            }
                            recent = Some(map.next_value()?);
                        }
                        _ => return Err(serde::de::Error::unknown_field(&field, FIELDS)),
                    }
                }

                Ok(GrowthDayLedger {
                    compacted_through: compacted_through
                        .ok_or_else(|| serde::de::Error::missing_field("compacted_through"))?,
                    recent: recent.ok_or_else(|| serde::de::Error::missing_field("recent"))?,
                })
            }
        }

        // `Option<T>` normally treats an absent field exactly like an explicit
        // `null`. This hand-written visitor keeps those two cases distinct.
        deserializer.deserialize_struct("GrowthDayLedger", FIELDS, LedgerVisitor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GrowthDayObservation {
    New,
    Seen,
    Closed,
}

impl GrowthDayObservation {
    const fn is_open(self) -> bool {
        !matches!(self, Self::Closed)
    }
}

impl GrowthDayLedger {
    fn observe(&mut self, day: i64) -> GrowthDayObservation {
        match self.recent.binary_search(&day) {
            Ok(_) => return GrowthDayObservation::Seen,
            Err(_) if self.is_closed(day) => return GrowthDayObservation::Closed,
            Err(index) => self.recent.insert(index, day),
        }

        if self.recent.len() > MAX_RECENT_GROWTH_DAYS {
            let compacted = self.recent.remove(0);
            self.compacted_through = Some(
                self.compacted_through
                    .map_or(compacted, |prior| prior.max(compacted)),
            );
        }
        GrowthDayObservation::New
    }

    fn contains(&self, day: i64) -> bool {
        self.recent.binary_search(&day).is_ok()
    }

    fn is_closed(&self, day: i64) -> bool {
        self.compacted_through
            .is_some_and(|compacted| day <= compacted)
            || (self.recent.len() == MAX_RECENT_GROWTH_DAYS
                && self.recent.first().is_some_and(|oldest| day < *oldest))
    }

    /// Anvilt exact ordering for `day` and every earlier day. Daily records
    /// are bounded independently from the distinct-day ledger, so evicting a
    /// record with build history also loses the evidence needed to merge a
    /// later out-of-order recovery with the already-counted episode. Closing
    /// the prefix keeps those late events valid but growth-neutral.
    fn close_through(&mut self, day: i64) {
        let compacted = self
            .compacted_through
            .map_or(day, |previous| previous.max(day));
        self.compacted_through = Some(compacted);
        let first_open = self.recent.partition_point(|recent| *recent <= compacted);
        self.recent.drain(..first_open);
    }

    fn validate(&self, days_seen: u32) -> bool {
        let retained = self.recent.len() as u32;
        let shape_valid = self.recent.len() <= MAX_RECENT_GROWTH_DAYS
            && self.recent.windows(2).all(|days| days[0] < days[1])
            && self
                .compacted_through
                .is_none_or(|compacted| self.recent.iter().all(|day| *day > compacted));
        let history_valid = if days_seen < retained {
            false
        } else if days_seen == retained && self.recent.is_empty() {
            self.compacted_through.is_none()
        } else if days_seen == retained {
            // With no known evictions, this is either a fresh ledger or the
            // conservative v1 migration boundary immediately before the
            // oldest retained semantic day. A wider gap would incorrectly
            // close unseen days that a future out-of-order event could prove.
            self.compacted_through.is_none()
                || self.compacted_through == self.recent.first().and_then(|day| day.checked_sub(1))
        } else {
            // Exact days may also be discarded before this set reaches 64:
            // the independently bounded repo/day window can evict one repo's
            // build ordering while newer days remain exact. Such a compressed
            // prefix must always carry a cursor; `shape_valid` proves every
            // retained day lies strictly after it.
            self.compacted_through.is_some()
        };
        shape_valid && history_valid
    }
}

/// Like `MigratingCounter`, this serializes only the value; `present` exists
/// solely to distinguish an old v1 file from a partially written new shape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MigratingGrowthDayLedger {
    value: GrowthDayLedger,
    present: bool,
}

impl MigratingGrowthDayLedger {
    fn new(value: GrowthDayLedger) -> Self {
        Self {
            value,
            present: true,
        }
    }
}

impl Serialize for MigratingGrowthDayLedger {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MigratingGrowthDayLedger {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        GrowthDayLedger::deserialize(deserializer).map(Self::new)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DailyStats {
    day: i64,
    repo: String,
    build_failures: u32,
    build_successes: u32,
    git_pushes: u32,
    open_failures: u32,
    open_failure_at_ms: Option<u64>,
    last_recovery_duration_ms: Option<u64>,
    recovered_pending_push: bool,
    /// Same-day failure-to-success transitions. A repeated one-failure flip
    /// is a content-free hint that a build/test may be intermittent rather
    /// than evidence that the human is stuck.
    #[serde(default)]
    failure_success_flips: MigratingCounter,
    /// Snapshot before the first retained observation. Version 1 was never
    /// shipped without these fields, but defaults let development fixtures and
    /// the standalone seed format migrate without weakening strict decoding.
    #[serde(default)]
    baseline: Option<StatsBaseline>,
    #[serde(default)]
    observations: Vec<Observation>,
    /// Saturating aggregates of successful build/test wall times — scalars
    /// only, never command text. Accumulated directly by `apply_event` and
    /// deliberately outside the observation replay, mirroring how late
    /// compacted events only bump monotonic counters.
    #[serde(default)]
    build_duration_sum_ms: u64,
    #[serde(default)]
    build_duration_count: u32,
    /// Counts of repo-scoped build/test and push completions in eight local
    /// three-hour wall-clock buckets. This monotone aggregate deliberately
    /// lives outside observation replay and contains no command content.
    #[serde(default)]
    activity_buckets: [u16; CIRCADIAN_BUCKET_COUNT],
}

impl DailyStats {
    fn new(day: i64, repo: String) -> Self {
        Self {
            day,
            repo,
            build_failures: 0,
            build_successes: 0,
            git_pushes: 0,
            open_failures: 0,
            open_failure_at_ms: None,
            last_recovery_duration_ms: None,
            recovered_pending_push: false,
            failure_success_flips: MigratingCounter::new(0),
            baseline: None,
            observations: Vec::new(),
            build_duration_sum_ms: 0,
            build_duration_count: 0,
            activity_buckets: [0; CIRCADIAN_BUCKET_COUNT],
        }
    }

    fn baseline(&self) -> StatsBaseline {
        StatsBaseline {
            build_failures: self.build_failures,
            build_successes: self.build_successes,
            git_pushes: self.git_pushes,
            open_failures: self.open_failures,
            open_failure_at_ms: self.open_failure_at_ms,
            last_recovery_duration_ms: self.last_recovery_duration_ms,
            recovered_pending_push: self.recovered_pending_push,
            failure_success_flips: self.failure_success_flips,
            compacted_through: self
                .baseline
                .as_ref()
                .and_then(|baseline| baseline.compacted_through.clone()),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct StatsBaseline {
    build_failures: u32,
    build_successes: u32,
    git_pushes: u32,
    open_failures: u32,
    open_failure_at_ms: Option<u64>,
    last_recovery_duration_ms: Option<u64>,
    recovered_pending_push: bool,
    #[serde(default)]
    failure_success_flips: MigratingCounter,
    /// Events at or before this total-order cursor were folded into the
    /// aggregate prefix. A pathologically late writer is ignored instead of
    /// being replayed on the wrong side of a compacted success or push.
    #[serde(default)]
    compacted_through: Option<ObservationCursor>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct ObservationCursor {
    at_ms: u64,
    id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ObservationKind {
    BuildFailure,
    BuildSuccess,
    GitPush,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    id: String,
    at_ms: u64,
    kind: ObservationKind,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DiskMemory {
    version: u32,
    life: LifeSnapshot,
    life_updated_at_ms: u64,
    #[serde(default)]
    life_updated_event_id: String,
    /// Global lifetime growth counters. They remain outside the bounded
    /// repo/day window so record eviction can never make the organism younger.
    #[serde(default)]
    days_seen: MigratingCounter,
    #[serde(default)]
    lifetime_recoveries: MigratingCounter,
    /// Exact recent distinct-day membership, independent of how many repos
    /// happen to occupy the 64 bounded `DailyStats` records.
    #[serde(default)]
    growth_days: MigratingGrowthDayLedger,
    /// Bounded durable idempotence tokens. They contain no command data or PID
    /// and outlive observation compaction long enough to cover every
    /// in-process pending/retry window.
    #[serde(default)]
    recent_event_ids: Vec<String>,
    days: Vec<DailyStats>,
}

impl Default for DiskMemory {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            life: LifeSnapshot::default(),
            life_updated_at_ms: 0,
            life_updated_event_id: String::new(),
            days_seen: MigratingCounter::new(0),
            lifetime_recoveries: MigratingCounter::new(0),
            growth_days: MigratingGrowthDayLedger::new(GrowthDayLedger::default()),
            recent_event_ids: Vec::new(),
            days: Vec::new(),
        }
    }
}

impl DiskMemory {
    fn validate(&self) -> io::Result<()> {
        if self.version != SCHEMA_VERSION {
            return Err(invalid(format!(
                "unsupported ASCII organism memory version {} (expected {SCHEMA_VERSION})",
                self.version
            )));
        }
        if !self.life.valid() {
            return Err(invalid("ASCII organism memory has an invalid life state"));
        }
        if !self.life_updated_event_id.is_empty() && !valid_event_id(&self.life_updated_event_id) {
            return Err(invalid(
                "ASCII organism memory has an invalid life-state cursor",
            ));
        }
        if !self.days_seen.present || !self.lifetime_recoveries.present || !self.growth_days.present
        {
            return Err(invalid(
                "ASCII organism memory has an incomplete growth ledger",
            ));
        }
        if !self.growth_days.value.validate(self.days_seen.value) {
            return Err(invalid(
                "ASCII organism memory has an invalid growth ledger",
            ));
        }
        if self.days_seen.value == 0 && self.lifetime_recoveries.value != 0 {
            return Err(invalid(
                "ASCII organism memory has recoveries without a work day",
            ));
        }
        if self.days.len() > MAX_DAILY_RECORDS {
            return Err(invalid("ASCII organism memory has too many daily records"));
        }
        if self.days_seen.value > self.growth_days.value.recent.len() as u32
            && self.days.len() != MAX_DAILY_RECORDS
        {
            return Err(invalid(
                "ASCII organism memory has a compressed growth ledger without a full daily window",
            ));
        }
        if self
            .growth_days
            .value
            .compacted_through
            .is_some_and(|cursor| {
                self.days
                    .iter()
                    .map(|stats| stats.day)
                    .max()
                    .is_none_or(|latest| cursor > latest)
            })
        {
            return Err(invalid(
                "ASCII organism memory has a growth cursor beyond retained daily history",
            ));
        }
        if self.days.len() < MAX_DAILY_RECORDS
            && self.growth_days.value.recent.iter().any(|day| {
                !self
                    .days
                    .iter()
                    .any(|stats| stats.day == *day && daily_stats_has_semantic_activity(stats))
            })
        {
            return Err(invalid(
                "ASCII organism memory has a recent growth day without semantic evidence",
            ));
        }
        if self.recent_event_ids.len() > MAX_RECENT_EVENT_IDS {
            return Err(invalid(
                "ASCII organism memory has too many idempotence tokens",
            ));
        }
        let mut recent_ids = HashSet::with_capacity(self.recent_event_ids.len());
        for id in &self.recent_event_ids {
            if !valid_event_id(id) || !recent_ids.insert(id.as_str()) {
                return Err(invalid(
                    "ASCII organism memory has an invalid or duplicate idempotence token",
                ));
            }
        }

        let mut seen = HashSet::with_capacity(self.days.len());
        let mut open_recoveries = 0_u64;
        let mut observation_ids = HashSet::new();
        let mut observation_count = 0usize;
        for stats in &self.days {
            if !valid_repo_id(&stats.repo) {
                return Err(invalid(
                    "ASCII organism memory has an invalid repository identifier",
                ));
            }
            if !seen.insert((stats.day, stats.repo.as_str())) {
                return Err(invalid(
                    "ASCII organism memory has duplicate day/repository records",
                ));
            }
            if daily_stats_has_semantic_activity(stats) {
                let tracked = self.growth_days.value.contains(stats.day);
                if !tracked && !self.growth_days.value.is_closed(stats.day) {
                    return Err(invalid("ASCII organism memory has an untracked growth day"));
                }
            }
            if !failure_state_valid(
                stats.build_failures,
                stats.open_failures,
                stats.open_failure_at_ms,
                stats.recovered_pending_push,
                stats.last_recovery_duration_ms,
            ) {
                return Err(invalid(
                    "ASCII organism memory has an inconsistent failure streak",
                ));
            }
            if stats.baseline.as_ref().is_some_and(|baseline| {
                !failure_state_valid(
                    baseline.build_failures,
                    baseline.open_failures,
                    baseline.open_failure_at_ms,
                    baseline.recovered_pending_push,
                    baseline.last_recovery_duration_ms,
                )
            }) {
                return Err(invalid(
                    "ASCII organism memory has an inconsistent baseline failure state",
                ));
            }
            if !stats.failure_success_flips.present
                || stats.failure_success_flips.value > stats.build_failures
                || stats.failure_success_flips.value > stats.build_successes
            {
                return Err(invalid(
                    "ASCII organism memory has an inconsistent failure-success flip count",
                ));
            }
            if stats.baseline.as_ref().is_some_and(|baseline| {
                !baseline.failure_success_flips.present
                    || baseline.failure_success_flips.value > baseline.build_failures
                    || baseline.failure_success_flips.value > baseline.build_successes
            }) {
                return Err(invalid(
                    "ASCII organism memory has an inconsistent baseline flip count",
                ));
            }
            if stats.build_duration_count > stats.build_successes
                || stats.build_duration_sum_ms
                    > u64::from(stats.build_duration_count).saturating_mul(MAX_TRACKED_BUILD_MS)
            {
                return Err(invalid(
                    "ASCII organism memory has an inconsistent build-duration aggregate",
                ));
            }
            if stats.observations.len() > MAX_OBSERVATIONS_PER_RECORD
                || (!stats.observations.is_empty() && stats.baseline.is_none())
            {
                return Err(invalid(
                    "ASCII organism memory has an invalid observation window",
                ));
            }
            observation_count = observation_count
                .checked_add(stats.observations.len())
                .ok_or_else(|| invalid("ASCII organism observation count overflow"))?;
            let compacted_through = stats
                .baseline
                .as_ref()
                .and_then(|baseline| baseline.compacted_through.as_ref());
            if let Some(cursor) = compacted_through {
                let baseline = stats
                    .baseline
                    .as_ref()
                    .expect("a compaction cursor belongs to a baseline");
                if !valid_event_id(&cursor.id)
                    || !observation_ids.insert(cursor.id.as_str())
                    || (baseline.build_failures == 0
                        && baseline.build_successes == 0
                        && baseline.git_pushes == 0)
                {
                    return Err(invalid(
                        "ASCII organism memory has an invalid compaction cursor",
                    ));
                }
            }
            for observation in &stats.observations {
                let cursor = ObservationCursor {
                    at_ms: observation.at_ms,
                    id: observation.id.clone(),
                };
                if !valid_event_id(&observation.id)
                    || compacted_through.is_some_and(|compacted| &cursor <= compacted)
                    || !observation_ids.insert(observation.id.as_str())
                {
                    return Err(invalid(
                        "ASCII organism memory has an invalid or duplicate observation id",
                    ));
                }
            }
            if stats.baseline.is_some() && !summary_matches_observations(stats) {
                return Err(invalid(
                    "ASCII organism memory counters do not match their observation log",
                ));
            }
            if self.growth_days.value.contains(stats.day) {
                open_recoveries =
                    open_recoveries.saturating_add(u64::from(stats.failure_success_flips.value));
            }
        }
        if observation_count > MAX_OBSERVATIONS {
            return Err(invalid(
                "ASCII organism memory has too many retained observations",
            ));
        }
        if u64::from(self.lifetime_recoveries.value) < open_recoveries.min(u64::from(u32::MAX)) {
            return Err(invalid(
                "ASCII organism memory has an inconsistent lifetime recovery count",
            ));
        }
        Ok(())
    }

    fn stats(&self, day: i64, repo: &str) -> Option<&DailyStats> {
        self.days
            .iter()
            .find(|stats| stats.day == day && stats.repo == repo)
    }

    fn has_event_id(&self, id: &str) -> bool {
        self.recent_event_ids.iter().any(|recent| recent == id)
            || self.days.iter().any(|stats| {
                stats
                    .baseline
                    .as_ref()
                    .and_then(|baseline| baseline.compacted_through.as_ref())
                    .is_some_and(|cursor| cursor.id == id)
                    || stats
                        .observations
                        .iter()
                        .any(|observation| observation.id == id)
            })
    }

    fn remember_event_id(&mut self, id: &str) {
        if self.recent_event_ids.iter().any(|recent| recent == id) {
            return;
        }
        self.recent_event_ids.push(id.to_owned());
        if self.recent_event_ids.len() > MAX_RECENT_EVENT_IDS {
            let overflow = self.recent_event_ids.len() - MAX_RECENT_EVENT_IDS;
            self.recent_event_ids.drain(..overflow);
        }
    }

    fn observe_growth_day(&mut self, day: i64) -> GrowthDayObservation {
        let observation = self.growth_days.value.observe(day);
        if observation == GrowthDayObservation::New {
            self.days_seen.saturating_increment();
        }
        observation
    }

    fn stats_mut(&mut self, day: i64, repo: &str) -> &mut DailyStats {
        if let Some(index) = self
            .days
            .iter()
            .position(|stats| stats.day == day && stats.repo == repo)
        {
            return &mut self.days[index];
        }

        self.days.push(DailyStats::new(day, repo.to_owned()));
        self.prune_around(day, repo);
        let index = self
            .days
            .iter()
            .position(|stats| stats.day == day && stats.repo == repo)
            .expect("new organism memory record must survive bounded pruning");
        &mut self.days[index]
    }

    fn prune_around(&mut self, inserted_day: i64, inserted_repo: &str) {
        if self.days.len() <= MAX_DAILY_RECORDS {
            return;
        }
        self.days.sort_by(|left, right| {
            left.day
                .cmp(&right.day)
                .then_with(|| left.repo.cmp(&right.repo))
        });
        let inserted = self
            .days
            .iter()
            .position(|stats| stats.day == inserted_day && stats.repo == inserted_repo)
            .expect("inserted organism record exists before pruning");
        let remove = if inserted == 0 { 1 } else { 0 };
        let evicted = self.days.remove(remove);
        if evicted.build_failures > 0 || evicted.build_successes > 0 {
            self.growth_days.value.close_through(evicted.day);
        }
    }
}

fn daily_stats_has_semantic_activity(stats: &DailyStats) -> bool {
    stats.build_failures > 0
        || stats.build_successes > 0
        || stats.git_pushes > 0
        || stats.activity_buckets.iter().any(|count| *count > 0)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn valid_repo_id(repo: &str) -> bool {
    !repo.is_empty()
        && repo.len() <= MAX_REPO_BYTES
        && Path::new(repo).is_absolute()
        && !repo.chars().any(char::is_control)
        && !crate::review_input::contains_visual_spoofing(repo)
}

fn valid_event_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_EVENT_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn failure_state_valid(
    build_failures: u32,
    open_failures: u32,
    open_failure_at_ms: Option<u64>,
    recovered_pending_push: bool,
    last_recovery_duration_ms: Option<u64>,
) -> bool {
    open_failures <= build_failures
        && (open_failures == 0) == open_failure_at_ms.is_none()
        && (open_failures == 0 || !recovered_pending_push)
        && (!recovered_pending_push || last_recovery_duration_ms.is_some())
}

#[derive(Clone, Debug)]
pub(crate) struct MemoryEvent {
    id: String,
    at_ms: u64,
    day: i64,
    /// Frozen with `day` when the event is created, so a queued transaction is
    /// never reinterpreted after a timezone or DST transition.
    activity_bucket: u8,
    repo: Option<String>,
    kind: CommandKind,
    exit_code: Option<i32>,
    /// Wall time of the finished command; a content-free scalar feeding the
    /// per-repo build-duration baseline. Never persisted as an event — only
    /// the bounded aggregates reach disk.
    duration_ms: Option<u64>,
    life: LifeSnapshot,
}

impl MemoryEvent {
    /// Freeze one lifecycle completion to the caller's single wall-clock
    /// sample. Day and activity bucket are always derived from that same
    /// millisecond, so neither can cross midnight independently of reduction.
    pub(crate) fn at_ms_for_repo(
        at_ms: u64,
        kind: CommandKind,
        exit_code: Option<i32>,
        repo: Option<String>,
        life: LifeState,
        duration_ms: Option<u64>,
    ) -> Self {
        let local = local_circadian_time_at_ms(at_ms);
        Self {
            id: next_event_id(),
            at_ms,
            day: local.day,
            activity_bucket: local.bucket,
            repo,
            kind,
            exit_code,
            duration_ms,
            life: LifeSnapshot::from_state(life),
        }
    }

    pub(crate) const fn day(&self) -> i64 {
        self.day
    }

    #[cfg(test)]
    fn fixed(
        at_ms: u64,
        day: i64,
        repo: Option<&str>,
        kind: CommandKind,
        exit_code: Option<i32>,
        life: LifeState,
    ) -> Self {
        let local = local_circadian_time_at_ms(at_ms);
        Self {
            id: next_event_id(),
            at_ms,
            day,
            activity_bucket: local.bucket,
            repo: repo.map(str::to_owned),
            kind,
            exit_code,
            duration_ms: None,
            life: LifeSnapshot::from_state(life),
        }
    }

    #[cfg(test)]
    fn with_duration(mut self, duration_ms: Option<u64>) -> Self {
        self.duration_ms = duration_ms;
        self
    }

    #[cfg(test)]
    fn with_activity_bucket(mut self, bucket: u8) -> Self {
        assert!(bucket < CIRCADIAN_BUCKET_COUNT as u8);
        self.activity_bucket = bucket;
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MemoryInsight {
    /// Whether the incoming observation still lies inside the retained suffix
    /// whose exact total-order position can be reconstructed. Event-local
    /// fields below must be treated as unknown when this is false.
    pub(crate) event_order_exact: bool,
    /// Event-position failure depth used to shape the immediate reaction when
    /// `event_order_exact` is true. This is deliberately distinct from
    /// `current_work`, which describes the final repo/day state after every
    /// retained observation has been replayed.
    pub(crate) open_failures: u32,
    pub(crate) recovered_failures: u32,
    pub(crate) push_after_recovery: bool,
    pub(crate) faster_than_yesterday: bool,
    /// This success was at least the third same-day failure-to-success flip,
    /// and only one failure was open immediately before it.
    pub(crate) likely_flaky: bool,
    /// Authoritative post-replay repo/day state after this transaction. Every
    /// semantic return path fills it, including duplicate, unknown-outcome,
    /// failed-push, out-of-order, and compacted-prefix cases.
    pub(crate) current_work: RepoWorkState,
    pub(crate) repo_remembered: bool,
    /// Mean successful build/test wall time across every remembered day of
    /// this repo, once at least three samples exist.
    pub(crate) typical_build_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepoContext {
    pub(crate) day: i64,
    pub(crate) repo: String,
    pub(crate) work: RepoWorkState,
    /// Today's build/test successes in this repo, for habituated reactions.
    pub(crate) successes_today: u32,
    /// Today's build/test failures in this repo, for sensitized reactions.
    pub(crate) failures_today: u32,
    /// Distinct remembered day records for this repo. Derived on read from the
    /// bounded `days` window — never persisted separately. Zero means the
    /// organism has no memory of ever working in this checkout.
    pub(crate) familiarity_days: u32,
}

pub(crate) struct OrganismMemory {
    path: PathBuf,
    memory: DiskMemory,
    session_events: VecDeque<MemoryEvent>,
}

impl OrganismMemory {
    pub(crate) fn load_default() -> io::Result<Self> {
        Self::load(crate::config::default_ascii_organism_memory_path())
    }

    fn load(path: PathBuf) -> io::Result<Self> {
        Ok(Self {
            memory: read_memory(&path)?,
            path,
            session_events: VecDeque::new(),
        })
    }

    pub(crate) fn life_state(&self) -> LifeState {
        self.memory.life.state()
    }

    pub(crate) fn growth_progress(&self) -> GrowthProgress {
        GrowthProgress {
            days_seen: self.memory.days_seen.value,
            lifetime_recoveries: self.memory.lifetime_recoveries.value,
        }
    }

    /// Infer one global working-hours profile from completed recent days in
    /// every remembered repository. Keeping this window-global matches the
    /// shared life/tick clock; a pane-local schedule would make the simulation
    /// depend on which pane happened to claim a wall-clock slice.
    pub(crate) fn circadian_profile_at(&self, at_ms: u64) -> Option<CircadianProfile> {
        infer_circadian_profile(&self.memory, local_circadian_time_at_ms(at_ms).day)
    }

    pub(crate) fn refresh(&mut self) -> io::Result<()> {
        acknowledge_session_events(&self.path, &mut self.session_events);
        let mut memory = read_memory(&self.path)?;
        for event in &self.session_events {
            apply_event(&mut memory, event);
        }
        self.memory = memory;
        Ok(())
    }

    /// Resolve the repository against the caller's already-frozen local day.
    /// A lifecycle callback must not read the wall clock twice across midnight.
    pub(crate) fn context_for_day(&mut self, cwd: Option<&str>, day: i64) -> Option<RepoContext> {
        let cwd = cwd?;
        // Repository identity is deliberately resolved afresh. A permanent
        // cwd cache goes stale across `git init`/worktree removal and lets
        // attacker-controlled OSC cwd strings grow process memory without a
        // bound. This path runs only for build/test/push lifecycle events.
        let repo = git_repo_root_for(cwd)?;
        Some(self.context_for_repo_day(&repo, day))
    }

    /// Rebuild a command's reducer context from an already-resolved repo key.
    /// Finish callbacks use this after refreshing memory, avoiding a second
    /// Git lookup while still incorporating work completed by other windows
    /// during the command.
    pub(crate) fn context_for_repo_day(&self, repo: &str, day: i64) -> RepoContext {
        let (work, successes_today, failures_today) = self
            .memory
            .stats(day, repo)
            .map(|stats| {
                (
                    RepoWorkState::new(
                        stats.open_failures,
                        stats.recovered_pending_push,
                        stats.failure_success_flips.value,
                    ),
                    stats.build_successes,
                    stats.build_failures,
                )
            })
            .unwrap_or((RepoWorkState::default(), 0, 0));
        let familiarity_days = self
            .memory
            .days
            .iter()
            .filter(|stats| stats.repo == repo)
            .count() as u32;
        RepoContext {
            day,
            repo: repo.to_owned(),
            work,
            successes_today,
            failures_today,
            familiarity_days,
        }
    }

    fn apply_local(&mut self, event: &MemoryEvent) -> MemoryInsight {
        if self.session_events.len() == MAX_PENDING_MEMORY_EVENTS {
            self.session_events.pop_front();
        }
        self.session_events.push_back(event.clone());
        apply_event(&mut self.memory, event)
    }

    /// Admit one update to the durable queue and local optimistic view as a
    /// single operation. A persistence-worker scheduling error is still safe
    /// when the event was retained for shutdown/later retry. If the bounded
    /// event queue itself rejects the update, return only a throw-away preview
    /// insight so refresh can never resurrect an event absent from disk.
    pub(crate) fn apply_and_enqueue(
        &mut self,
        event: MemoryEvent,
    ) -> (MemoryInsight, io::Result<()>, bool) {
        let outcome = enqueue_event(self.path.clone(), event.clone());
        self.apply_enqueue_outcome(event, outcome)
    }

    fn apply_enqueue_outcome(
        &mut self,
        event: MemoryEvent,
        outcome: EventEnqueue,
    ) -> (MemoryInsight, io::Result<()>, bool) {
        match outcome {
            EventEnqueue::Retained(result) => (self.apply_local(&event), result, true),
            EventEnqueue::Rejected(error) => {
                let mut preview = self.memory.clone();
                (apply_event(&mut preview, &event), Err(error), false)
            }
        }
    }

    #[cfg(test)]
    fn memory(&self) -> &DiskMemory {
        &self.memory
    }
}

fn apply_event(memory: &mut DiskMemory, event: &MemoryEvent) -> MemoryInsight {
    if event.at_ms > memory.life_updated_at_ms
        || (event.at_ms == memory.life_updated_at_ms
            && event.id.as_str() > memory.life_updated_event_id.as_str())
    {
        memory.life = event.life;
        memory.life_updated_at_ms = event.at_ms;
        memory.life_updated_event_id = event.id.clone();
    }

    let Some(repo) = event.repo.as_deref() else {
        return MemoryInsight::default();
    };
    let mut insight = MemoryInsight {
        repo_remembered: memory.days.iter().any(|stats| stats.repo == repo),
        ..MemoryInsight::default()
    };

    // Activity is deliberately narrower than arbitrary shell usage: only the
    // two repo-scoped semantic command kinds already admitted by the organism
    // can teach its working-hours rhythm.
    if !matches!(event.kind, CommandKind::BuildOrTest | CommandKind::GitPush) {
        return insight;
    }
    insight.current_work = repo_work_state(memory, event.day, repo);
    // This check precedes every capacity mutation. It also covers meaningful
    // commands without an ordered observation (unknown build outcome or failed
    // push), whose activity bucket must remain exactly-once across retries.
    if memory.has_event_id(&event.id) {
        return insight;
    }
    let recoveries_before = memory
        .stats(event.day, repo)
        .map(|stats| stats.failure_success_flips.value)
        .unwrap_or(0);
    let growth_day = memory.observe_growth_day(event.day);
    // Snapshot repo duration history before the activity aggregate creates a
    // new day record. At the 64-record bound, `stats_mut` may evict the oldest
    // record; the current command must still compare against history that was
    // present when it arrived.
    let (duration_sum, duration_count) = memory
        .days
        .iter()
        .filter(|stats| stats.repo == repo)
        .fold((0u64, 0u64), |(sum, count), stats| {
            (
                sum.saturating_add(stats.build_duration_sum_ms),
                count.saturating_add(u64::from(stats.build_duration_count)),
            )
        });
    if duration_count >= 3 {
        insight.typical_build_ms = Some(duration_sum / duration_count);
    }
    {
        let stats = memory.stats_mut(event.day, repo);
        let bucket = stats
            .activity_buckets
            .get_mut(usize::from(event.activity_bucket))
            .expect("a local circadian bucket is always in 0..8");
        *bucket = bucket.saturating_add(1);
    }

    let observation_kind = match (event.kind, event.exit_code) {
        (CommandKind::BuildOrTest, Some(code)) if code != 0 => Some(ObservationKind::BuildFailure),
        (CommandKind::BuildOrTest, Some(0)) => Some(ObservationKind::BuildSuccess),
        (CommandKind::GitPush, Some(0)) => Some(ObservationKind::GitPush),
        _ => None,
    };
    let Some(kind) = observation_kind else {
        memory.remember_event_id(&event.id);
        return insight;
    };
    // Duration aggregates live outside the replayed observation window: they
    // are bounded monotone scalars, accumulated exactly once per event id.
    if kind == ObservationKind::BuildSuccess {
        if let Some(duration) = event.duration_ms {
            let stats = memory.stats_mut(event.day, repo);
            // Once the count saturates the pair must stop moving together,
            // or the sum could outgrow the count*cap validation invariant
            // and wedge persistence fail-closed.
            if stats.build_duration_count < u32::MAX {
                stats.build_duration_sum_ms = stats
                    .build_duration_sum_ms
                    .saturating_add(duration.min(MAX_TRACKED_BUILD_MS));
                stats.build_duration_count = stats.build_duration_count.saturating_add(1);
            }
        }
    }

    let replay = {
        let stats = memory.stats_mut(event.day, repo);
        let cursor = ObservationCursor {
            at_ms: event.at_ms,
            id: event.id.clone(),
        };
        if stats
            .baseline
            .as_ref()
            .and_then(|baseline| baseline.compacted_through.as_ref())
            .is_some_and(|compacted| &cursor <= compacted)
        {
            // Exact ordering before the compacted cursor is unavailable. A
            // genuinely new late event still contributes its monotonic count;
            // durable recent_event_ids distinguishes it from a retry. Keep the
            // newer prefix's derived failure/recovery state unchanged.
            let baseline = stats
                .baseline
                .as_mut()
                .expect("a compaction cursor always belongs to a baseline");
            match kind {
                ObservationKind::BuildFailure => {
                    baseline.build_failures = baseline.build_failures.saturating_add(1);
                }
                ObservationKind::BuildSuccess => {
                    baseline.build_successes = baseline.build_successes.saturating_add(1);
                }
                ObservationKind::GitPush => {
                    baseline.git_pushes = baseline.git_pushes.saturating_add(1);
                }
            }
            replay_observations(stats, None);
            insight.open_failures = stats.open_failures;
            None
        } else {
            if stats.baseline.is_none() {
                stats.baseline = Some(stats.baseline());
            }
            stats.observations.push(Observation {
                id: event.id.clone(),
                at_ms: event.at_ms,
                kind,
            });
            let replay = replay_observations(stats, Some(&event.id));
            if stats.observations.len() > MAX_OBSERVATIONS_PER_RECORD {
                // Insert and total-order the incoming event before advancing
                // the cursor, including an older event at exact capacity.
                compact_oldest_observations(stats);
            }
            Some(replay)
        }
    };
    if growth_day.is_open() {
        let recoveries_after = memory
            .stats(event.day, repo)
            .map(|stats| stats.failure_success_flips.value)
            .unwrap_or(recoveries_before);
        memory
            .lifetime_recoveries
            .saturating_add(recoveries_after.saturating_sub(recoveries_before));
    }
    memory.remember_event_id(&event.id);
    compact_global_observations(memory);
    if let Some(replay) = replay {
        insight.event_order_exact = true;
        insight.recovered_failures = replay.recovered_failures;
        insight.open_failures = replay.open_failures;
        insight.push_after_recovery = replay.push_after_recovery;
        insight.likely_flaky = replay.likely_flaky;
    }
    insight.faster_than_yesterday =
        insight.push_after_recovery && faster_than_previous_day(memory, event.day, repo);
    insight.current_work = repo_work_state(memory, event.day, repo);
    insight
}

fn repo_work_state(memory: &DiskMemory, day: i64, repo: &str) -> RepoWorkState {
    memory
        .stats(day, repo)
        .map(|stats| {
            RepoWorkState::new(
                stats.open_failures,
                stats.recovered_pending_push,
                stats.failure_success_flips.value,
            )
        })
        .unwrap_or_default()
}

fn infer_circadian_profile(memory: &DiskMemory, today: i64) -> Option<CircadianProfile> {
    let oldest_day = today.saturating_sub(CIRCADIAN_LOOKBACK_DAYS);
    let mut counts = [0_u64; CIRCADIAN_BUCKET_COUNT];
    let mut active_days = HashSet::new();

    for stats in &memory.days {
        // Today is deliberately excluded: a learned schedule remains stable
        // throughout the day instead of being pulled toward each new command.
        // Future records (clock corrections/timezone changes) also cannot teach
        // the current profile until local time catches up with them.
        if stats.day < oldest_day || stats.day >= today {
            continue;
        }
        let mut record_total = 0_u64;
        for (total, count) in counts.iter_mut().zip(stats.activity_buckets) {
            *total = total.saturating_add(u64::from(count));
            record_total = record_total.saturating_add(u64::from(count));
        }
        if record_total > 0 {
            active_days.insert(stats.day);
        }
    }

    let total = counts.iter().copied().fold(0_u64, u64::saturating_add);
    if active_days.len() < MIN_CIRCADIAN_ACTIVE_DAYS || total < MIN_CIRCADIAN_SAMPLES {
        return None;
    }

    // Score every circular nine-hour window as the candidate centre plus its
    // immediate neighbours. Prefer the busier centre on an equal window sum;
    // a complete tie keeps the lower centre for deterministic reconstruction.
    let mut best_center = 0_usize;
    let mut best_window = 0_u64;
    let mut best_centre_count = 0_u64;
    for center in 0..CIRCADIAN_BUCKET_COUNT {
        let previous = (center + CIRCADIAN_BUCKET_COUNT - 1) % CIRCADIAN_BUCKET_COUNT;
        let next = (center + 1) % CIRCADIAN_BUCKET_COUNT;
        let window = counts[previous]
            .saturating_add(counts[center])
            .saturating_add(counts[next]);
        if window > best_window || (window == best_window && counts[center] > best_centre_count) {
            best_center = center;
            best_window = window;
            best_centre_count = counts[center];
        }
    }

    // A near-uniform or heavily fragmented rhythm has no honest habitual
    // window. Fail neutral instead of manufacturing one from the tie-breaker.
    if best_window.saturating_mul(2) <= total {
        return None;
    }

    let previous = (best_center + CIRCADIAN_BUCKET_COUNT - 1) % CIRCADIAN_BUCKET_COUNT;
    let next = (best_center + 1) % CIRCADIAN_BUCKET_COUNT;
    Some(CircadianProfile {
        mask: (1_u8 << previous) | (1_u8 << best_center) | (1_u8 << next),
    })
}

fn compact_global_observations(memory: &mut DiskMemory) {
    while memory
        .days
        .iter()
        .map(|stats| stats.observations.len())
        .sum::<usize>()
        > MAX_OBSERVATIONS
    {
        let Some(index) = memory
            .days
            .iter()
            .enumerate()
            .filter(|(_, stats)| stats.observations.len() >= 2)
            .min_by(|(_, left), (_, right)| {
                let left = left
                    .observations
                    .iter()
                    .min_by(|left, right| {
                        left.at_ms
                            .cmp(&right.at_ms)
                            .then_with(|| left.id.cmp(&right.id))
                    })
                    .map(|event| (event.at_ms, &event.id));
                let right = right
                    .observations
                    .iter()
                    .min_by(|left, right| {
                        left.at_ms
                            .cmp(&right.at_ms)
                            .then_with(|| left.id.cmp(&right.id))
                    })
                    .map(|event| (event.at_ms, &event.id));
                left.cmp(&right)
            })
            .map(|(index, _)| index)
        else {
            // With at most MAX_DAILY_RECORDS records and more than
            // MAX_OBSERVATIONS observations, pigeonhole guarantees a foldable
            // record. Keep this guard fail-closed if constants ever change.
            break;
        };
        compact_oldest_observations(&mut memory.days[index]);
    }
}

fn compact_oldest_observations(stats: &mut DailyStats) {
    if stats.observations.len() < 2 {
        return;
    }
    stats.observations.sort_by(|left, right| {
        left.at_ms
            .cmp(&right.at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    let keep = stats.observations.split_off(stats.observations.len() / 2);
    let folded_events = std::mem::replace(&mut stats.observations, keep);
    let compacted_through = folded_events.last().map(|event| ObservationCursor {
        at_ms: event.at_ms,
        id: event.id.clone(),
    });
    let retained = std::mem::take(&mut stats.observations);
    stats.observations = folded_events;
    replay_observations(stats, None);
    let mut baseline = stats.baseline();
    if compacted_through > baseline.compacted_through {
        baseline.compacted_through = compacted_through;
    }
    stats.baseline = Some(baseline);
    stats.observations = retained;
    replay_observations(stats, None);
}

#[derive(Clone, Copy, Debug, Default)]
struct ReplayInsight {
    open_failures: u32,
    recovered_failures: u32,
    push_after_recovery: bool,
    likely_flaky: bool,
}

fn replay_observations(stats: &mut DailyStats, target_id: Option<&str>) -> ReplayInsight {
    stats.observations.sort_by(|left, right| {
        left.at_ms
            .cmp(&right.at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    let baseline = stats.baseline.clone().unwrap_or_default();
    stats.build_failures = baseline.build_failures;
    stats.build_successes = baseline.build_successes;
    stats.git_pushes = baseline.git_pushes;
    stats.open_failures = baseline.open_failures;
    stats.open_failure_at_ms = baseline.open_failure_at_ms;
    stats.last_recovery_duration_ms = baseline.last_recovery_duration_ms;
    stats.recovered_pending_push = baseline.recovered_pending_push;
    stats.failure_success_flips = baseline.failure_success_flips;

    let mut insight = ReplayInsight::default();
    for observation in &stats.observations {
        match observation.kind {
            ObservationKind::BuildFailure => {
                stats.build_failures = stats.build_failures.saturating_add(1);
                stats.open_failures = stats.open_failures.saturating_add(1);
                stats.open_failure_at_ms.get_or_insert(observation.at_ms);
                stats.recovered_pending_push = false;
                if target_id == Some(observation.id.as_str()) {
                    insight.open_failures = stats.open_failures;
                }
            }
            ObservationKind::BuildSuccess => {
                stats.build_successes = stats.build_successes.saturating_add(1);
                let recovered_failures = stats.open_failures;
                if recovered_failures > 0 {
                    stats.failure_success_flips.saturating_increment();
                }
                if let Some(started) = stats.open_failure_at_ms.take() {
                    stats.last_recovery_duration_ms = observation.at_ms.checked_sub(started);
                    stats.recovered_pending_push = stats.last_recovery_duration_ms.is_some();
                }
                stats.open_failures = 0;
                if target_id == Some(observation.id.as_str()) {
                    insight.recovered_failures = recovered_failures;
                    insight.likely_flaky =
                        recovered_failures == 1 && stats.failure_success_flips.value >= 3;
                }
            }
            ObservationKind::GitPush => {
                stats.git_pushes = stats.git_pushes.saturating_add(1);
                let after_recovery = std::mem::take(&mut stats.recovered_pending_push);
                if target_id == Some(observation.id.as_str()) {
                    insight.push_after_recovery = after_recovery;
                }
            }
        }
    }
    insight
}

fn summary_matches_observations(stats: &DailyStats) -> bool {
    let expected = stats.baseline();
    let mut rebuilt = stats.clone();
    replay_observations(&mut rebuilt, None);
    rebuilt.baseline() == expected
}

fn faster_than_previous_day(memory: &DiskMemory, day: i64, repo: &str) -> bool {
    let Some(previous_day) = day.checked_sub(1) else {
        return false;
    };
    let (Some(today), Some(previous)) = (memory.stats(day, repo), memory.stats(previous_day, repo))
    else {
        return false;
    };
    if today.build_successes == 0 || previous.build_successes == 0 {
        return false;
    }
    match (
        today.last_recovery_duration_ms,
        previous.last_recovery_duration_ms,
    ) {
        (Some(today), Some(previous)) => today < previous,
        (None, None) => today.build_failures < previous.build_failures,
        _ => false,
    }
}

fn read_memory(path: &Path) -> io::Result<DiskMemory> {
    let json = match jterm_core::snapshot_file::read_bounded_private(path, MAX_MEMORY_BYTES) {
        Ok(json) => json,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(DiskMemory::default()),
        Err(error) => return Err(error),
    };
    // Decode the typed form first. In particular, serde's struct decoder
    // rejects duplicate security-sensitive fields; routing through Value
    // first would collapse duplicates with last-one-wins semantics.
    let mut memory: DiskMemory = serde_json::from_str(&json)
        .map_err(|error| invalid(format!("invalid ASCII organism memory: {error}")))?;
    // These two legacy-defaulted scalars form one aggregate. Decode the
    // strict typed form first (above), then inspect field presence separately:
    // a released writer always emits both, while an old seed emits neither.
    // In particular this keeps a real zero-duration sample `(0, 1)` from
    // being silently changed to `(0, 0)` by deleting only its count field.
    let raw: serde_json::Value = serde_json::from_str(&json)
        .map_err(|error| invalid(format!("invalid ASCII organism memory: {error}")))?;
    let raw_days = raw
        .get("days")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| invalid("ASCII organism memory has an invalid daily-record array"))?;
    if raw_days.len() != memory.days.len() {
        return Err(invalid(
            "ASCII organism memory has an inconsistent daily-record array",
        ));
    }
    let mut duration_generation = None;
    let mut activity_generation = None;
    for day in raw_days {
        let day = day
            .as_object()
            .ok_or_else(|| invalid("ASCII organism memory has an invalid daily record"))?;
        let sum_present = day.contains_key("build_duration_sum_ms");
        let count_present = day.contains_key("build_duration_count");
        if sum_present != count_present {
            return Err(invalid(
                "ASCII organism memory has an incomplete build-duration aggregate",
            ));
        }
        if duration_generation
            .replace(sum_present)
            .is_some_and(|current| current != sum_present)
        {
            return Err(invalid(
                "ASCII organism memory mixes build-duration aggregate generations",
            ));
        }
        let activity_present = day.contains_key("activity_buckets");
        if activity_generation
            .replace(activity_present)
            .is_some_and(|current| current != activity_present)
        {
            return Err(invalid(
                "ASCII organism memory mixes activity-bucket generations",
            ));
        }
    }
    let flip_generation = memory
        .days
        .iter()
        .map(|stats| {
            let daily_present = stats.failure_success_flips.present;
            let baseline_present = stats
                .baseline
                .as_ref()
                .map(|baseline| baseline.failure_success_flips.present);
            match (daily_present, baseline_present) {
                (false, Some(false)) | (false, None) => Ok(false),
                (true, Some(true)) | (true, None) => Ok(true),
                // A released writer always emits both counters together.
                // Mixed presence inside one record is truncation/tampering.
                _ => Err(invalid(
                    "ASCII organism memory has an incomplete failure-success flip counter",
                )),
            }
        })
        .collect::<io::Result<Vec<_>>>()?;
    let has_legacy_flip_record = flip_generation.iter().any(|current| !current);
    let has_current_flip_record = flip_generation.iter().any(|current| *current);
    if has_legacy_flip_record && has_current_flip_record {
        // Migration is a whole-file generation. A released writer rewrites
        // every DailyStats field, so per-record old/new mixtures can only be
        // a partial edit that would otherwise erase compacted flip history.
        return Err(invalid(
            "ASCII organism memory mixes failure-success counter generations",
        ));
    }
    let growth_generation = match (
        memory.days_seen.present,
        memory.lifetime_recoveries.present,
        memory.growth_days.present,
    ) {
        (false, false, false) => false,
        (true, true, true) => true,
        _ => {
            return Err(invalid(
                "ASCII organism memory has an incomplete growth ledger",
            ));
        }
    };
    if !memory.days.is_empty() {
        let duration_generation =
            duration_generation.expect("a non-empty daily array has a duration generation");
        let activity_generation =
            activity_generation.expect("a non-empty daily array has an activity generation");
        if has_current_flip_record != activity_generation
            || has_current_flip_record != growth_generation
            || (has_current_flip_record && !duration_generation)
        {
            return Err(invalid(
                "ASCII organism memory has incoherent schema generations",
            ));
        }
    }
    if has_legacy_flip_record {
        for stats in &mut memory.days {
            // Compacted history predating this field cannot be recovered; its
            // default baseline is zero. Retained ordered observations can and
            // should be replayed so the next shallow recovery sees them. Do
            // that on a clone and copy only the new scalar: replaying in place
            // before validation would silently repair corrupt legacy summary
            // fields that must continue to fail closed.
            let mut rebuilt = stats.clone();
            if let Some(baseline) = rebuilt.baseline.as_mut() {
                baseline.failure_success_flips.mark_present();
            }
            replay_observations(&mut rebuilt, None);
            if let Some(baseline) = stats.baseline.as_mut() {
                baseline.failure_success_flips.mark_present();
            }
            stats.failure_success_flips =
                MigratingCounter::new(rebuilt.failure_success_flips.value);
        }
    }

    if !growth_generation {
        // Old v1 files cannot reveal records already evicted from their
        // bounded repo/day window. Reconstruct a conservative lower bound
        // from retained semantic evidence and close all older dates so a
        // late retry cannot make the migrated organism grow twice.
        let mut recent: Vec<_> = memory
            .days
            .iter()
            .filter(|stats| daily_stats_has_semantic_activity(stats))
            .map(|stats| stats.day)
            .collect();
        recent.sort_unstable();
        recent.dedup();
        let compacted_through = recent.first().and_then(|day| day.checked_sub(1));
        let recoveries = memory.days.iter().fold(0_u64, |total, stats| {
            total.saturating_add(u64::from(stats.failure_success_flips.value))
        });
        memory.days_seen = MigratingCounter::new(u32::try_from(recent.len()).unwrap_or(u32::MAX));
        memory.lifetime_recoveries =
            MigratingCounter::new(u32::try_from(recoveries).unwrap_or(u32::MAX));
        memory.growth_days = MigratingGrowthDayLedger::new(GrowthDayLedger {
            compacted_through,
            recent,
        });
    }
    memory.validate()?;
    Ok(memory)
}

/// Admission and scheduling are separate facts. Once an event has entered the
/// per-path queue, a scheduler error (or a worker racing ahead and removing
/// the event) can never turn that retained admission into a rejection.
enum EventEnqueue {
    Retained(io::Result<()>),
    Rejected(io::Error),
}

fn enqueue_event(path: PathBuf, event: MemoryEvent) -> EventEnqueue {
    enqueue_event_with_scheduler(path, event, schedule_queued_events)
}

fn enqueue_event_with_scheduler<F>(path: PathBuf, event: MemoryEvent, schedule: F) -> EventEnqueue
where
    F: FnOnce(PathBuf) -> io::Result<()>,
{
    let queues = event_queues();
    {
        let mut queues = queues
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let queue = queues.entry(path.clone()).or_default();
        let already_retained = queue.iter().any(|queued| queued.id == event.id);
        if !already_retained && queue.len() >= MAX_PENDING_MEMORY_EVENTS {
            return EventEnqueue::Rejected(io::Error::new(
                io::ErrorKind::WouldBlock,
                "ASCII organism memory queue is full",
            ));
        }
        if !already_retained {
            queue.push_back(event);
        }
    }

    EventEnqueue::Retained(schedule(path))
}

fn schedule_queued_events(path: PathBuf) -> io::Result<()> {
    let key = crate::persistence::PersistenceKey::for_path("ascii-organism", &path);
    let queued_path = path.clone();
    crate::persistence::enqueue(key, "Save ASCII organism memory", move || {
        let events: Vec<_> = {
            let queues = event_queues()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            queues
                .get(&queued_path)
                .map(|events| events.iter().cloned().collect())
                .unwrap_or_default()
        };
        if events.is_empty() {
            return Ok(());
        }
        let mut events = events;
        events.sort_by(|left, right| {
            left.at_ms
                .cmp(&right.at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        transact_batch(&queued_path, &events)?;
        record_acknowledged_events(&queued_path, &events);
        let completed: HashSet<_> = events.iter().map(|event| event.id.as_str()).collect();
        let mut queues = event_queues()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(queue) = queues.get_mut(&queued_path) {
            queue.retain(|event| !completed.contains(event.id.as_str()));
            if queue.is_empty() {
                queues.remove(&queued_path);
            }
        }
        Ok(())
    })
}

/// Re-publish every retained memory queue before the shared persistence worker
/// enters its bounded shutdown drain. Failed transactions deliberately keep
/// their events here, so a recovered mount/permission state gets one last
/// durable attempt even when no later command arrived to schedule it.
pub(crate) fn flush_pending(timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let paths: Vec<_> = event_queues()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .filter(|(_, events)| !events.is_empty())
        .map(|(path, _)| path.clone())
        .collect();
    let mut first_error = None;
    for path in paths {
        loop {
            match schedule_queued_events(path.clone()) {
                Ok(()) => break,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(LOCK_POLL);
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                    break;
                }
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn event_queues() -> &'static Mutex<HashMap<PathBuf, VecDeque<MemoryEvent>>> {
    static QUEUES: OnceLock<Mutex<HashMap<PathBuf, VecDeque<MemoryEvent>>>> = OnceLock::new();
    QUEUES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn acknowledged_events() -> &'static Mutex<HashMap<PathBuf, VecDeque<String>>> {
    static ACKNOWLEDGED: OnceLock<Mutex<HashMap<PathBuf, VecDeque<String>>>> = OnceLock::new();
    ACKNOWLEDGED.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_acknowledged_events(path: &Path, events: &[MemoryEvent]) {
    let mut acknowledged = acknowledged_events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let ids = acknowledged.entry(path.to_path_buf()).or_default();
    for event in events {
        if !ids.iter().any(|id| id == &event.id) {
            ids.push_back(event.id.clone());
        }
    }
    while ids.len() > MAX_ACKNOWLEDGED_MEMORY_EVENTS {
        ids.pop_front();
    }
}

fn acknowledge_session_events(path: &Path, session_events: &mut VecDeque<MemoryEvent>) {
    if session_events.is_empty() {
        return;
    }
    let local_ids: HashSet<_> = session_events
        .iter()
        .map(|event| event.id.as_str())
        .collect();
    let mut acknowledged = acknowledged_events()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut completed = HashSet::new();
    if let Some(ids) = acknowledged.get_mut(path) {
        ids.retain(|id| {
            if local_ids.contains(id.as_str()) {
                completed.insert(id.clone());
                false
            } else {
                true
            }
        });
        if ids.is_empty() {
            acknowledged.remove(path);
        }
    }
    session_events.retain(|event| !completed.contains(&event.id));
}

fn transact(path: &Path, event: &MemoryEvent) -> io::Result<()> {
    transact_batch(path, std::slice::from_ref(event))
}

fn transact_batch(path: &Path, events: &[MemoryEvent]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "ASCII organism memory path has no parent",
        )
    })?;
    jterm_core::snapshot_file::ensure_private_directory(parent)?;
    let _lock = MemoryTransactionLock::acquire(path, LOCK_TIMEOUT)?;
    let mut memory = read_memory(path)?;
    for event in events {
        apply_event(&mut memory, event);
    }
    memory.validate()?;
    let mut bytes = serde_json::to_vec_pretty(&memory).map_err(io::Error::other)?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_MEMORY_BYTES {
        return Err(invalid("ASCII organism memory exceeds its 512 KiB limit"));
    }
    jterm_core::snapshot_file::write_atomic_private(path, &bytes)
}

struct MemoryTransactionLock {
    directory: File,
    sidecar: File,
}

impl MemoryTransactionLock {
    fn acquire(path: &Path, timeout: Duration) -> io::Result<Self> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "memory path has no parent")
        })?;
        let directory = open_private_directory(parent)?;
        lock_with_timeout(&directory, timeout, "ASCII organism memory directory")?;

        let lock_path = lock_path_for(path)?;
        let sidecar = match open_private_lock_file(&lock_path) {
            Ok(file) => file,
            Err(error) => {
                unlock(&directory);
                return Err(error);
            }
        };
        if let Err(error) = lock_with_timeout(&sidecar, timeout, "ASCII organism memory lock") {
            unlock(&directory);
            return Err(error);
        }
        Ok(Self { directory, sidecar })
    }
}

impl Drop for MemoryTransactionLock {
    fn drop(&mut self) {
        unlock(&self.sidecar);
        unlock(&self.directory);
    }
}

fn lock_path_for(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "memory path has no file name")
    })?;
    let mut lock_name = name.to_os_string();
    lock_name.push(".lock");
    Ok(path.with_file_name(lock_name))
}

fn open_private_directory(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ASCII organism memory parent is not a directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions and only reads process state.
        if metadata.uid() != unsafe { nix::libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "ASCII organism memory parent must be private and user-owned",
            ));
        }
    }
    Ok(file)
}

fn open_private_lock_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ASCII organism memory lock is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions and only reads process state.
        if metadata.uid() != unsafe { nix::libc::geteuid() }
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o022 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "ASCII organism memory lock is not a private user-owned file",
            ));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(unix)]
fn try_lock(file: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: file owns a live descriptor and flock retains no Rust pointer.
    let result =
        unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| code == nix::libc::EAGAIN || code == nix::libc::EWOULDBLOCK)
    {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(not(unix))]
fn try_lock(_file: &File) -> io::Result<bool> {
    Ok(true)
}

fn lock_with_timeout(file: &File, timeout: Duration, label: &str) -> io::Result<()> {
    let started = Instant::now();
    loop {
        match try_lock(file)? {
            true => return Ok(()),
            false if started.elapsed() < timeout => std::thread::sleep(LOCK_POLL),
            false => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("timed out waiting for {label}"),
                ));
            }
        }
    }
}

#[cfg(unix)]
fn unlock(file: &File) {
    use std::os::fd::AsRawFd;
    // SAFETY: file owns a live descriptor for this call.
    let result = unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_UN) };
    if result != 0 {
        log::warn!(
            "failed to release ASCII organism memory lock: {}",
            io::Error::last_os_error()
        );
    }
}

#[cfg(not(unix))]
fn unlock(_file: &File) {}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn next_event_id() -> String {
    static NEXT_EVENT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_EVENT.fetch_add(1, Ordering::Relaxed);
    #[cfg(target_os = "linux")]
    {
        let mut random = [0_u8; 16];
        // SAFETY: random is a live writable buffer and getrandom retains no
        // pointer. The id is opaque on disk: it encodes neither PID nor time.
        let read = unsafe {
            nix::libc::getrandom(
                random.as_mut_ptr().cast(),
                random.len(),
                nix::libc::GRND_NONBLOCK,
            )
        };
        if read == random.len() as isize {
            return random.iter().map(|byte| format!("{byte:02x}")).collect();
        }
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    format!(
        "{:016x}",
        nonce.rotate_left(17) ^ sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15)
    )
}

pub(crate) fn local_circadian_time_at_ms(at_ms: u64) -> LocalCircadianTime {
    let unix_seconds = i64::try_from(at_ms / 1_000).unwrap_or(i64::MAX);
    #[cfg(unix)]
    {
        let seconds: nix::libc::time_t = unix_seconds;
        let mut local: nix::libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers refer to live, correctly aligned values.
        if !unsafe { nix::libc::localtime_r(&seconds, &mut local) }.is_null()
            && (0..12).contains(&local.tm_mon)
            && (1..=31).contains(&local.tm_mday)
            && (0..24).contains(&local.tm_hour)
        {
            return LocalCircadianTime {
                day: days_from_civil(
                    i64::from(local.tm_year) + 1900,
                    u32::try_from(local.tm_mon + 1).unwrap_or(1),
                    u32::try_from(local.tm_mday).unwrap_or(1),
                ),
                bucket: u8::try_from(local.tm_hour / 3).unwrap_or(0),
            };
        }
    }
    utc_circadian_time(unix_seconds)
}

pub(crate) fn local_day_at_ms(at_ms: u64) -> i64 {
    local_circadian_time_at_ms(at_ms).day
}

fn utc_circadian_time(unix_seconds: i64) -> LocalCircadianTime {
    let seconds_in_day = unix_seconds.rem_euclid(86_400);
    LocalCircadianTime {
        day: unix_seconds.div_euclid(86_400),
        bucket: u8::try_from(seconds_in_day / (3 * 60 * 60)).unwrap_or(0),
    }
}

/// Gregorian civil date to days since 1970-01-01 (Howard Hinnant).
fn days_from_civil(mut year: i64, month: u32, day: u32) -> i64 {
    year -= i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let doy = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub(crate) fn git_repo_root_for(cwd: &str) -> Option<String> {
    if cwd.is_empty()
        || cwd.len() > 16 * 1024
        || cwd.chars().any(char::is_control)
        || crate::review_input::contains_visual_spoofing(cwd)
    {
        return None;
    }
    let cwd = Path::new(cwd);
    if !cwd.is_absolute() {
        return None;
    }
    // A canonical root keeps one checkout under one key even when the shell's
    // logical cwd contains symlinks or `..`. Remote panes never call this
    // helper; see the explicit provenance gate in ui/organism.rs.
    let canonical = fs::canonicalize(cwd).ok()?;
    if !canonical.is_dir() {
        return None;
    }
    let mut directory = Some(canonical.as_path());
    while let Some(candidate) = directory {
        let marker = candidate.join(".git");
        match fs::symlink_metadata(&marker) {
            Ok(metadata) => {
                let kind = metadata.file_type();
                if !kind.is_dir() && !kind.is_file() {
                    // A symlinked marker is neither a trustworthy local repo
                    // identity nor a reason to keep walking into a parent
                    // checkout and misattribute the command there.
                    return None;
                }
                let head = if kind.is_dir() {
                    marker.join("HEAD")
                } else {
                    let pointer = read_small_git_file(&marker)?;
                    let target = pointer.trim().strip_prefix("gitdir:")?.trim();
                    let target = Path::new(target);
                    let git_dir = if target.is_absolute() {
                        target.to_path_buf()
                    } else {
                        candidate.join(target)
                    };
                    git_dir.join("HEAD")
                };
                if !valid_git_head(&head) {
                    return None;
                }
                let repo = candidate.to_str()?;
                return valid_repo_id(repo).then(|| repo.to_owned());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        directory = candidate.parent();
    }
    None
}

fn valid_git_head(path: &Path) -> bool {
    let Some(head) = read_small_git_file(path) else {
        return false;
    };
    let head = head.trim();
    head.strip_prefix("ref:")
        .is_some_and(|reference| reference.trim().starts_with("refs/"))
        || (matches!(head.len(), 40 | 64) && head.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn read_small_git_file(path: &Path) -> Option<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    }
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_GIT_POINTER_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_GIT_POINTER_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_GIT_POINTER_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "anvil-organism-memory-{label}-{}-{id}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self(path)
        }

        fn memory_path(&self) -> PathBuf {
            self.0.join("state/ascii-organism.json")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn event(
        at_ms: u64,
        day: i64,
        repo: &Path,
        kind: CommandKind,
        exit_code: Option<i32>,
    ) -> MemoryEvent {
        MemoryEvent::fixed(
            at_ms,
            day,
            repo.to_str(),
            kind,
            exit_code,
            LifeState::default(),
        )
    }

    /// Shape emitted by the last released v1 writer: duration aggregates were
    /// present, while flaky/circadian/growth landed together in this series.
    fn strip_current_organism_families(value: &mut serde_json::Value) {
        for day in value["days"].as_array_mut().unwrap() {
            let day = day.as_object_mut().unwrap();
            day.remove("failure_success_flips");
            day.remove("activity_buckets");
            if let Some(baseline) = day
                .get_mut("baseline")
                .and_then(serde_json::Value::as_object_mut)
            {
                baseline.remove("failure_success_flips");
            }
        }
        for field in ["days_seen", "lifetime_recoveries", "growth_days"] {
            value.as_object_mut().unwrap().remove(field);
        }
    }

    fn strip_duration_aggregates(value: &mut serde_json::Value) {
        for day in value["days"].as_array_mut().unwrap() {
            let day = day.as_object_mut().unwrap();
            day.remove("build_duration_sum_ms");
            day.remove("build_duration_count");
        }
    }

    #[test]
    fn civil_day_index_has_exact_gregorian_boundaries() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(
            days_from_civil(2000, 2, 28) + 1,
            days_from_civil(2000, 2, 29)
        );
        assert_eq!(
            days_from_civil(2000, 2, 29) + 1,
            days_from_civil(2000, 3, 1)
        );
        assert_eq!(
            days_from_civil(2100, 2, 28) + 1,
            days_from_civil(2100, 3, 1)
        );
    }

    #[test]
    fn utc_circadian_fallback_has_euclidean_day_boundaries() {
        assert_eq!(
            utc_circadian_time(-1),
            LocalCircadianTime { day: -1, bucket: 7 }
        );
        assert_eq!(
            utc_circadian_time(0),
            LocalCircadianTime { day: 0, bucket: 0 }
        );
        assert_eq!(
            utc_circadian_time(86_399),
            LocalCircadianTime { day: 0, bucket: 7 }
        );
        assert_eq!(
            utc_circadian_time(86_400),
            LocalCircadianTime { day: 1, bucket: 0 }
        );
    }

    #[cfg(unix)]
    #[test]
    fn circadian_dst_timezone_helper() {
        if std::env::var_os("FORGE_ORGANISM_DST_TEST").is_none() {
            return;
        }
        let utc_ms = |year, month, day, hour, minute| {
            let seconds = days_from_civil(year, month, day)
                .saturating_mul(86_400)
                .saturating_add(i64::from(hour) * 3_600)
                .saturating_add(i64::from(minute) * 60);
            u64::try_from(seconds).unwrap().saturating_mul(1_000)
        };

        let spring_day = days_from_civil(2024, 3, 10);
        assert_eq!(
            local_circadian_time_at_ms(utc_ms(2024, 3, 10, 6, 59)),
            LocalCircadianTime {
                day: spring_day,
                bucket: 0,
            }
        );
        // 02:00 does not exist on this local day: the next minute is 03:00.
        assert_eq!(
            local_circadian_time_at_ms(utc_ms(2024, 3, 10, 7, 0)),
            LocalCircadianTime {
                day: spring_day,
                bucket: 1,
            }
        );

        let fall_day = days_from_civil(2024, 11, 3);
        // The two instances of 01:30 share one wall-clock bucket.
        for hour in [5, 6] {
            assert_eq!(
                local_circadian_time_at_ms(utc_ms(2024, 11, 3, hour, 30)),
                LocalCircadianTime {
                    day: fall_day,
                    bucket: 0,
                }
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_circadian_time_follows_wall_clock_dst_without_mutating_test_process_tz() {
        if std::env::var_os("FORGE_ORGANISM_DST_TEST").is_some() {
            return;
        }
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("organism_memory::tests::circadian_dst_timezone_helper")
            .arg("--nocapture")
            // A self-contained POSIX rule avoids depending on the host's tzdata.
            .env("TZ", "EST5EDT,M3.2.0/2,M11.1.0/2")
            .env("FORGE_ORGANISM_DST_TEST", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn git_identity_is_the_full_local_root_and_rejects_symlink_markers() {
        let root = TestDir::new("repo-root");
        let repo = root.0.join("same-name");
        let nested = repo.join("a/b");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(git_repo_root_for(nested.to_str().unwrap()), None);
        fs::create_dir(repo.join(".git")).unwrap();
        fs::write(repo.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        assert_eq!(
            git_repo_root_for(nested.to_str().unwrap()),
            repo.to_str().map(str::to_owned)
        );
        assert_eq!(git_repo_root_for("relative/repo"), None);

        fs::remove_dir_all(repo.join(".git")).unwrap();
        assert_eq!(git_repo_root_for(nested.to_str().unwrap()), None);
        fs::create_dir(repo.join(".git")).unwrap();
        fs::write(repo.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let linked = root.0.join("linked");
            fs::create_dir(&linked).unwrap();
            symlink(repo.join(".git"), linked.join(".git")).unwrap();
            assert_eq!(git_repo_root_for(linked.to_str().unwrap()), None);
        }
    }

    #[test]
    fn context_lookup_uses_the_callers_frozen_day() {
        let root = TestDir::new("context-day");
        let path = root.memory_path();
        let repo = root.0.join("repo");
        let nested = repo.join("nested");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        fs::create_dir(&nested).unwrap();
        let repo_text = repo.to_str().unwrap();

        let mut memory = OrganismMemory::load(path).unwrap();
        memory.apply_local(&MemoryEvent::fixed(
            1_000,
            70,
            Some(repo_text),
            CommandKind::BuildOrTest,
            Some(1),
            LifeState::default(),
        ));
        memory.apply_local(&MemoryEvent::fixed(
            2_000,
            71,
            Some(repo_text),
            CommandKind::BuildOrTest,
            Some(0),
            LifeState::default(),
        ));

        let first = memory
            .context_for_day(nested.to_str(), 70)
            .expect("day 70 context");
        assert_eq!(first.day, 70);
        assert_eq!(first.work, RepoWorkState::new(1, false, 0));
        assert_eq!(first.failures_today, 1);

        let finish_refresh = memory.context_for_repo_day(repo_text, 70);
        assert_eq!(finish_refresh, first);

        let second = memory
            .context_for_day(nested.to_str(), 71)
            .expect("day 71 context");
        assert_eq!(second.day, 71);
        assert_eq!(second.work, RepoWorkState::default());
        assert_eq!(second.successes_today, 1);
    }

    #[test]
    fn memory_event_day_and_bucket_share_one_frozen_millisecond() {
        for at_ms in [0, 1, 86_399_999, 86_400_000, u64::MAX / 2] {
            let local = local_circadian_time_at_ms(at_ms);
            let event = MemoryEvent::at_ms_for_repo(
                at_ms,
                CommandKind::BuildOrTest,
                Some(0),
                None,
                LifeState::default(),
                None,
            );
            assert_eq!(event.at_ms, at_ms);
            assert_eq!(event.day, local.day);
            assert_eq!(event.activity_bucket, local.bucket);
        }
    }

    #[test]
    fn restart_round_trip_is_repo_and_day_scoped_and_private() {
        let root = TestDir::new("restart");
        let path = root.memory_path();
        let repo_a = root.0.join("repo-a");
        let repo_b = root.0.join("repo-b");
        let day = 20_000;

        transact(
            &path,
            &event(10_000, day, &repo_a, CommandKind::BuildOrTest, Some(1)),
        )
        .unwrap();
        transact(
            &path,
            &event(16_000, day, &repo_a, CommandKind::BuildOrTest, Some(0)),
        )
        .unwrap();
        transact(
            &path,
            &event(17_000, day, &repo_a, CommandKind::GitPush, Some(0)),
        )
        .unwrap();
        transact(
            &path,
            &event(18_000, day, &repo_b, CommandKind::BuildOrTest, Some(2)),
        )
        .unwrap();

        let loaded = OrganismMemory::load(path.clone()).unwrap();
        let a = loaded
            .memory()
            .stats(day, repo_a.to_str().unwrap())
            .unwrap();
        assert_eq!(a.build_failures, 1);
        assert_eq!(a.build_successes, 1);
        assert_eq!(a.git_pushes, 1);
        assert_eq!(a.last_recovery_duration_ms, Some(6_000));
        assert_eq!(a.open_failures, 0);
        let b = loaded
            .memory()
            .stats(day, repo_b.to_str().unwrap())
            .unwrap();
        assert_eq!(b.build_failures, 1);
        assert_eq!(b.open_failures, 1);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        assert!(fs::read_dir(path.parent().unwrap()).unwrap().all(|entry| {
            let name = entry.unwrap().file_name();
            !name.to_string_lossy().contains(".tmp.")
        }));
    }

    #[test]
    fn repeated_shallow_recoveries_are_recognized_as_likely_flaky() {
        let repo = "/work/flaky";
        let day = 59_000;
        let mut memory = DiskMemory::default();
        let build = |at_ms: u64, exit_code: i32| {
            MemoryEvent::fixed(
                at_ms,
                day,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(exit_code),
                LifeState::default(),
            )
        };

        for cycle in 0..3u64 {
            apply_event(&mut memory, &build(cycle * 2 + 1, 1));
            let success = build(cycle * 2 + 2, 0);
            let insight = apply_event(&mut memory, &success);
            assert_eq!(insight.likely_flaky, cycle == 2);
            assert_eq!(
                insight.current_work,
                RepoWorkState::new(0, true, u32::try_from(cycle + 1).unwrap())
            );

            // An ambiguous retry cannot advance either the aggregate or the
            // visible threshold a second time.
            let retry = apply_event(&mut memory, &success);
            assert!(!retry.likely_flaky);
            assert_eq!(retry.current_work, insight.current_work);
        }

        let stats = memory.stats(day, repo).unwrap();
        assert_eq!(stats.failure_success_flips.value, 3);
        assert_eq!(stats.open_failures, 0);

        // A deep recovery is real debugging even after the repo has shown a
        // flaky rhythm; only one-open-failure recoveries get the quiet hint.
        apply_event(&mut memory, &build(10, 1));
        apply_event(&mut memory, &build(11, 1));
        let deep = apply_event(&mut memory, &build(12, 0));
        assert_eq!(deep.recovered_failures, 2);
        assert!(!deep.likely_flaky);
        assert_eq!(
            memory.stats(day, repo).unwrap().failure_success_flips.value,
            4
        );
        memory.validate().unwrap();
    }

    #[test]
    fn flaky_flip_count_follows_event_time_and_survives_compaction() {
        let repo = "/work/ordered-flaky";
        let day = 59_001;
        let mut memory = DiskMemory::default();
        let mut first_events = Vec::new();
        for cycle in 0..3u64 {
            for (offset, exit_code) in [(0, 1), (1, 0)] {
                first_events.push(MemoryEvent::fixed(
                    100 + cycle * 2 + offset,
                    day,
                    Some(repo),
                    CommandKind::BuildOrTest,
                    Some(exit_code),
                    LifeState::default(),
                ));
            }
        }
        // Every event remains inside the ordering window, so writer lock
        // order cannot change the derived flip count.
        for event in first_events.iter().rev() {
            apply_event(&mut memory, event);
        }
        assert_eq!(
            memory.stats(day, repo).unwrap().failure_success_flips.value,
            3
        );

        // Continue chronologically past the per-record observation bound.
        for cycle in 3..40u64 {
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    100 + cycle * 2,
                    day,
                    Some(repo),
                    CommandKind::BuildOrTest,
                    Some(1),
                    LifeState::default(),
                ),
            );
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    101 + cycle * 2,
                    day,
                    Some(repo),
                    CommandKind::BuildOrTest,
                    Some(0),
                    LifeState::default(),
                ),
            );
        }
        let stats = memory.stats(day, repo).unwrap();
        assert_eq!(stats.failure_success_flips.value, 40);
        assert!(stats.baseline.as_ref().unwrap().failure_success_flips.value > 0);
        assert!(stats.observations.len() <= MAX_OBSERVATIONS_PER_RECORD);
        memory.validate().unwrap();
    }

    #[test]
    fn version_one_memory_without_flip_counters_migrates_from_retained_events() {
        let root = TestDir::new("flaky-counter-migration");
        let path = root.memory_path();
        let repo = root.0.join("repo");
        let repo = repo.to_str().unwrap();
        let day = 59_002;
        let mut memory = DiskMemory::default();
        for cycle in 0..2u64 {
            for (offset, exit_code) in [(0, 1), (1, 0)] {
                apply_event(
                    &mut memory,
                    &MemoryEvent::fixed(
                        200 + cycle * 2 + offset,
                        day,
                        Some(repo),
                        CommandKind::BuildOrTest,
                        Some(exit_code),
                        LifeState::default(),
                    ),
                );
            }
        }
        let mut legacy = serde_json::to_value(&memory).unwrap();
        strip_current_organism_families(&mut legacy);
        let mut bytes = serde_json::to_vec_pretty(&legacy).unwrap();
        bytes.push(b'\n');
        jterm_core::snapshot_file::ensure_private_directory(path.parent().unwrap()).unwrap();
        jterm_core::snapshot_file::write_atomic_private(&path, &bytes).unwrap();

        let migrated = read_memory(&path).unwrap();
        assert_eq!(
            migrated
                .stats(day, repo)
                .unwrap()
                .failure_success_flips
                .value,
            2
        );
        migrated.validate().unwrap();
    }

    #[test]
    fn flip_counter_migration_does_not_repair_corrupt_legacy_summaries() {
        let root = TestDir::new("flaky-counter-fail-closed");
        let path = root.memory_path();
        let repo = root.0.join("repo");
        let repo = repo.to_str().unwrap();
        let mut memory = DiskMemory::default();
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                1,
                59_003,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(1),
                LifeState::default(),
            ),
        );
        let mut legacy = serde_json::to_value(&memory).unwrap();
        strip_current_organism_families(&mut legacy);
        let day_value = legacy["days"][0].as_object_mut().unwrap();
        day_value.insert("build_failures".to_string(), serde_json::json!(2));
        let mut bytes = serde_json::to_vec_pretty(&legacy).unwrap();
        bytes.push(b'\n');
        jterm_core::snapshot_file::ensure_private_directory(path.parent().unwrap()).unwrap();
        jterm_core::snapshot_file::write_atomic_private(&path, &bytes).unwrap();

        let error = read_memory(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("counters do not match"));
    }

    #[test]
    fn partially_missing_flip_counters_are_not_mistaken_for_legacy_files() {
        let root = TestDir::new("partial-flip-counter");
        let repo = root.0.join("repo");
        let repo = repo.to_str().unwrap();
        let mut memory = DiskMemory::default();
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                1,
                59_004,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(1),
                LifeState::default(),
            ),
        );

        for remove_baseline in [false, true] {
            let mut partial = serde_json::to_value(&memory).unwrap();
            let day = partial["days"][0].as_object_mut().unwrap();
            if remove_baseline {
                day["baseline"]
                    .as_object_mut()
                    .unwrap()
                    .remove("failure_success_flips");
            } else {
                day.remove("failure_success_flips");
            }
            let path = root.0.join(format!("partial-{remove_baseline}.json"));
            let mut bytes = serde_json::to_vec_pretty(&partial).unwrap();
            bytes.push(b'\n');
            jterm_core::snapshot_file::write_atomic_private(&path, &bytes).unwrap();
            let error = read_memory(&path).unwrap_err();
            assert!(error.to_string().contains("incomplete"));
        }
    }

    #[test]
    fn flip_counter_migration_is_whole_file_not_per_record() {
        let root = TestDir::new("mixed-flip-generations");
        let path = root.memory_path();
        let mut memory = DiskMemory::default();
        for day in [59_005, 59_006] {
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    day as u64,
                    day,
                    Some(&format!("/work/mixed-generation-{day}")),
                    CommandKind::BuildOrTest,
                    Some(1),
                    LifeState::default(),
                ),
            );
        }

        let mut mixed = serde_json::to_value(&memory).unwrap();
        let legacy_record = mixed["days"][0].as_object_mut().unwrap();
        legacy_record.remove("failure_success_flips");
        legacy_record["baseline"]
            .as_object_mut()
            .unwrap()
            .remove("failure_success_flips");
        let mut bytes = serde_json::to_vec_pretty(&mixed).unwrap();
        bytes.push(b'\n');
        jterm_core::snapshot_file::ensure_private_directory(path.parent().unwrap()).unwrap();
        jterm_core::snapshot_file::write_atomic_private(&path, &bytes).unwrap();

        let error = read_memory(&path).unwrap_err();
        assert!(error.to_string().contains("mixes failure-success"));
    }

    #[test]
    fn impossible_baseline_flip_count_is_rejected() {
        let mut memory = DiskMemory::default();
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                1,
                59_005,
                Some("/work/impossible-flips"),
                CommandKind::BuildOrTest,
                Some(1),
                LifeState::default(),
            ),
        );
        let baseline = memory.days[0].baseline.as_mut().unwrap();
        baseline.failure_success_flips = MigratingCounter::new(1);
        assert!(memory
            .validate()
            .unwrap_err()
            .to_string()
            .contains("baseline flip"));
    }

    #[test]
    fn migration_presence_scan_does_not_weaken_duplicate_field_rejection() {
        let root = TestDir::new("duplicate-field");
        let path = root.memory_path();
        let encoded = serde_json::to_string_pretty(&DiskMemory::default()).unwrap();
        let duplicate = encoded.replacen('{', "{\n  \"version\": 1,", 1);
        jterm_core::snapshot_file::ensure_private_directory(path.parent().unwrap()).unwrap();
        jterm_core::snapshot_file::write_atomic_private(&path, duplicate.as_bytes()).unwrap();

        let error = read_memory(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("duplicate field"));
    }

    #[test]
    fn nested_schema_fields_reject_duplicates_before_the_presence_scan() {
        let root = TestDir::new("nested-duplicate-fields");
        jterm_core::snapshot_file::ensure_private_directory(&root.0).unwrap();

        let mut memory = DiskMemory::default();
        let mut stats = DailyStats::new(60_000, "/work/nested-duplicates".to_string());
        stats.baseline = Some(StatsBaseline::default());
        memory.days.push(stats);
        let encoded = serde_json::to_string(&memory).unwrap();

        let cases = [
            (
                "daily-duration",
                "\"build_duration_sum_ms\":0",
                "\"build_duration_sum_ms\":0,\"build_duration_sum_ms\":0",
                "build_duration_sum_ms",
            ),
            (
                "daily-activity",
                "\"activity_buckets\":[0,0,0,0,0,0,0,0]",
                "\"activity_buckets\":[0,0,0,0,0,0,0,0],\"activity_buckets\":[0,0,0,0,0,0,0,0]",
                "activity_buckets",
            ),
            (
                "baseline",
                "\"baseline\":{\"build_failures\":0",
                "\"baseline\":{\"build_failures\":0,\"build_failures\":0",
                "build_failures",
            ),
            (
                "growth-ledger",
                "\"growth_days\":{\"compacted_through\":null",
                "\"growth_days\":{\"compacted_through\":null,\"compacted_through\":null",
                "compacted_through",
            ),
        ];

        for (label, needle, replacement, field) in cases {
            assert_eq!(
                encoded.matches(needle).count(),
                1,
                "fixture must identify exactly one {label} field"
            );
            let duplicate = encoded.replacen(needle, replacement, 1);
            let path = root.0.join(format!("{label}.json"));
            jterm_core::snapshot_file::write_atomic_private(&path, duplicate.as_bytes()).unwrap();

            let error = read_memory(&path).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            let message = error.to_string();
            assert!(message.contains("duplicate field"), "{label}: {message}");
            assert!(message.contains(field), "{label}: {message}");
        }
    }

    #[test]
    fn build_durations_aggregate_once_per_event_and_yield_a_baseline() {
        let repo = "/work/anvil";
        let day = 60_000;
        let mut memory = DiskMemory::default();
        let success = |at_ms: u64, duration: Option<u64>| {
            MemoryEvent::fixed(
                at_ms,
                day,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(0),
                LifeState::default(),
            )
            .with_duration(duration)
        };

        let first = success(1_000, Some(30_000));
        let one = apply_event(&mut memory, &first);
        assert_eq!(one.typical_build_ms, None, "needs three samples");
        // An ambiguous retry of the same event id is a byte-for-byte no-op.
        apply_event(&mut memory, &first);
        apply_event(&mut memory, &success(2_000, Some(60_000)));
        let third = apply_event(&mut memory, &success(3_000, Some(90_000)));
        assert_eq!(
            third.typical_build_ms, None,
            "the current run is not history"
        );
        let fourth = apply_event(&mut memory, &success(4_000, Some(120_000)));
        assert_eq!(fourth.typical_build_ms, Some(60_000));

        let stats = memory.stats(day, repo).unwrap();
        assert_eq!(stats.build_duration_sum_ms, 300_000);
        assert_eq!(stats.build_duration_count, 4);
        memory.validate().unwrap();

        // Failures and duration-less successes never touch the aggregate,
        // and a pathological duration is capped before it lands.
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                5_000,
                day,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(1),
                LifeState::default(),
            )
            .with_duration(Some(500_000)),
        );
        apply_event(&mut memory, &success(6_000, None));
        apply_event(&mut memory, &success(7_000, Some(u64::MAX)));
        let stats = memory.stats(day, repo).unwrap();
        assert_eq!(stats.build_duration_count, 5);
        assert_eq!(stats.build_duration_sum_ms, 300_000 + MAX_TRACKED_BUILD_MS);
        memory.validate().unwrap();

        // The aggregate survives observation compaction untouched.
        let sum_before = memory.stats(day, repo).unwrap().build_duration_sum_ms;
        compact_oldest_observations(memory.stats_mut(day, repo));
        assert_eq!(
            memory.stats(day, repo).unwrap().build_duration_sum_ms,
            sum_before
        );

        // An inconsistent aggregate is rejected on load.
        let mut poisoned = memory.clone();
        poisoned.days[0].build_duration_sum_ms =
            u64::from(poisoned.days[0].build_duration_count) * MAX_TRACKED_BUILD_MS + 1;
        assert!(poisoned.validate().is_err());

        let mut impossible_count = memory;
        impossible_count.days[0].build_duration_count =
            impossible_count.days[0].build_successes.saturating_add(1);
        impossible_count.days[0].build_duration_sum_ms = 0;
        assert!(impossible_count.validate().is_err());
    }

    #[test]
    fn build_duration_aggregate_fields_migrate_atomically() {
        let root = TestDir::new("duration-presence");
        let mut memory = DiskMemory::default();
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                1,
                60_001,
                Some("/work/zero-duration"),
                CommandKind::BuildOrTest,
                Some(0),
                LifeState::default(),
            )
            .with_duration(Some(0)),
        );
        assert_eq!(memory.days[0].build_duration_sum_ms, 0);
        assert_eq!(memory.days[0].build_duration_count, 1);

        for missing in ["build_duration_sum_ms", "build_duration_count"] {
            let mut partial = serde_json::to_value(&memory).unwrap();
            partial["days"][0].as_object_mut().unwrap().remove(missing);
            let path = root.0.join(format!("partial-{missing}.json"));
            let mut bytes = serde_json::to_vec_pretty(&partial).unwrap();
            bytes.push(b'\n');
            jterm_core::snapshot_file::write_atomic_private(&path, &bytes).unwrap();
            assert!(read_memory(&path)
                .unwrap_err()
                .to_string()
                .contains("incomplete build-duration"));
        }

        let mut legacy = serde_json::to_value(&memory).unwrap();
        strip_current_organism_families(&mut legacy);
        strip_duration_aggregates(&mut legacy);
        let path = root.0.join("legacy.json");
        let mut bytes = serde_json::to_vec_pretty(&legacy).unwrap();
        bytes.push(b'\n');
        jterm_core::snapshot_file::write_atomic_private(&path, &bytes).unwrap();
        let migrated = read_memory(&path).unwrap();
        assert_eq!(migrated.days[0].build_duration_sum_ms, 0);
        assert_eq!(migrated.days[0].build_duration_count, 0);
    }

    #[test]
    fn schema_family_generations_follow_the_released_v1_lineage() {
        let root = TestDir::new("schema-lineage");
        let mut memory = DiskMemory::default();
        for day in [60_010, 60_011] {
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    day as u64,
                    day,
                    Some(&format!("/work/schema-lineage-{day}")),
                    CommandKind::GitPush,
                    Some(0),
                    LifeState::default(),
                ),
            );
        }
        let current = serde_json::to_value(&memory).unwrap();

        for missing_family in ["flip", "activity", "growth", "duration"] {
            let mut tampered = current.clone();
            match missing_family {
                "flip" => {
                    for day in tampered["days"].as_array_mut().unwrap() {
                        let day = day.as_object_mut().unwrap();
                        day.remove("failure_success_flips");
                        day["baseline"]
                            .as_object_mut()
                            .unwrap()
                            .remove("failure_success_flips");
                    }
                }
                "activity" => {
                    for day in tampered["days"].as_array_mut().unwrap() {
                        day.as_object_mut().unwrap().remove("activity_buckets");
                    }
                }
                "growth" => {
                    for field in ["days_seen", "lifetime_recoveries", "growth_days"] {
                        tampered.as_object_mut().unwrap().remove(field);
                    }
                }
                "duration" => strip_duration_aggregates(&mut tampered),
                _ => unreachable!(),
            }
            let path = root.0.join(format!("missing-{missing_family}.json"));
            let mut bytes = serde_json::to_vec_pretty(&tampered).unwrap();
            bytes.push(b'\n');
            jterm_core::snapshot_file::write_atomic_private(&path, &bytes).unwrap();
            assert!(read_memory(&path)
                .unwrap_err()
                .to_string()
                .contains("incoherent schema generations"));
        }

        // The last released v1 writer already emitted duration aggregates,
        // but none of this series' flaky/circadian/growth cohort.
        let mut released_head = current.clone();
        strip_current_organism_families(&mut released_head);
        let head_path = root.0.join("released-head.json");
        let mut bytes = serde_json::to_vec_pretty(&released_head).unwrap();
        bytes.push(b'\n');
        jterm_core::snapshot_file::write_atomic_private(&head_path, &bytes).unwrap();
        read_memory(&head_path).unwrap().validate().unwrap();

        // Older seed/development files may predate the duration pair too.
        strip_duration_aggregates(&mut released_head);
        let seed_path = root.0.join("pre-duration-seed.json");
        let mut bytes = serde_json::to_vec_pretty(&released_head).unwrap();
        bytes.push(b'\n');
        jterm_core::snapshot_file::write_atomic_private(&seed_path, &bytes).unwrap();
        read_memory(&seed_path).unwrap().validate().unwrap();
    }

    #[test]
    fn an_empty_observation_suffix_cannot_disagree_with_its_baseline() {
        let day = 77;
        let repo = "/tmp/empty-suffix";
        let mut memory = DiskMemory::default();
        assert_eq!(memory.observe_growth_day(day), GrowthDayObservation::New);
        let mut stats = DailyStats::new(day, repo.to_string());
        stats.build_failures = 1;
        stats.open_failures = 1;
        stats.open_failure_at_ms = Some(9_000);
        stats.baseline = Some(stats.baseline());
        memory.days.push(stats);
        memory.validate().unwrap();

        // A compacted record may legitimately retain no suffix, but its daily
        // summary is then exactly the baseline—not an unchecked second truth.
        memory.days[0].build_failures = 2;
        assert!(memory.validate().is_err());
    }

    #[test]
    fn a_replayable_suffix_cannot_hide_an_impossible_baseline_failure_state() {
        let build = |day: i64, baseline: StatsBaseline, observations: Vec<Observation>| {
            let mut stats = DailyStats::new(day, format!("/tmp/baseline-{day}"));
            stats.baseline = Some(baseline);
            stats.observations = observations;
            replay_observations(&mut stats, None);
            assert!(failure_state_valid(
                stats.build_failures,
                stats.open_failures,
                stats.open_failure_at_ms,
                stats.recovered_pending_push,
                stats.last_recovery_duration_ms,
            ));
            let mut memory = DiskMemory::default();
            assert_eq!(memory.observe_growth_day(day), GrowthDayObservation::New);
            memory.days.push(stats);
            memory
        };

        let bad_depth = build(
            81,
            StatsBaseline {
                build_failures: 0,
                open_failures: 1,
                open_failure_at_ms: Some(100),
                failure_success_flips: MigratingCounter::new(0),
                ..StatsBaseline::default()
            },
            vec![
                Observation {
                    id: "baseline-depth-failure".to_string(),
                    at_ms: 150,
                    kind: ObservationKind::BuildFailure,
                },
                Observation {
                    id: "baseline-depth-success".to_string(),
                    at_ms: 200,
                    kind: ObservationKind::BuildSuccess,
                },
            ],
        );
        assert!(bad_depth.validate().is_err());

        let bad_cursor = build(
            82,
            StatsBaseline {
                failure_success_flips: MigratingCounter::new(0),
                compacted_through: Some(ObservationCursor {
                    at_ms: 100,
                    id: "baseline-empty-cursor".to_string(),
                }),
                ..StatsBaseline::default()
            },
            Vec::new(),
        );
        assert!(bad_cursor.validate().is_err());

        let bad_recovery = build(
            83,
            StatsBaseline {
                recovered_pending_push: true,
                failure_success_flips: MigratingCounter::new(0),
                ..StatsBaseline::default()
            },
            vec![Observation {
                id: "baseline-recovery-push".to_string(),
                at_ms: 150,
                kind: ObservationKind::GitPush,
            }],
        );
        assert!(bad_recovery.validate().is_err());

        let overlapping_streak_and_recovery = build(
            84,
            StatsBaseline {
                build_failures: 1,
                open_failures: 1,
                open_failure_at_ms: Some(100),
                last_recovery_duration_ms: Some(50),
                recovered_pending_push: true,
                failure_success_flips: MigratingCounter::new(0),
                ..StatsBaseline::default()
            },
            vec![Observation {
                id: "baseline-overlap-push".to_string(),
                at_ms: 150,
                kind: ObservationKind::GitPush,
            }],
        );
        assert!(overlapping_streak_and_recovery.validate().is_err());
    }

    #[test]
    fn an_open_failure_streak_cannot_also_be_pending_push() {
        let day = 85;
        let mut memory = DiskMemory::default();
        assert_eq!(memory.observe_growth_day(day), GrowthDayObservation::New);
        let mut stats = DailyStats::new(day, "/tmp/overlapping-states".to_string());
        stats.build_failures = 1;
        stats.open_failures = 1;
        stats.open_failure_at_ms = Some(100);
        stats.last_recovery_duration_ms = Some(50);
        stats.recovered_pending_push = true;
        memory.days.push(stats);

        assert!(memory.validate().is_err());
    }

    #[test]
    fn compaction_cursor_event_ids_are_globally_unique() {
        let compacted_stats = |day: i64, repo: &str, id: &str| {
            let mut stats = DailyStats::new(day, repo.to_string());
            stats.build_successes = 1;
            stats.baseline = Some(StatsBaseline {
                build_successes: 1,
                failure_success_flips: MigratingCounter::new(0),
                compacted_through: Some(ObservationCursor {
                    at_ms: 100,
                    id: id.to_string(),
                }),
                ..StatsBaseline::default()
            });
            stats
        };

        let mut duplicate_cursors = DiskMemory::default();
        for (day, repo) in [(90, "/tmp/cursor-a"), (91, "/tmp/cursor-b")] {
            assert_eq!(
                duplicate_cursors.observe_growth_day(day),
                GrowthDayObservation::New
            );
            duplicate_cursors
                .days
                .push(compacted_stats(day, repo, "shared-cursor-id"));
        }
        assert!(duplicate_cursors.validate().is_err());

        let day = 92;
        let mut cursor_and_suffix = DiskMemory::default();
        assert_eq!(
            cursor_and_suffix.observe_growth_day(day),
            GrowthDayObservation::New
        );
        let mut stats = compacted_stats(day, "/tmp/cursor-suffix", "reused-event-id");
        stats.observations.push(Observation {
            id: "reused-event-id".to_string(),
            at_ms: 200,
            kind: ObservationKind::BuildSuccess,
        });
        replay_observations(&mut stats, None);
        cursor_and_suffix.days.push(stats);
        assert!(cursor_and_suffix.validate().is_err());
    }

    #[test]
    fn duration_baseline_is_snapshotted_before_day_record_eviction() {
        let repo = "/work/evicted-baseline";
        let mut memory = DiskMemory::default();
        let oldest = memory.stats_mut(0, repo);
        oldest.build_duration_sum_ms = 90_000;
        oldest.build_duration_count = 3;
        for day in 1..MAX_DAILY_RECORDS as i64 {
            memory.stats_mut(day, &format!("/work/filler-{day}"));
        }
        assert_eq!(memory.days.len(), MAX_DAILY_RECORDS);

        let insight = apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                100_000,
                MAX_DAILY_RECORDS as i64,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(0),
                LifeState::default(),
            )
            .with_duration(Some(120_000))
            .with_activity_bucket(3),
        );
        assert_eq!(insight.typical_build_ms, Some(30_000));
        assert!(memory.stats(0, repo).is_none(), "oldest record was pruned");
        memory.validate().unwrap();
    }

    #[test]
    fn growth_stage_boundaries_are_explicit_and_saturating() {
        assert_eq!(GrowthProgress::default().stage(), GrowthStage::Juvenile);
        assert_eq!(GrowthStage::from_counts(6, u32::MAX), GrowthStage::Juvenile);
        assert_eq!(GrowthStage::from_counts(7, 0), GrowthStage::Adult);
        assert_eq!(GrowthStage::from_counts(59, u32::MAX), GrowthStage::Adult);
        assert_eq!(GrowthStage::from_counts(60, 11), GrowthStage::Adult);
        assert_eq!(GrowthStage::from_counts(60, 12), GrowthStage::Seasoned);
        assert_eq!(
            GrowthStage::from_counts(u32::MAX, u32::MAX),
            GrowthStage::Seasoned
        );
    }

    #[test]
    fn growth_days_are_global_across_repos_and_idempotent_under_reordering() {
        let mut memory = DiskMemory::default();
        let first = MemoryEvent::fixed(
            1_000,
            100,
            Some("/work/a"),
            CommandKind::BuildOrTest,
            None,
            LifeState::default(),
        );
        apply_event(&mut memory, &first);
        apply_event(&mut memory, &first);
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                2_000,
                100,
                Some("/work/b"),
                CommandKind::GitPush,
                Some(1),
                LifeState::default(),
            ),
        );
        for day in [102, 101] {
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    3_000 + day as u64,
                    day,
                    Some("/work/a"),
                    CommandKind::BuildOrTest,
                    None,
                    LifeState::default(),
                ),
            );
        }
        // Neither an arbitrary command nor an unscoped semantic command is a
        // remembered workday.
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                9_000,
                103,
                Some("/work/a"),
                CommandKind::Other,
                Some(0),
                LifeState::default(),
            ),
        );
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                10_000,
                103,
                None,
                CommandKind::BuildOrTest,
                Some(0),
                LifeState::default(),
            ),
        );

        assert_eq!(memory.days_seen.value, 3);
        assert_eq!(memory.growth_days.value.recent, [100, 101, 102]);
        assert_eq!(memory.lifetime_recoveries.value, 0);
        memory.validate().unwrap();

        for day in 103..=106 {
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    20_000 + day as u64,
                    day,
                    Some("/work/a"),
                    CommandKind::BuildOrTest,
                    None,
                    LifeState::default(),
                ),
            );
        }
        assert_eq!(memory.days_seen.value, 7);
        assert_eq!(
            GrowthStage::from_counts(memory.days_seen.value, memory.lifetime_recoveries.value),
            GrowthStage::Adult
        );
    }

    #[test]
    fn compressed_growth_ledger_shapes_keep_a_proven_closed_prefix() {
        let mut ledger = GrowthDayLedger::default();
        assert_eq!(ledger.observe(500), GrowthDayObservation::New);
        ledger.close_through(500);
        assert_eq!(ledger.compacted_through, Some(500));
        assert!(ledger.recent.is_empty());
        assert!(ledger.validate(1));
        assert_eq!(ledger.observe(500), GrowthDayObservation::Closed);

        assert_eq!(ledger.observe(502), GrowthDayObservation::New);
        assert_eq!(ledger.recent, [502]);
        assert!(ledger.validate(2));

        let no_cursor = GrowthDayLedger {
            compacted_through: None,
            recent: vec![502],
        };
        assert!(!no_cursor.validate(2));
        let no_history = GrowthDayLedger {
            compacted_through: Some(500),
            recent: Vec::new(),
        };
        assert!(!no_history.validate(0));
        let overlapping = GrowthDayLedger {
            compacted_through: Some(500),
            recent: vec![500],
        };
        assert!(!overlapping.validate(2));
    }

    #[test]
    fn lifetime_recoveries_follow_ordered_flip_deltas_not_arrival_order() {
        let repo = "/work/growth-order";
        let day = 500;
        let events = [
            MemoryEvent::fixed(
                10,
                day,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(1),
                LifeState::default(),
            ),
            MemoryEvent::fixed(
                20,
                day,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(1),
                LifeState::default(),
            ),
            MemoryEvent::fixed(
                30,
                day,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(0),
                LifeState::default(),
            ),
            MemoryEvent::fixed(
                40,
                day,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(1),
                LifeState::default(),
            ),
            MemoryEvent::fixed(
                50,
                day,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(0),
                LifeState::default(),
            ),
        ];
        let mut forward = DiskMemory::default();
        for event in &events {
            apply_event(&mut forward, event);
        }
        let mut reverse = DiskMemory::default();
        for event in events.iter().rev() {
            apply_event(&mut reverse, event);
        }

        assert_eq!(forward.lifetime_recoveries.value, 2);
        assert_eq!(reverse.lifetime_recoveries.value, 2);
        assert_eq!(
            forward
                .stats(day, repo)
                .unwrap()
                .failure_success_flips
                .value,
            2
        );
        assert_eq!(
            reverse
                .stats(day, repo)
                .unwrap()
                .failure_success_flips
                .value,
            2
        );

        // A second repository contributes another episode, never another day.
        for (at_ms, code) in [(60, 1), (70, 0)] {
            apply_event(
                &mut reverse,
                &MemoryEvent::fixed(
                    at_ms,
                    day,
                    Some("/work/growth-other"),
                    CommandKind::BuildOrTest,
                    Some(code),
                    LifeState::default(),
                ),
            );
        }
        assert_eq!(reverse.days_seen.value, 1);
        assert_eq!(reverse.lifetime_recoveries.value, 3);
        let before = reverse.lifetime_recoveries.value;
        apply_event(&mut reverse, &events[4]);
        assert_eq!(reverse.lifetime_recoveries.value, before);
        forward.validate().unwrap();
        reverse.validate().unwrap();
    }

    #[test]
    fn evicted_build_order_closes_late_recoveries_but_new_days_still_grow() {
        let day = 500;
        let repo = "/work/evicted-recovery-order";
        let mut memory = DiskMemory::default();
        for (at_ms, code) in [(200, 1), (300, 0)] {
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    at_ms,
                    day,
                    Some(repo),
                    CommandKind::BuildOrTest,
                    Some(code),
                    LifeState::default(),
                ),
            );
        }
        assert_eq!(memory.days_seen.value, 1);
        assert_eq!(memory.lifetime_recoveries.value, 1);

        // More repo/day records can evict this build history while its day is
        // still far below the distinct-day ledger's own 64-day capacity.
        // Pure-push records carry no recovery ordering and must not close the
        // prefix when one of them is later displaced.
        for index in 0..MAX_DAILY_RECORDS {
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    1_000 + index as u64,
                    day + 1,
                    Some(&format!("/work/push-filler-{index:02}")),
                    CommandKind::GitPush,
                    Some(0),
                    LifeState::default(),
                ),
            );
        }
        assert!(memory.stats(day, repo).is_none());
        assert_eq!(memory.days_seen.value, 2);
        assert_eq!(memory.growth_days.value.compacted_through, Some(day));
        assert_eq!(memory.growth_days.value.recent, [day + 1]);

        // In the complete order F100,F200,S250,S300 is one episode. Replaying
        // only the late pair after record eviction must not count it again.
        for (at_ms, code) in [(100, 1), (250, 0)] {
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    at_ms,
                    day,
                    Some(repo),
                    CommandKind::BuildOrTest,
                    Some(code),
                    LifeState::default(),
                ),
            );
        }
        assert_eq!(
            memory.stats(day, repo).unwrap().failure_success_flips.value,
            1
        );
        assert_eq!(memory.lifetime_recoveries.value, 1);
        assert_eq!(memory.growth_days.value.compacted_through, Some(day));
        assert_eq!(memory.growth_days.value.recent, [day + 1]);

        for (at_ms, code) in [(2_000, 1), (2_100, 0)] {
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    at_ms,
                    day + 2,
                    Some("/work/new-recovery-day"),
                    CommandKind::BuildOrTest,
                    Some(code),
                    LifeState::default(),
                ),
            );
        }
        assert_eq!(memory.days_seen.value, 3);
        assert_eq!(memory.lifetime_recoveries.value, 2);
        assert_eq!(memory.growth_days.value.recent, [day + 1, day + 2]);
        memory.validate().unwrap();
    }

    #[test]
    fn lifetime_recoveries_require_at_least_one_work_day() {
        let memory = DiskMemory {
            lifetime_recoveries: MigratingCounter::new(1),
            ..DiskMemory::default()
        };

        assert!(memory
            .validate()
            .unwrap_err()
            .to_string()
            .contains("recoveries without a work day"));
    }

    #[test]
    fn compressed_growth_requires_a_writer_reachable_daily_window() {
        let memory = DiskMemory {
            days_seen: MigratingCounter::new(1),
            growth_days: MigratingGrowthDayLedger::new(GrowthDayLedger {
                compacted_through: Some(i64::MAX),
                recent: Vec::new(),
            }),
            ..DiskMemory::default()
        };

        assert!(memory
            .validate()
            .unwrap_err()
            .to_string()
            .contains("compressed growth ledger without a full daily window"));

        let mut anvild = memory;
        for index in 0..MAX_DAILY_RECORDS {
            anvild
                .days
                .push(DailyStats::new(0, format!("/tmp/empty-{index}")));
        }
        assert!(anvild
            .validate()
            .unwrap_err()
            .to_string()
            .contains("growth cursor beyond retained daily history"));
    }

    #[test]
    fn recent_growth_days_require_retained_semantic_evidence_below_capacity() {
        let empty = DiskMemory {
            days_seen: MigratingCounter::new(1),
            growth_days: MigratingGrowthDayLedger::new(GrowthDayLedger {
                compacted_through: None,
                recent: vec![5],
            }),
            ..DiskMemory::default()
        };
        assert!(empty
            .validate()
            .unwrap_err()
            .to_string()
            .contains("recent growth day without semantic evidence"));

        let mut missing_one = DiskMemory::default();
        for day in [5, 6] {
            apply_event(
                &mut missing_one,
                &MemoryEvent::fixed(
                    day as u64,
                    day,
                    Some("/work/growth-provenance"),
                    CommandKind::GitPush,
                    Some(0),
                    LifeState::default(),
                ),
            );
        }
        missing_one.days.retain(|stats| stats.day != 6);
        assert_eq!(missing_one.days.len(), 1);
        assert_eq!(missing_one.growth_days.value.recent, [5, 6]);
        assert!(missing_one
            .validate()
            .unwrap_err()
            .to_string()
            .contains("recent growth day without semantic evidence"));
    }

    #[test]
    fn closed_growth_days_stay_persistable_at_capacity_and_after_migration() {
        let root = TestDir::new("closed-growth-day");
        let path = root.memory_path();
        let mut memory = DiskMemory::default();
        for offset in 0..MAX_RECENT_GROWTH_DAYS as i64 {
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    1_000 + offset as u64,
                    100 + offset,
                    Some("/work/full-growth-window"),
                    CommandKind::GitPush,
                    Some(0),
                    LifeState::default(),
                ),
            );
        }
        assert_eq!(memory.days_seen.value, MAX_RECENT_GROWTH_DAYS as u32);
        assert_eq!(memory.growth_days.value.compacted_through, None);
        let mut bytes = serde_json::to_vec_pretty(&memory).unwrap();
        bytes.push(b'\n');
        jterm_core::snapshot_file::ensure_private_directory(path.parent().unwrap()).unwrap();
        jterm_core::snapshot_file::write_atomic_private(&path, &bytes).unwrap();

        let late = MemoryEvent::fixed(
            10_000,
            99,
            Some("/work/late-at-capacity"),
            CommandKind::BuildOrTest,
            Some(1),
            LifeState::default(),
        );
        transact(&path, &late).unwrap();
        let persisted = read_memory(&path).unwrap();
        assert_eq!(persisted.days_seen.value, MAX_RECENT_GROWTH_DAYS as u32);
        assert!(persisted.stats(99, "/work/late-at-capacity").is_some());
        persisted.validate().unwrap();

        let migrated_root = TestDir::new("closed-growth-migration");
        let migrated_path = migrated_root.memory_path();
        let mut retained = DiskMemory::default();
        apply_event(
            &mut retained,
            &MemoryEvent::fixed(
                20_000,
                500,
                Some("/work/migrated-growth"),
                CommandKind::GitPush,
                Some(0),
                LifeState::default(),
            ),
        );
        let mut legacy = serde_json::to_value(&retained).unwrap();
        strip_current_organism_families(&mut legacy);
        let mut bytes = serde_json::to_vec_pretty(&legacy).unwrap();
        bytes.push(b'\n');
        jterm_core::snapshot_file::ensure_private_directory(migrated_path.parent().unwrap())
            .unwrap();
        jterm_core::snapshot_file::write_atomic_private(&migrated_path, &bytes).unwrap();

        let late = MemoryEvent::fixed(
            20_001,
            499,
            Some("/work/migrated-late"),
            CommandKind::BuildOrTest,
            Some(1),
            LifeState::default(),
        );
        transact(&migrated_path, &late).unwrap();
        let persisted = read_memory(&migrated_path).unwrap();
        assert_eq!(persisted.days_seen.value, 1);
        assert_eq!(persisted.growth_days.value.compacted_through, Some(499));
        assert!(persisted.stats(499, "/work/migrated-late").is_some());
        persisted.validate().unwrap();
    }

    #[test]
    fn growth_survives_observation_and_daily_compaction_and_closes_late_days() {
        let repo = "/work/growth-compaction";
        let day = 700;
        let mut memory = DiskMemory::default();
        for cycle in 0..80_u64 {
            for (offset, code) in [(0, 1), (1, 0)] {
                apply_event(
                    &mut memory,
                    &MemoryEvent::fixed(
                        1_000 + cycle * 2 + offset,
                        day,
                        Some(repo),
                        CommandKind::BuildOrTest,
                        Some(code),
                        LifeState::default(),
                    ),
                );
            }
        }
        assert_eq!(memory.days_seen.value, 1);
        assert_eq!(memory.lifetime_recoveries.value, 80);
        let stats = memory.stats(day, repo).unwrap();
        assert_eq!(stats.failure_success_flips.value, 80);
        assert!(stats.baseline.is_some());
        assert!(stats.observations.len() <= MAX_OBSERVATIONS_PER_RECORD);
        for (at_ms, code) in [(1, 1), (2, 0)] {
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    at_ms,
                    day,
                    Some(repo),
                    CommandKind::BuildOrTest,
                    Some(code),
                    LifeState::default(),
                ),
            );
        }
        assert_eq!(memory.lifetime_recoveries.value, 80);
        assert_eq!(
            memory.stats(day, repo).unwrap().failure_success_flips.value,
            80
        );

        let mut bounded = DiskMemory::default();
        for offset in 0..=MAX_RECENT_GROWTH_DAYS as i64 {
            apply_event(
                &mut bounded,
                &MemoryEvent::fixed(
                    10_000 + offset as u64,
                    1_000 + offset,
                    Some("/work/growth-days"),
                    CommandKind::BuildOrTest,
                    None,
                    LifeState::default(),
                ),
            );
        }
        assert_eq!(bounded.days_seen.value, 65);
        assert_eq!(bounded.days.len(), MAX_DAILY_RECORDS);
        assert_eq!(
            bounded.growth_days.value.recent.len(),
            MAX_RECENT_GROWTH_DAYS
        );
        assert_eq!(bounded.growth_days.value.compacted_through, Some(1_000));

        // A genuinely new pair on a closed old day may still update its
        // repo/day summary, but can never manufacture more lifetime growth.
        for (at_ms, code) in [(90_000, 1), (90_001, 0)] {
            apply_event(
                &mut bounded,
                &MemoryEvent::fixed(
                    at_ms,
                    1_000,
                    Some("/work/late-growth"),
                    CommandKind::BuildOrTest,
                    Some(code),
                    LifeState::default(),
                ),
            );
        }
        assert_eq!(bounded.days_seen.value, 65);
        assert_eq!(bounded.lifetime_recoveries.value, 0);

        for (at_ms, code) in [(91_000, 1), (91_001, 0)] {
            apply_event(
                &mut bounded,
                &MemoryEvent::fixed(
                    at_ms,
                    1_064,
                    Some("/work/open-growth"),
                    CommandKind::BuildOrTest,
                    Some(code),
                    LifeState::default(),
                ),
            );
        }
        assert_eq!(bounded.lifetime_recoveries.value, 1);

        bounded.days_seen = MigratingCounter::new(u32::MAX);
        bounded.lifetime_recoveries = MigratingCounter::new(u32::MAX);
        apply_event(
            &mut bounded,
            &MemoryEvent::fixed(
                92_000,
                1_065,
                Some("/work/saturated-growth"),
                CommandKind::BuildOrTest,
                None,
                LifeState::default(),
            ),
        );
        for (at_ms, code) in [(92_001, 1), (92_002, 0)] {
            apply_event(
                &mut bounded,
                &MemoryEvent::fixed(
                    at_ms,
                    1_065,
                    Some("/work/saturated-growth"),
                    CommandKind::BuildOrTest,
                    Some(code),
                    LifeState::default(),
                ),
            );
        }
        assert_eq!(bounded.days_seen.value, u32::MAX);
        assert_eq!(bounded.lifetime_recoveries.value, u32::MAX);
        memory.validate().unwrap();
        bounded.validate().unwrap();
    }

    #[test]
    fn activity_buckets_count_every_repo_semantic_finish_exactly_once() {
        let repo = "/work/circadian";
        let day = 60_100;
        let mut memory = DiskMemory::default();

        let unknown_build = MemoryEvent::fixed(
            1_000,
            day,
            Some(repo),
            CommandKind::BuildOrTest,
            None,
            LifeState::default(),
        )
        .with_activity_bucket(2);
        apply_event(&mut memory, &unknown_build);
        apply_event(&mut memory, &unknown_build);

        let failed_push = MemoryEvent::fixed(
            2_000,
            day,
            Some(repo),
            CommandKind::GitPush,
            Some(1),
            LifeState::default(),
        )
        .with_activity_bucket(5);
        apply_event(&mut memory, &failed_push);
        apply_event(&mut memory, &failed_push);

        // Even an explicitly supplied repo cannot make an arbitrary command
        // teach the circadian profile.
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                3_000,
                day,
                Some(repo),
                CommandKind::Other,
                Some(0),
                LifeState::default(),
            )
            .with_activity_bucket(7),
        );

        let stats = memory.stats(day, repo).unwrap();
        assert_eq!(stats.activity_buckets, [0, 0, 1, 0, 0, 1, 0, 0]);
        assert!(memory.has_event_id(&unknown_build.id));
        assert!(memory.has_event_id(&failed_push.id));
        assert!(stats.observations.is_empty());

        // Saturation does not make a new event retryable: its id is retained
        // even though the monotone scalar can no longer move.
        memory.stats_mut(day, repo).activity_buckets[2] = u16::MAX;
        let saturated = MemoryEvent::fixed(
            4_000,
            day,
            Some(repo),
            CommandKind::BuildOrTest,
            Some(1),
            LifeState::default(),
        )
        .with_activity_bucket(2);
        apply_event(&mut memory, &saturated);
        apply_event(&mut memory, &saturated);
        assert_eq!(
            memory.stats(day, repo).unwrap().activity_buckets[2],
            u16::MAX
        );
        assert!(memory.has_event_id(&saturated.id));
        memory.validate().unwrap();
    }

    #[test]
    fn activity_buckets_are_commutative_and_survive_observation_compaction() {
        let repo = "/work/circadian-order";
        let day = 60_101;
        let events: Vec<_> = (0..(MAX_OBSERVATIONS_PER_RECORD + 17))
            .map(|index| {
                MemoryEvent::fixed(
                    10_000 + index as u64,
                    day,
                    Some(repo),
                    CommandKind::BuildOrTest,
                    Some(i32::from(index.is_multiple_of(3))),
                    LifeState::default(),
                )
                .with_activity_bucket((index % CIRCADIAN_BUCKET_COUNT) as u8)
            })
            .collect();
        let mut forward = DiskMemory::default();
        for event in &events {
            apply_event(&mut forward, event);
        }
        let mut reverse = DiskMemory::default();
        for event in events.iter().rev() {
            apply_event(&mut reverse, event);
        }

        let forward_stats = forward.stats(day, repo).unwrap();
        let reverse_stats = reverse.stats(day, repo).unwrap();
        assert_eq!(
            forward_stats.activity_buckets,
            reverse_stats.activity_buckets
        );
        assert_eq!(
            forward_stats
                .activity_buckets
                .iter()
                .map(|count| usize::from(*count))
                .sum::<usize>(),
            events.len()
        );
        assert!(forward_stats.baseline.is_some());
        assert!(forward_stats.observations.len() <= MAX_OBSERVATIONS_PER_RECORD);

        let before = forward_stats.activity_buckets;
        compact_oldest_observations(forward.stats_mut(day, repo));
        assert_eq!(forward.stats(day, repo).unwrap().activity_buckets, before);
        forward.validate().unwrap();
        reverse.validate().unwrap();
    }

    #[test]
    fn circadian_profile_needs_three_completed_days_and_a_concentrated_window() {
        let today = 10_000;
        let mut memory = DiskMemory::default();
        let set = |memory: &mut DiskMemory, day, repo: &str, buckets| {
            memory.stats_mut(day, repo).activity_buckets = buckets;
        };

        let mut early = [0_u16; CIRCADIAN_BUCKET_COUNT];
        early[3] = 2;
        let mut middle = [0_u16; CIRCADIAN_BUCKET_COUNT];
        middle[4] = 2;
        let mut late = [0_u16; CIRCADIAN_BUCKET_COUNT];
        late[5] = 2;
        set(&mut memory, today - 3, "/work/a", early);
        set(&mut memory, today - 2, "/work/b", middle);
        // A second repo on an existing day adds weight, not another active day.
        set(
            &mut memory,
            today - 2,
            "/work/c",
            [0; CIRCADIAN_BUCKET_COUNT],
        );
        assert_eq!(infer_circadian_profile(&memory, today), None);

        set(&mut memory, today - 1, "/work/d", late);
        let profile = infer_circadian_profile(&memory, today).unwrap();
        assert_eq!(profile.mask(), 0b0011_1000);
        for bucket in [3, 4, 5] {
            assert!(profile.contains(bucket));
        }
        assert!(!profile.contains(2));
        assert!(!profile.contains(6));

        // Current, future, and stale records are excluded and cannot pull the
        // already learned completed-day profile toward another time of day.
        let mut noise = [0_u16; CIRCADIAN_BUCKET_COUNT];
        noise[0] = 1_000;
        set(&mut memory, today, "/work/today", noise);
        set(&mut memory, today + 1, "/work/future", noise);
        set(
            &mut memory,
            today - CIRCADIAN_LOOKBACK_DAYS - 1,
            "/work/stale",
            noise,
        );
        assert_eq!(infer_circadian_profile(&memory, today), Some(profile));

        let mut uniform = DiskMemory::default();
        for offset in 1..=3 {
            set(
                &mut uniform,
                today - offset,
                &format!("/work/uniform-{offset}"),
                [1; CIRCADIAN_BUCKET_COUNT],
            );
        }
        assert_eq!(infer_circadian_profile(&uniform, today), None);

        // Two equally strong but disjoint clusters do not establish one
        // habitual window: concentration must be a strict majority.
        let mut bimodal = DiskMemory::default();
        for offset in 1..=3 {
            let mut buckets = [0_u16; CIRCADIAN_BUCKET_COUNT];
            buckets[0] = 2;
            buckets[4] = 2;
            set(
                &mut bimodal,
                today - offset,
                &format!("/work/bimodal-{offset}"),
                buckets,
            );
        }
        assert_eq!(infer_circadian_profile(&bimodal, today), None);
    }

    #[test]
    fn circadian_profile_wraps_a_night_shift_across_midnight() {
        let today = 11_000;
        let mut memory = DiskMemory::default();
        for (offset, bucket) in [(1, 7), (2, 0), (3, 1)] {
            let mut buckets = [0_u16; CIRCADIAN_BUCKET_COUNT];
            buckets[bucket] = 2;
            memory
                .stats_mut(today - offset, &format!("/work/night-{offset}"))
                .activity_buckets = buckets;
        }
        let profile = infer_circadian_profile(&memory, today).unwrap();
        assert_eq!(profile.mask(), 0b1000_0011);
        assert!(profile.contains(7));
        assert!(profile.contains(0));
        assert!(profile.contains(1));
        assert!(!profile.contains(CIRCADIAN_BUCKET_COUNT as u8));
        assert_eq!(
            profile.session_day(LocalCircadianTime {
                day: today - 1,
                bucket: 7,
            }),
            today - 1
        );
        assert_eq!(
            profile.session_day(LocalCircadianTime {
                day: today,
                bucket: 0,
            }),
            today - 1
        );
        assert_eq!(
            profile.session_day(LocalCircadianTime {
                day: today,
                bucket: 1,
            }),
            today - 1
        );
    }

    #[test]
    fn old_v1_growth_fields_migrate_to_a_conservative_lower_bound() {
        let root = TestDir::new("growth-migration");
        let path = root.memory_path();
        let repo = root.0.join("repo");
        let repo = repo.to_str().unwrap();
        let mut memory = DiskMemory::default();
        for (at_ms, code) in [(1_000, 1), (2_000, 0)] {
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    at_ms,
                    12_000,
                    Some(repo),
                    CommandKind::BuildOrTest,
                    Some(code),
                    LifeState::default(),
                ),
            );
        }
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                3_000,
                12_002,
                Some("/work/other"),
                CommandKind::GitPush,
                Some(1),
                LifeState::default(),
            ),
        );
        // A structurally valid but evidence-free legacy record is not a day
        // the organism can honestly claim to have seen.
        memory.stats_mut(11_999, "/work/empty");

        let current = serde_json::to_value(&memory).unwrap();
        let mut legacy = current.clone();
        strip_current_organism_families(&mut legacy);
        let mut bytes = serde_json::to_vec_pretty(&legacy).unwrap();
        bytes.push(b'\n');
        jterm_core::snapshot_file::ensure_private_directory(path.parent().unwrap()).unwrap();
        jterm_core::snapshot_file::write_atomic_private(&path, &bytes).unwrap();

        let migrated = read_memory(&path).unwrap();
        // The failed push day was visible only through the new activity
        // buckets. A released-HEAD file had neither field, so migration keeps
        // the provable build/recovery day and does not invent the second.
        assert_eq!(migrated.days_seen.value, 1);
        assert_eq!(migrated.lifetime_recoveries.value, 1);
        assert_eq!(migrated.growth_days.value.recent, [12_000]);
        assert_eq!(migrated.growth_days.value.compacted_through, Some(11_999));
        migrated.validate().unwrap();

        // These three fields are one atomic schema addition. A released
        // writer emits all three, so any partial absence is corruption rather
        // than a legacy file.
        for missing in ["days_seen", "lifetime_recoveries", "growth_days"] {
            let mut partial = current.clone();
            partial.as_object_mut().unwrap().remove(missing);
            let partial_path = root.0.join(format!("partial-{missing}.json"));
            let mut bytes = serde_json::to_vec_pretty(&partial).unwrap();
            bytes.push(b'\n');
            jterm_core::snapshot_file::write_atomic_private(&partial_path, &bytes).unwrap();
            assert!(read_memory(&partial_path)
                .unwrap_err()
                .to_string()
                .contains("incomplete growth ledger"));
        }

        let mut invalid = memory.clone();
        invalid.days_seen = MigratingCounter::new(u32::MAX);
        invalid.growth_days = MigratingGrowthDayLedger::new(GrowthDayLedger::default());
        assert!(invalid.validate().is_err());

        let mut invalid = memory.clone();
        invalid.growth_days.value.compacted_through = Some(i64::MAX);
        invalid.growth_days.value.recent.clear();
        invalid.days_seen = MigratingCounter::new(0);
        assert!(invalid.validate().is_err());

        let mut invalid = migrated.clone();
        invalid.growth_days.value.recent.remove(0);
        invalid.growth_days.value.compacted_through = None;
        invalid.days_seen = MigratingCounter::new(1);
        assert!(invalid.validate().is_err());

        let mut invalid = migrated.clone();
        invalid.growth_days.value.compacted_through = Some(11_998);
        assert!(invalid.validate().is_err());

        for missing_nested in ["compacted_through", "recent"] {
            let mut malformed = current.clone();
            malformed["growth_days"]
                .as_object_mut()
                .unwrap()
                .remove(missing_nested);
            assert!(
                serde_json::from_value::<DiskMemory>(malformed).is_err(),
                "missing nested growth field was accepted: {missing_nested}"
            );
        }
    }

    #[test]
    fn old_activity_schema_defaults_to_unlearned_and_malformed_arrays_fail_strictly() {
        let root = TestDir::new("circadian-migration");
        let path = root.memory_path();
        let repo = root.0.join("repo");
        let repo = repo.to_str().unwrap();
        let mut memory = DiskMemory::default();
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                1_000,
                12_000,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(0),
                LifeState::default(),
            )
            .with_activity_bucket(4),
        );
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                2_000,
                12_001,
                Some(repo),
                CommandKind::GitPush,
                Some(0),
                LifeState::default(),
            )
            .with_activity_bucket(5),
        );

        let mut legacy = serde_json::to_value(&memory).unwrap();
        strip_current_organism_families(&mut legacy);
        let mut bytes = serde_json::to_vec_pretty(&legacy).unwrap();
        bytes.push(b'\n');
        jterm_core::snapshot_file::ensure_private_directory(path.parent().unwrap()).unwrap();
        jterm_core::snapshot_file::write_atomic_private(&path, &bytes).unwrap();
        let migrated = read_memory(&path).unwrap();
        assert_eq!(
            migrated.stats(12_000, repo).unwrap().activity_buckets,
            [0; CIRCADIAN_BUCKET_COUNT]
        );
        assert_eq!(infer_circadian_profile(&migrated, 12_001), None);

        let mut mixed = serde_json::to_value(&memory).unwrap();
        mixed["days"][0]
            .as_object_mut()
            .unwrap()
            .remove("activity_buckets");
        let mixed_path = root.0.join("mixed.json");
        let mut bytes = serde_json::to_vec_pretty(&mixed).unwrap();
        bytes.push(b'\n');
        jterm_core::snapshot_file::write_atomic_private(&mixed_path, &bytes).unwrap();
        assert!(read_memory(&mixed_path)
            .unwrap_err()
            .to_string()
            .contains("mixes activity-bucket"));

        for invalid in [
            serde_json::json!([0, 0, 0, 0, 0, 0, 0]),
            serde_json::json!([0, 0, 0, 0, 0, 0, 0, 0, 0]),
            serde_json::json!([0, 0, 0, 0, 0, 0, 0, 65_536]),
        ] {
            let mut value = serde_json::to_value(&memory).unwrap();
            value["days"][0]["activity_buckets"] = invalid;
            assert!(serde_json::from_value::<DiskMemory>(value).is_err());
        }
    }

    #[test]
    fn yesterday_comparison_is_exact_and_repo_local() {
        let repo = "/work/anvil";
        let other = "/work/other";
        let day = 50_000;
        let mut memory = DiskMemory::default();

        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                1_000,
                day - 1,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(1),
                LifeState::default(),
            ),
        );
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                11_000,
                day - 1,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(0),
                LifeState::default(),
            ),
        );
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                20_000,
                day,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(1),
                LifeState::default(),
            ),
        );
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                25_000,
                day,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(0),
                LifeState::default(),
            ),
        );
        let insight = apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                26_000,
                day,
                Some(repo),
                CommandKind::GitPush,
                Some(0),
                LifeState::default(),
            ),
        );
        assert!(insight.push_after_recovery);
        assert!(insight.faster_than_yesterday);

        let mut gap = memory.clone();
        gap.days.retain(|stats| stats.day != day - 1);
        assert!(!faster_than_previous_day(&gap, day, repo));
        assert!(!faster_than_previous_day(&memory, day, other));

        let today = memory.stats_mut(day, repo);
        today.last_recovery_duration_ms = Some(10_000);
        assert!(!faster_than_previous_day(&memory, day, repo));
    }

    #[test]
    fn corrupt_or_future_memory_is_never_replaced_by_a_transaction() {
        let root = TestDir::new("fail-closed");
        let path = root.memory_path();
        jterm_core::snapshot_file::ensure_private_directory(path.parent().unwrap()).unwrap();
        let corrupt = b"{\"version\":99,\"life\":{},\"life_updated_at_ms\":0,\"days\":[]}";
        jterm_core::snapshot_file::write_atomic_private(&path, corrupt).unwrap();
        let repo = root.0.join("repo");
        let result = transact(
            &path,
            &event(1, 1, &repo, CommandKind::BuildOrTest, Some(1)),
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), corrupt);
    }

    #[cfg(unix)]
    #[test]
    fn repo_paths_are_never_loaded_from_a_group_or_world_readable_file() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new("private-read");
        let path = root.memory_path();
        let repo = root.0.join("repo");
        transact(
            &path,
            &event(1, 1, &repo, CommandKind::BuildOrTest, Some(1)),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(OrganismMemory::load(path).is_err());
    }

    #[test]
    fn mixed_writer_order_replays_failure_recovery_and_push_by_event_time() {
        let repo = "/work/ordered";
        let day = 42;
        let mut memory = DiskMemory::default();
        let failure = MemoryEvent::fixed(
            1_000,
            day,
            Some(repo),
            CommandKind::BuildOrTest,
            Some(1),
            LifeState::default(),
        );
        let success = MemoryEvent::fixed(
            5_000,
            day,
            Some(repo),
            CommandKind::BuildOrTest,
            Some(0),
            LifeState::default(),
        );
        let push = MemoryEvent::fixed(
            6_000,
            day,
            Some(repo),
            CommandKind::GitPush,
            Some(0),
            LifeState::default(),
        );

        // Simulate three processes acquiring the filesystem lock backwards.
        apply_event(&mut memory, &push);
        apply_event(&mut memory, &success);
        apply_event(&mut memory, &failure);

        let stats = memory.stats(day, repo).unwrap();
        assert_eq!(stats.build_failures, 1);
        assert_eq!(stats.build_successes, 1);
        assert_eq!(stats.git_pushes, 1);
        assert_eq!(stats.open_failures, 0);
        assert_eq!(stats.last_recovery_duration_ms, Some(4_000));
        assert!(!stats.recovered_pending_push);
        memory.validate().unwrap();
    }

    #[test]
    fn every_semantic_insight_reports_the_post_replay_repo_work_state() {
        let repo = "/work/guard-replay";
        let day = 43;
        let mut memory = DiskMemory::default();
        let success = MemoryEvent::fixed(
            5_000,
            day,
            Some(repo),
            CommandKind::BuildOrTest,
            Some(0),
            LifeState::default(),
        );
        let late_failure = MemoryEvent::fixed(
            1_000,
            day,
            Some(repo),
            CommandKind::BuildOrTest,
            Some(1),
            LifeState::default(),
        );
        let failed_push = MemoryEvent::fixed(
            6_000,
            day,
            Some(repo),
            CommandKind::GitPush,
            Some(1),
            LifeState::default(),
        );
        let pushed = MemoryEvent::fixed(
            7_000,
            day,
            Some(repo),
            CommandKind::GitPush,
            Some(0),
            LifeState::default(),
        );

        let first_success = apply_event(&mut memory, &success);
        assert!(first_success.event_order_exact);
        assert_eq!(first_success.current_work, RepoWorkState::default());
        let replayed = apply_event(&mut memory, &late_failure);
        assert!(replayed.event_order_exact);
        assert_eq!(
            replayed.open_failures, 1,
            "the event itself opened one failure"
        );
        assert_eq!(
            replayed.current_work,
            RepoWorkState::new(0, true, 1),
            "ordered replay turns the earlier success into a finished recovery"
        );
        let failed = apply_event(&mut memory, &failed_push);
        assert!(!failed.event_order_exact);
        assert_eq!(failed.current_work, RepoWorkState::new(0, true, 1));
        let closed = apply_event(&mut memory, &pushed);
        assert!(closed.event_order_exact);
        assert_eq!(closed.current_work, RepoWorkState::new(0, false, 1));
    }

    #[test]
    fn duplicate_unknown_and_failed_push_keep_the_current_open_failure() {
        let repo = "/work/open-state";
        let day = 44;
        let mut memory = DiskMemory::default();
        let failure = MemoryEvent::fixed(
            5_000,
            day,
            Some(repo),
            CommandKind::BuildOrTest,
            Some(1),
            LifeState::default(),
        );
        let unknown = MemoryEvent::fixed(
            6_000,
            day,
            Some(repo),
            CommandKind::BuildOrTest,
            None,
            LifeState::default(),
        );
        let failed_push = MemoryEvent::fixed(
            7_000,
            day,
            Some(repo),
            CommandKind::GitPush,
            Some(1),
            LifeState::default(),
        );

        let open = RepoWorkState::new(1, false, 0);
        let first = apply_event(&mut memory, &failure);
        assert!(first.event_order_exact);
        assert_eq!(first.current_work, open);
        let duplicate = apply_event(&mut memory, &failure);
        assert!(!duplicate.event_order_exact);
        assert_eq!(duplicate.current_work, open);
        let unknown = apply_event(&mut memory, &unknown);
        assert!(!unknown.event_order_exact);
        assert_eq!(unknown.current_work, open);
        let failed_push = apply_event(&mut memory, &failed_push);
        assert!(!failed_push.event_order_exact);
        assert_eq!(failed_push.current_work, open);

        // This success belongs before the retained failure. Its event-local
        // depth is clear, but the post-replay snapshot must remain failed.
        let late_success = MemoryEvent::fixed(
            1_000,
            day,
            Some(repo),
            CommandKind::BuildOrTest,
            Some(0),
            LifeState::default(),
        );
        let insight = apply_event(&mut memory, &late_success);
        assert_eq!(insight.recovered_failures, 0);
        assert_eq!(insight.current_work, open);
    }

    #[test]
    fn observation_window_compacts_without_losing_aggregate_state() {
        let repo = "/work/hot-repo";
        let mut memory = DiskMemory::default();
        let failures = u64::try_from(MAX_OBSERVATIONS_PER_RECORD).unwrap() + 73;
        for index in 0..failures {
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    10_000 + index,
                    99,
                    Some(repo),
                    CommandKind::BuildOrTest,
                    Some(1),
                    LifeState::default(),
                ),
            );
        }
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                20_000,
                99,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(0),
                LifeState::default(),
            ),
        );
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                21_000,
                99,
                Some(repo),
                CommandKind::GitPush,
                Some(0),
                LifeState::default(),
            ),
        );
        let stats = memory.stats(99, repo).unwrap();
        assert_eq!(stats.build_failures, failures as u32);
        assert_eq!(stats.build_successes, 1);
        assert_eq!(stats.git_pushes, 1);
        assert_eq!(stats.open_failures, 0);
        assert!(!stats.recovered_pending_push);
        assert!(stats.baseline.is_some());
        assert!(stats.observations.len() <= MAX_OBSERVATIONS_PER_RECORD);

        let baseline_failures_before = stats.baseline.as_ref().unwrap().build_failures;
        let retained_before = stats.observations.len();
        let watermark = stats
            .baseline
            .as_ref()
            .and_then(|baseline| baseline.compacted_through.as_ref())
            .unwrap()
            .at_ms;
        let compacted_late = apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                watermark.saturating_sub(1),
                99,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(1),
                LifeState::default(),
            ),
        );
        assert!(!compacted_late.event_order_exact);
        assert_eq!(compacted_late.current_work, RepoWorkState::new(0, false, 1));
        let stats = memory.stats(99, repo).unwrap();
        assert_eq!(stats.build_failures, failures as u32 + 1);
        assert_eq!(stats.build_successes, 1);
        assert_eq!(stats.git_pushes, 1);
        assert_eq!(stats.open_failures, 0);
        assert_eq!(stats.observations.len(), retained_before);
        assert_eq!(
            stats.baseline.as_ref().unwrap().build_failures,
            baseline_failures_before + 1
        );
        memory.validate().unwrap();
        let mut serialized = serde_json::to_vec_pretty(&memory).unwrap();
        serialized.push(b'\n');
        assert!(serialized.len() as u64 <= MAX_MEMORY_BYTES);
    }

    #[test]
    fn an_older_event_at_the_compaction_boundary_is_ordered_before_folding() {
        let repo = "/work/compaction-boundary";
        let mut memory = DiskMemory::default();
        for index in 0..MAX_OBSERVATIONS_PER_RECORD {
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    100 + index as u64,
                    100,
                    Some(repo),
                    CommandKind::BuildOrTest,
                    Some(1),
                    LifeState::default(),
                ),
            );
        }
        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                50,
                100,
                Some(repo),
                CommandKind::BuildOrTest,
                Some(1),
                LifeState::default(),
            ),
        );

        let stats = memory.stats(100, repo).unwrap();
        assert_eq!(stats.build_failures, 65);
        assert_eq!(stats.open_failures, 65);
        assert!(stats.observations.len() <= MAX_OBSERVATIONS_PER_RECORD);
        memory.validate().unwrap();
    }

    #[test]
    fn a_compacted_event_id_remains_idempotent_across_ambiguous_retry() {
        let repo = "/work/idempotent";
        let mut memory = DiskMemory::default();
        let first = MemoryEvent::fixed(
            1_000,
            101,
            Some(repo),
            CommandKind::BuildOrTest,
            Some(1),
            LifeState::default(),
        );
        apply_event(&mut memory, &first);
        for index in 1..=MAX_OBSERVATIONS_PER_RECORD {
            apply_event(
                &mut memory,
                &MemoryEvent::fixed(
                    1_000 + index as u64,
                    101,
                    Some(repo),
                    CommandKind::BuildOrTest,
                    Some(1),
                    LifeState::default(),
                ),
            );
        }
        let before = memory.stats(101, repo).unwrap().clone();
        let recent_before = memory.recent_event_ids.len();
        apply_event(&mut memory, &first);
        let after = memory.stats(101, repo).unwrap();
        assert_eq!(after.build_failures, before.build_failures);
        assert_eq!(after.open_failures, before.open_failures);
        assert_eq!(after.observations.len(), before.observations.len());
        assert_eq!(memory.recent_event_ids.len(), recent_before);
        memory.validate().unwrap();
    }

    #[test]
    fn compaction_boundary_id_remains_exact_after_recent_token_eviction() {
        let repo = "/work/cursor-idempotent";
        let mut memory = DiskMemory::default();
        let events: Vec<_> = (0..=MAX_OBSERVATIONS_PER_RECORD)
            .map(|index| {
                MemoryEvent::fixed(
                    3_000 + index as u64,
                    103,
                    Some(repo),
                    CommandKind::BuildOrTest,
                    Some(1),
                    LifeState::default(),
                )
            })
            .collect();
        for event in &events {
            apply_event(&mut memory, event);
        }
        let cursor_id = memory
            .stats(103, repo)
            .unwrap()
            .baseline
            .as_ref()
            .unwrap()
            .compacted_through
            .as_ref()
            .unwrap()
            .id
            .clone();
        let cursor_event = events.iter().find(|event| event.id == cursor_id).unwrap();
        memory.recent_event_ids.retain(|id| id != &cursor_id);
        assert!(memory.has_event_id(&cursor_id));

        let before = memory.stats(103, repo).unwrap().build_failures;
        apply_event(&mut memory, cursor_event);
        assert_eq!(memory.stats(103, repo).unwrap().build_failures, before);
        memory.validate().unwrap();
    }

    #[test]
    fn a_retried_compacted_transaction_is_idempotent_on_disk() {
        let root = TestDir::new("ambiguous-retry");
        let path = root.memory_path();
        let repo = root.0.join("repo");
        let events: Vec<_> = (0..=MAX_OBSERVATIONS_PER_RECORD)
            .map(|index| {
                event(
                    2_000 + index as u64,
                    102,
                    &repo,
                    CommandKind::BuildOrTest,
                    Some(1),
                )
            })
            .collect();
        transact_batch(&path, &events).unwrap();
        // Models rename succeeding while the first caller observes an error
        // from the final durability step and submits the exact batch again.
        transact_batch(&path, &events).unwrap();

        let memory = read_memory(&path).unwrap();
        let stats = memory.stats(102, repo.to_str().unwrap()).unwrap();
        assert_eq!(stats.build_failures, 65);
        assert_eq!(stats.open_failures, 65);
        memory.validate().unwrap();
    }

    #[test]
    fn global_observation_compaction_preserves_every_daily_aggregate() {
        let mut memory = DiskMemory::default();
        let repos: Vec<_> = (0..5).map(|index| format!("/work/repo-{index}")).collect();
        let mut duplicate = None;
        for (repo_index, repo) in repos.iter().enumerate() {
            let count = if repo_index == 0 { 52 } else { 51 };
            for event_index in 0..count {
                let event = MemoryEvent::fixed(
                    10_000 + (repo_index * 100 + event_index) as u64,
                    200 + repo_index as i64,
                    Some(repo),
                    CommandKind::BuildOrTest,
                    Some(1),
                    LifeState::default(),
                );
                if duplicate.is_none() {
                    duplicate = Some(event.clone());
                }
                apply_event(&mut memory, &event);
            }
        }
        assert_eq!(
            memory
                .days
                .iter()
                .map(|stats| stats.observations.len())
                .sum::<usize>(),
            MAX_OBSERVATIONS
        );
        let before: Vec<_> = memory
            .days
            .iter()
            .map(|stats| (stats.day, stats.repo.clone(), stats.build_failures))
            .collect();
        apply_event(&mut memory, duplicate.as_ref().unwrap());
        assert_eq!(
            memory
                .days
                .iter()
                .map(|stats| (stats.day, stats.repo.clone(), stats.build_failures))
                .collect::<Vec<_>>(),
            before
        );

        // Disk order is not semantic. A valid file may place its true oldest
        // observation later in the array; global compaction must still choose
        // that record, not whichever record happens to expose the smallest
        // first element.
        let mut reordered = memory.clone();
        {
            let stats = reordered.stats_mut(200, &repos[0]);
            for (index, observation) in stats.observations.iter_mut().enumerate() {
                observation.at_ms = if index == 0 { 1 } else { 1_000 + index as u64 };
            }
            replay_observations(stats, None);
            stats.observations.rotate_left(1);
        }
        {
            let stats = reordered.stats_mut(201, &repos[1]);
            for (index, observation) in stats.observations.iter_mut().enumerate() {
                observation.at_ms = 100 + index as u64;
            }
            replay_observations(stats, None);
        }
        reordered.validate().unwrap();
        let root = TestDir::new("unordered-global-compaction");
        let path = root.memory_path();
        let mut bytes = serde_json::to_vec_pretty(&reordered).unwrap();
        bytes.push(b'\n');
        jterm_core::snapshot_file::ensure_private_directory(path.parent().unwrap()).unwrap();
        jterm_core::snapshot_file::write_atomic_private(&path, &bytes).unwrap();
        let mut reordered = read_memory(&path).unwrap();
        assert_eq!(
            reordered.stats(200, &repos[0]).unwrap().observations[0].at_ms,
            1_001
        );
        apply_event(
            &mut reordered,
            &MemoryEvent::fixed(
                99_001,
                204,
                Some(&repos[4]),
                CommandKind::BuildOrTest,
                Some(1),
                LifeState::default(),
            ),
        );
        assert!(reordered
            .stats(200, &repos[0])
            .unwrap()
            .baseline
            .as_ref()
            .unwrap()
            .compacted_through
            .is_some());
        assert!(reordered
            .stats(201, &repos[1])
            .unwrap()
            .baseline
            .as_ref()
            .unwrap()
            .compacted_through
            .is_none());

        apply_event(
            &mut memory,
            &MemoryEvent::fixed(
                99_000,
                204,
                Some(&repos[4]),
                CommandKind::BuildOrTest,
                Some(1),
                LifeState::default(),
            ),
        );
        assert_eq!(memory.days.len(), 5);
        assert_eq!(
            memory
                .days
                .iter()
                .map(|stats| stats.build_failures)
                .sum::<u32>(),
            257
        );
        assert!(
            memory
                .days
                .iter()
                .map(|stats| stats.observations.len())
                .sum::<usize>()
                <= MAX_OBSERVATIONS
        );
        memory.validate().unwrap();
    }

    #[test]
    fn every_maximal_valid_schema_fits_the_pretty_json_budget() {
        let ids: Vec<_> = (0..MAX_RECENT_EVENT_IDS)
            .map(|index| format!("id{index:094}"))
            .collect();
        let mut memory = DiskMemory {
            recent_event_ids: ids.clone(),
            days_seen: MigratingCounter::new(u32::MAX),
            lifetime_recoveries: MigratingCounter::new(u32::MAX),
            growth_days: MigratingGrowthDayLedger::new(GrowthDayLedger {
                compacted_through: Some(i64::MIN),
                recent: (0..MAX_RECENT_GROWTH_DAYS)
                    .map(|record| i64::MIN + 1 + record as i64)
                    .collect(),
            }),
            ..DiskMemory::default()
        };
        for record in 0..MAX_DAILY_RECORDS {
            let suffix = format!("-{record}");
            let mut repo = String::from("/");
            while repo.len() + suffix.len() + 1 < MAX_REPO_BYTES {
                repo.push('"');
            }
            repo.push_str(&suffix);
            let observations: Vec<_> = (0..4)
                .map(|offset| Observation {
                    id: ids[record * 4 + offset].clone(),
                    at_ms: u64::MAX,
                    kind: ObservationKind::BuildFailure,
                })
                .collect();
            memory.days.push(DailyStats {
                day: i64::MIN + 1 + record as i64,
                repo,
                build_failures: u32::MAX,
                build_successes: u32::MAX,
                git_pushes: u32::MAX,
                open_failures: u32::MAX,
                open_failure_at_ms: Some(u64::MAX),
                last_recovery_duration_ms: Some(u64::MAX),
                recovered_pending_push: false,
                failure_success_flips: MigratingCounter::new(u32::MAX),
                baseline: Some(StatsBaseline {
                    build_failures: u32::MAX,
                    build_successes: u32::MAX,
                    git_pushes: u32::MAX,
                    open_failures: u32::MAX,
                    open_failure_at_ms: Some(u64::MAX),
                    last_recovery_duration_ms: Some(u64::MAX),
                    failure_success_flips: MigratingCounter::new(u32::MAX),
                    compacted_through: Some(ObservationCursor {
                        at_ms: u64::MAX,
                        id: format!("c{record:095}"),
                    }),
                    ..StatsBaseline::default()
                }),
                observations,
                // Widest valid encodings the duration aggregate can reach.
                build_duration_sum_ms: u64::from(u32::MAX) * MAX_TRACKED_BUILD_MS,
                build_duration_count: u32::MAX,
                activity_buckets: [u16::MAX; CIRCADIAN_BUCKET_COUNT],
            });
        }
        memory.validate().unwrap();
        let mut bytes = serde_json::to_vec_pretty(&memory).unwrap();
        bytes.push(b'\n');
        assert!(
            bytes.len() as u64 <= MAX_MEMORY_BYTES,
            "maximal valid schema encoded to {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn persisted_session_events_are_not_replayed_after_compaction() {
        let root = TestDir::new("session-ack");
        let path = root.memory_path();
        let repo = root.0.join("repo");
        let mut memory = OrganismMemory::load(path.clone()).unwrap();
        let count = MAX_OBSERVATIONS_PER_RECORD + 17;
        let events: Vec<_> = (0..count)
            .map(|index| {
                event(
                    30_000 + index as u64,
                    123,
                    &repo,
                    CommandKind::BuildOrTest,
                    Some(1),
                )
            })
            .collect();
        for event in &events {
            memory.apply_local(event);
        }
        transact_batch(&path, &events).unwrap();
        record_acknowledged_events(&path, &events);

        memory.refresh().unwrap();
        let stats = memory.memory().stats(123, repo.to_str().unwrap()).unwrap();
        assert_eq!(stats.build_failures, count as u32);
        assert_eq!(stats.open_failures, count as u32);
        assert!(memory.session_events.is_empty());

        // A second refresh is also idempotent after the folded observation ids
        // have disappeared from the retained tail.
        memory.refresh().unwrap();
        let stats = memory.memory().stats(123, repo.to_str().unwrap()).unwrap();
        assert_eq!(stats.build_failures, count as u32);
        assert_eq!(stats.open_failures, count as u32);
    }

    #[test]
    fn a_full_durable_queue_never_diverges_the_local_cache() {
        let root = TestDir::new("queue-full");
        let path = root.memory_path();
        let repo = root.0.join("repo");
        let mut queued = VecDeque::new();
        for index in 0..MAX_PENDING_MEMORY_EVENTS {
            queued.push_back(event(
                40_000 + index as u64,
                321,
                &repo,
                CommandKind::BuildOrTest,
                Some(1),
            ));
        }
        event_queues().lock().unwrap().insert(path.clone(), queued);

        let mut memory = OrganismMemory::load(path.clone()).unwrap();
        let rejected = event(50_000, 321, &repo, CommandKind::BuildOrTest, Some(1));
        let (preview, result, retained) = memory.apply_and_enqueue(rejected);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(!retained);
        assert_eq!(preview.open_failures, 1);
        assert_eq!(preview.current_work, RepoWorkState::new(1, false, 0));
        assert!(memory.memory().stats(321, repo.to_str().unwrap()).is_none());
        assert!(memory.session_events.is_empty());

        event_queues().lock().unwrap().remove(&path);
    }

    #[test]
    fn admitted_event_stays_retained_when_a_worker_wins_before_schedule_error() {
        let root = TestDir::new("queue-admission-race");
        let path = root.memory_path();
        let repo = root.0.join("repo");
        let admitted = event(60_000, 322, &repo, CommandKind::BuildOrTest, Some(1));
        let admitted_id = admitted.id.clone();

        let outcome = enqueue_event_with_scheduler(path.clone(), admitted, |scheduled_path| {
            // Taking the same mutex proves enqueue released it before calling
            // the scheduler. Removing the event models a running worker that
            // durably acknowledged it before scheduling reported an error.
            let mut queues = event_queues().lock().unwrap();
            let queue = queues.get_mut(&scheduled_path).unwrap();
            queue.retain(|event| event.id != admitted_id);
            if queue.is_empty() {
                queues.remove(&scheduled_path);
            }
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "synthetic scheduler saturation",
            ))
        });

        match outcome {
            EventEnqueue::Retained(Err(error)) => {
                assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            }
            _ => panic!("an admitted event must remain retained"),
        }
        assert!(!event_queues().lock().unwrap().contains_key(&path));

        let mut memory = OrganismMemory::load(path).unwrap();
        let retained_event = event(60_001, 322, &repo, CommandKind::BuildOrTest, Some(1));
        let (insight, result, retained) = memory.apply_enqueue_outcome(
            retained_event,
            EventEnqueue::Retained(Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "synthetic scheduling failure after admission",
            ))),
        );
        assert!(retained);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(insight.current_work, RepoWorkState::new(1, false, 0));
        assert_eq!(memory.session_events.len(), 1);
        assert_eq!(
            memory
                .memory()
                .stats(322, repo.to_str().unwrap())
                .unwrap()
                .open_failures,
            1
        );
    }

    #[test]
    fn life_snapshot_is_bounded_and_restored_without_floats_on_disk() {
        let state = LifeState {
            energy: -2.0,
            mood: 2.0,
            curiosity: f32::NAN,
            boredom: 0.1234,
            stress: 0.4,
            social_need: 0.5,
            attachment: 0.75,
            confidence: 1.0,
        };
        let snapshot = LifeSnapshot::from_state(state);
        assert!(snapshot.valid());
        let restored = snapshot.state();
        assert_eq!(restored.energy, 0.0);
        assert_eq!(restored.mood, 1.0);
        assert_eq!(restored.curiosity, 0.5);
        assert!((restored.boredom - 0.123).abs() < f32::EPSILON);
    }

    #[test]
    fn same_millisecond_life_updates_have_a_deterministic_event_cursor() {
        let mut lower = MemoryEvent::fixed(
            77_000,
            1,
            None,
            CommandKind::Other,
            Some(0),
            LifeState {
                energy: 0.1,
                ..LifeState::default()
            },
        );
        lower.id = "cursor-a".to_owned();
        let mut higher = MemoryEvent::fixed(
            77_000,
            1,
            None,
            CommandKind::Other,
            Some(0),
            LifeState {
                energy: 0.9,
                ..LifeState::default()
            },
        );
        higher.id = "cursor-b".to_owned();

        let mut forward = DiskMemory::default();
        apply_event(&mut forward, &lower);
        apply_event(&mut forward, &higher);
        let mut reverse = DiskMemory::default();
        apply_event(&mut reverse, &higher);
        apply_event(&mut reverse, &lower);

        assert_eq!(forward.life.energy, reverse.life.energy);
        assert_eq!(forward.life_updated_event_id, "cursor-b");
        assert_eq!(reverse.life_updated_event_id, "cursor-b");
        assert_eq!(forward.life.state().energy, 0.9);
    }

    #[test]
    fn concurrent_writer_helper() {
        let Some(path) = std::env::var_os("FORGE_ORGANISM_TEST_PATH") else {
            return;
        };
        let repo = PathBuf::from(std::env::var_os("FORGE_ORGANISM_TEST_REPO").unwrap());
        let count: u32 = std::env::var("FORGE_ORGANISM_TEST_COUNT")
            .unwrap()
            .parse()
            .unwrap();
        for index in 0..count {
            transact(
                Path::new(&path),
                &event(
                    1_000 + u64::from(index),
                    70_000,
                    &repo,
                    CommandKind::BuildOrTest,
                    Some(1),
                ),
            )
            .unwrap();
        }
    }

    #[test]
    fn concurrent_transactions_preserve_every_delta() {
        if std::env::var_os("FORGE_ORGANISM_TEST_PATH").is_some() {
            return;
        }
        let root = TestDir::new("concurrent");
        let path = root.memory_path();
        let repo = root.0.join("repo");
        let executable = std::env::current_exe().unwrap();
        let spawn = || {
            Command::new(&executable)
                .arg("--exact")
                .arg("organism_memory::tests::concurrent_writer_helper")
                .arg("--nocapture")
                .env("FORGE_ORGANISM_TEST_PATH", &path)
                .env("FORGE_ORGANISM_TEST_REPO", &repo)
                .env("FORGE_ORGANISM_TEST_COUNT", "20")
                .spawn()
                .unwrap()
        };
        let mut first = spawn();
        let mut second = spawn();
        assert!(first.wait().unwrap().success());
        assert!(second.wait().unwrap().success());

        let memory = read_memory(&path).unwrap();
        let stats = memory.stats(70_000, repo.to_str().unwrap()).unwrap();
        assert_eq!(stats.build_failures, 40);
        assert_eq!(stats.open_failures, 40);
        assert_eq!(memory.days_seen.value, 1);
        assert_eq!(memory.lifetime_recoveries.value, 0);
        assert_eq!(memory.growth_days.value.recent, [70_000]);
        assert_eq!(
            stats
                .activity_buckets
                .iter()
                .map(|count| u32::from(*count))
                .sum::<u32>(),
            40
        );
        assert_eq!(memory.days.len(), 1);
    }
}
