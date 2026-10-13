//! Native identity/clock adapter. All cadence and deadline policy is in core.
use std::time::Duration;

use jterm_core::organism::WatchRhythm;
use jterm_core::organism_watch::{
    WatchEvent, WatchObservation, WatchOutcome, WatchOutputDisposition, WatchSnapshot,
};

use crate::block_view::watch_batch::BatchDisposition;
use crate::block_view::watch_reader::{
    ReaderIdentity, ReaderSnapshot, ReaderWatchEvent, ReaderWatchFact,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity<O> {
    owner: O,
    reader: ReaderIdentity,
    observation: u64,
}

pub(super) struct WatchAdapter<O> {
    owner: O,
    core: WatchObservation<Identity<O>>,
    next_observation: Option<u64>,
    active: bool,
}

const NEUTRAL: WatchSnapshot = WatchSnapshot {
    rhythm: WatchRhythm::Steady,
    next_deadline: None,
};

impl<O: Clone + Eq> WatchAdapter<O> {
    pub fn new(owner: O) -> Self {
        Self {
            owner,
            core: WatchObservation::default(),
            next_observation: Some(0),
            active: false,
        }
    }

    pub fn retire(&mut self, now: Duration) {
        if self.active {
            if let Some(identity) = self.core.identity().cloned() {
                let _ = self.core.advance(now, &identity, WatchEvent::Retire);
            }
        }
        self.active = false;
    }

    fn observe(
        &mut self,
        now: Duration,
        reader: ReaderSnapshot,
        eligible: bool,
        trusted: bool,
    ) -> bool {
        let Some(reader_identity) = reader.identity.filter(|_| eligible && reader.available) else {
            self.retire(now);
            return false;
        };
        if self.active
            && self
                .core
                .identity()
                .is_some_and(|id| id.reader == reader_identity)
        {
            return true;
        }
        self.retire(now);
        let Some(observation) = self.next_observation else {
            return false;
        };
        self.next_observation = observation.checked_add(1);
        let identity = Identity {
            owner: self.owner.clone(),
            reader: reader_identity,
            observation,
        };
        self.core.set_identity(Some(identity.clone()));
        self.active = self
            .core
            .advance(
                now,
                &identity,
                if trusted {
                    WatchEvent::BeginTrustedCommand
                } else {
                    WatchEvent::AttachRunningObservation
                },
            )
            .is_ok();
        self.active
    }

    /// Reader callbacks are synchronous; a future queued transport must preserve
    /// this snapshot token. Reject obsolete reader facts before touching the core.
    pub fn event(
        &mut self,
        now: Duration,
        event: &ReaderWatchEvent,
        current: ReaderSnapshot,
        eligible: bool,
    ) -> bool {
        if event.snapshot != current {
            return false;
        }
        if !self.observe(
            now,
            current,
            eligible,
            matches!(event.fact, ReaderWatchFact::TrustedStart),
        ) {
            return false;
        }
        let identity = self
            .core
            .identity()
            .cloned()
            .expect("active observation has an identity");
        let fact = match &event.fact {
            ReaderWatchFact::Output(fact) => {
                if Some(fact.identity) != current.identity {
                    return false;
                }
                WatchEvent::OutputObserved {
                    batch_seq: fact.sequence,
                    disposition: match fact.disposition {
                        BatchDisposition::Accepted => WatchOutputDisposition::Accepted,
                        BatchDisposition::Quarantined => WatchOutputDisposition::Quarantined,
                    },
                }
            }
            _ => WatchEvent::Advance,
        };
        // In particular, do not Advance before Output: a stale batch must not
        // move the reducer's clock merely by carrying a newer timestamp.
        match self.core.advance(now, &identity, fact) {
            Ok(update) => matches!(
                update.outcome,
                WatchOutcome::Counted | WatchOutcome::BaselineOnly
            ),
            Err(_) => false,
        }
    }

    pub fn advance(
        &mut self,
        now: Duration,
        reader: ReaderSnapshot,
        eligible: bool,
    ) -> WatchSnapshot {
        if !self.observe(now, reader, eligible, false) {
            return NEUTRAL;
        }
        let identity = self
            .core
            .identity()
            .cloned()
            .expect("active observation has an identity");
        self.core
            .advance(now, &identity, WatchEvent::Advance)
            .map_or(NEUTRAL, |update| update.snapshot)
    }

    pub fn snapshot(&self, now: Duration) -> WatchSnapshot {
        if !self.active {
            return NEUTRAL;
        }
        self.core
            .identity()
            .and_then(|id| self.core.snapshot(now, id).ok())
            .unwrap_or(NEUTRAL)
    }
}

/// GLib timeout granularity is milliseconds. Never truncate a positive core
/// remainder to zero, or wake before an inclusive (+1ns) boundary and lose it.
/// A read-only due snapshot requests one cleanup turn on the existing source.
pub(super) fn frame_delay(base: Duration, snapshot: WatchSnapshot, now: Duration) -> Duration {
    let Some(deadline) = snapshot.next_deadline else {
        return base;
    };
    let remaining = deadline.at().saturating_sub(now);
    let millis = remaining
        .as_millis()
        .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0));
    base.min(Duration::from_millis(
        u64::try_from(millis).unwrap_or(u64::MAX).max(1),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_view::watch_reader::WatchReader;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Instant;

    fn rig() -> (WatchReader, Rc<RefCell<WatchAdapter<u64>>>, Instant) {
        let reader = WatchReader::default();
        let adapter = Rc::new(RefCell::new(WatchAdapter::new(7)));
        let target = adapter.clone();
        let epoch = Instant::now();
        reader.connect(move |event| {
            target.borrow_mut().event(
                event.at.saturating_duration_since(epoch),
                event,
                event.snapshot,
                true,
            );
        });
        (reader, adapter, epoch)
    }
    fn batch(reader: &WatchReader, now: Instant, start: bool, stretches: usize) {
        reader.begin_dispatch(reader.fresh_origin(now), now);
        if start {
            reader.command_started(now);
        }
        for _ in 0..stretches {
            reader.note_output();
        }
        reader.end_dispatch(now);
    }
    #[test]
    fn actual_reader_facts_reach_core_once_and_preserve_inclusive_busy_boundary() {
        let (reader, adapter, epoch) = rig();
        batch(&reader, epoch, true, 3);
        assert_eq!(
            adapter.borrow().snapshot(Duration::ZERO).rhythm,
            WatchRhythm::Steady
        );
        batch(&reader, epoch + Duration::from_millis(600), false, 2);
        batch(&reader, epoch + Duration::from_millis(1200), false, 3);
        let busy = adapter.borrow().snapshot(Duration::from_millis(1200));
        assert_eq!(busy.rhythm, WatchRhythm::Busy);
        assert_eq!(
            frame_delay(
                Duration::from_millis(100),
                busy,
                Duration::from_millis(1200)
            ),
            Duration::from_millis(1)
        );
        let steady =
            adapter
                .borrow_mut()
                .advance(Duration::from_millis(1201), reader.snapshot(), true);
        assert_eq!(steady.rhythm, WatchRhythm::Steady);
        assert!(steady.next_deadline.unwrap().at() > Duration::from_millis(1201));
    }
    #[test]
    fn hold_recovery_and_replayed_start_attach_without_resumed_or_busy_replay() {
        let (reader, adapter, epoch) = rig();
        batch(&reader, epoch, true, 1);
        reader.set_held(true, epoch + Duration::from_secs(4));
        assert_eq!(
            adapter
                .borrow()
                .snapshot(Duration::from_secs(4))
                .next_deadline,
            None
        );
        reader.begin_dispatch(
            Some(crate::block_view::watch_batch::ReaderBatchOrigin::BufferedReplay),
            epoch + Duration::from_secs(5),
        );
        reader.command_finished(epoch + Duration::from_secs(5));
        reader.command_started(epoch + Duration::from_secs(5));
        for _ in 0..3 {
            reader.note_output();
        }
        reader.end_dispatch(epoch + Duration::from_secs(5));
        reader.set_held(false, epoch + Duration::from_secs(5));
        batch(&reader, epoch + Duration::from_secs(10), false, 3);
        assert_eq!(
            adapter.borrow().snapshot(Duration::from_secs(10)).rhythm,
            WatchRhythm::Steady
        );
    }
    #[test]
    fn stale_reader_event_and_duplicate_batch_cannot_advance_mutable_clock() {
        let (reader, adapter, epoch) = rig();
        let captured = Rc::new(RefCell::new(Vec::new()));
        let log = captured.clone();
        reader.connect(move |event| log.borrow_mut().push(event.clone()));
        batch(&reader, epoch, true, 1);
        let old = captured.borrow().last().unwrap().clone();
        let before = adapter.borrow().core.clone();
        adapter
            .borrow_mut()
            .event(Duration::from_secs(900), &old, reader.snapshot(), true);
        assert_eq!(
            adapter.borrow().core,
            before,
            "duplicate sequence rejected before clock"
        );
        reader.set_held(true, epoch);
        reader.set_held(false, epoch);
        let before = adapter.borrow().core.clone();
        adapter
            .borrow_mut()
            .event(Duration::from_secs(900), &old, reader.snapshot(), true);
        assert_eq!(
            adapter.borrow().core,
            before,
            "old reader context rejected before clock"
        );
    }
    #[test]
    fn reader_reset_retires_rhythm_and_deadline_without_a_late_revival() {
        let (reader, adapter, epoch) = rig();
        batch(&reader, epoch, true, 1);
        assert!(adapter
            .borrow()
            .snapshot(Duration::ZERO)
            .next_deadline
            .is_some());
        reader.reset(epoch + Duration::from_secs(1));
        assert_eq!(adapter.borrow().snapshot(Duration::from_secs(1)), NEUTRAL);
        assert_eq!(adapter.borrow().snapshot(Duration::from_secs(90)), NEUTRAL);
        assert_eq!(
            adapter
                .borrow_mut()
                .advance(Duration::from_secs(90), reader.snapshot(), true),
            NEUTRAL,
            "a later existing frame cannot restore retired RIS evidence or a deadline"
        );
    }

    #[test]
    fn due_snapshot_is_not_lost_and_mutable_cleanup_stops_due_polling() {
        let (reader, adapter, epoch) = rig();
        batch(&reader, epoch, true, 0);
        let due = adapter.borrow().snapshot(Duration::from_secs(3));
        assert_eq!(due.rhythm, WatchRhythm::Waiting);
        assert_eq!(
            frame_delay(Duration::from_secs(1), due, Duration::from_secs(3)),
            Duration::from_millis(1)
        );
        let consumed =
            adapter
                .borrow_mut()
                .advance(Duration::from_secs(3), reader.snapshot(), true);
        assert_eq!(consumed.next_deadline, None);
        assert_eq!(
            frame_delay(Duration::from_secs(1), consumed, Duration::from_secs(3)),
            Duration::from_secs(1)
        );
    }
    #[test]
    fn pane_clock_survives_context_replacement_and_local_context_never_wraps() {
        let (reader, adapter, epoch) = rig();
        batch(&reader, epoch, true, 1);
        adapter.borrow_mut().retire(Duration::from_secs(30));
        let backward =
            adapter
                .borrow_mut()
                .advance(Duration::from_secs(20), reader.snapshot(), true);
        assert_eq!(backward, NEUTRAL);
        adapter.borrow_mut().next_observation = Some(u64::MAX);
        adapter
            .borrow_mut()
            .advance(Duration::from_secs(30), reader.snapshot(), true);
        adapter.borrow_mut().retire(Duration::from_secs(31));
        assert_eq!(
            adapter
                .borrow_mut()
                .advance(Duration::from_secs(32), reader.snapshot(), true),
            NEUTRAL
        );
    }
    #[test]
    fn quarantined_and_stale_batches_never_accelerate_the_native_frame() {
        let reader = WatchReader::default();
        let mut adapter = WatchAdapter::new(1_u64);
        let events = Rc::new(RefCell::new(Vec::new()));
        let log = events.clone();
        reader.connect(move |event| log.borrow_mut().push(event.clone()));
        let now = Instant::now();
        batch(&reader, now, true, 1);
        let start = events.borrow()[0].clone();
        assert!(!adapter.event(Duration::ZERO, &start, start.snapshot, true));
        let output = events.borrow()[1].clone();
        assert!(adapter.event(Duration::ZERO, &output, output.snapshot, true));
        assert!(!adapter.event(Duration::from_secs(1), &output, output.snapshot, true));
        reader.begin_dispatch(reader.fresh_origin(now), now);
        reader.note_output();
        reader.command_finished(now);
        reader.command_started(now);
        reader.end_dispatch(now);
        let output = events.borrow().last().unwrap().clone();
        assert!(!adapter.event(Duration::from_secs(2), &output, output.snapshot, true));
    }
}
