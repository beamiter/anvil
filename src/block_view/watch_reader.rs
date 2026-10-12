//! Content-free facts from the accepted reader lifecycle. No rhythm policy.
//! One sequence belongs to a real read, even when that read is parked or split.
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use super::watch_batch::{BatchFact, ReadBatchSequence, ReaderBatchOrigin, WatchBatch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReaderIdentity {
    command: u64,
    context: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReaderSnapshot {
    pub revision: u64,
    pub identity: Option<ReaderIdentity>,
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReaderWatchFact {
    Reconcile,
    TrustedStart,
    Output(BatchFact<ReaderIdentity>),
}

#[derive(Debug, Clone)]
pub(crate) struct ReaderWatchEvent {
    pub snapshot: ReaderSnapshot,
    pub at: Instant,
    pub fact: ReaderWatchFact,
}

type Listener = Rc<dyn Fn(&ReaderWatchEvent)>;

#[derive(Default)]
struct State {
    revision: u64,
    command: Option<u64>,
    held: bool,
    replay: bool,
    alt: bool,
    ended: bool,
    exhausted: bool,
    sequence: ReadBatchSequence,
    batch: Option<WatchBatch<ReaderIdentity>>,
    fresh_dispatch: bool,
}

impl State {
    fn snapshot(&self) -> ReaderSnapshot {
        ReaderSnapshot {
            revision: self.revision,
            identity: self.command.map(|command| ReaderIdentity {
                command,
                context: self.revision,
            }),
            available: !self.held && !self.replay && !self.alt && !self.ended && !self.exhausted,
        }
    }

    fn invalidate(&mut self) {
        if let Some(next) = self.revision.checked_add(1) {
            self.revision = next;
        } else {
            self.exhausted = true;
        }
    }
}

#[derive(Default)]
pub(crate) struct WatchReader {
    state: RefCell<State>,
    listeners: RefCell<Vec<Listener>>,
}

impl WatchReader {
    pub fn snapshot(&self) -> ReaderSnapshot {
        self.state.borrow().snapshot()
    }

    pub fn connect(&self, listener: impl Fn(&ReaderWatchEvent) + 'static) {
        self.listeners.borrow_mut().push(Rc::new(listener));
    }

    fn emit(&self, fact: ReaderWatchFact, at: Instant) {
        // No state borrow crosses fanout: the presentation adapter queries us.
        let event = ReaderWatchEvent {
            snapshot: self.snapshot(),
            at,
            fact,
        };
        let listeners = self.listeners.borrow().clone();
        for listener in listeners {
            listener(&event);
        }
    }

    /// Called before SelectionFeedHold::try_buffer, once per nonempty real read.
    /// Exhaustion disables only Watch; terminal parsing must continue normally.
    pub fn fresh_origin(&self, at: Instant) -> Option<ReaderBatchOrigin> {
        let result = {
            let mut state = self.state.borrow_mut();
            if state.exhausted || state.ended {
                return None;
            }
            match state.sequence.issue() {
                Some(sequence) => Some(ReaderBatchOrigin::Fresh { sequence }),
                None => {
                    state.exhausted = true;
                    state.invalidate();
                    None
                }
            }
        };
        if result.is_none() {
            self.emit(ReaderWatchFact::Reconcile, at);
        }
        result
    }

    /// Brackets the entire splitter/parser dispatch, never an individual event.
    pub fn begin_dispatch(&self, origin: Option<ReaderBatchOrigin>, at: Instant) {
        {
            let mut state = self.state.borrow_mut();
            debug_assert!(state.batch.is_none());
            state.fresh_dispatch = matches!(origin, Some(ReaderBatchOrigin::Fresh { .. }));
            state.replay = origin == Some(ReaderBatchOrigin::BufferedReplay);
            state.batch = origin.map(WatchBatch::new);
        }
        if origin == Some(ReaderBatchOrigin::BufferedReplay) {
            self.emit(ReaderWatchFact::Reconcile, at);
        }
    }

    pub fn end_dispatch(&self, at: Instant) {
        let (fact, replay) = {
            let mut state = self.state.borrow_mut();
            let identity = state.snapshot().identity;
            let fact = state
                .batch
                .take()
                .and_then(|batch| batch.finish(identity.as_ref()));
            let replay = state.replay;
            state.fresh_dispatch = false;
            state.replay = false;
            (fact, replay)
        };
        if let Some(fact) = fact {
            self.emit(ReaderWatchFact::Output(fact), at);
        } else if replay {
            self.emit(ReaderWatchFact::Reconcile, at);
        }
    }

    /// Only the existing reader's nonempty output path calls this. OSC controls
    /// with no accepted Bytes stretch do not manufacture activity.
    pub fn note_output(&self) {
        let mut state = self.state.borrow_mut();
        let snapshot = state.snapshot();
        let identity = snapshot.available.then_some(snapshot.identity).flatten();
        if let Some(batch) = &mut state.batch {
            batch.note_output(identity.as_ref());
        }
    }

    /// Invoked only after the reader accepted C and notified existing observers.
    /// Replayed/unscoped C updates facts, but never becomes a new Watch start.
    pub fn command_started(&self, at: Instant) {
        let trusted = {
            let mut state = self.state.borrow_mut();
            state.invalidate();
            state.command = (!state.exhausted).then_some(state.revision);
            state.fresh_dispatch && state.snapshot().available
        };
        self.emit(
            if trusted {
                ReaderWatchFact::TrustedStart
            } else {
                ReaderWatchFact::Reconcile
            },
            at,
        );
    }

    /// Called before existing finish observers, including trusted-A recovery.
    pub fn command_finished(&self, at: Instant) {
        {
            let mut state = self.state.borrow_mut();
            if state.command.is_none() {
                return;
            }
            state.invalidate();
            state.command = None;
        }
        self.emit(ReaderWatchFact::Reconcile, at);
    }

    pub fn set_held(&self, held: bool, at: Instant) {
        {
            let mut state = self.state.borrow_mut();
            if state.held == held {
                return;
            }
            state.held = held;
            state.invalidate();
        }
        self.emit(ReaderWatchFact::Reconcile, at);
    }

    pub fn set_alt(&self, alt: bool, at: Instant) {
        {
            let mut state = self.state.borrow_mut();
            if state.alt == alt {
                return;
            }
            state.alt = alt;
            state.invalidate();
        }
        self.emit(ReaderWatchFact::Reconcile, at);
    }

    pub fn reset(&self, at: Instant) {
        {
            let mut state = self.state.borrow_mut();
            state.invalidate();
            state.command = None;
            state.alt = false;
        }
        self.emit(ReaderWatchFact::Reconcile, at);
    }

    pub fn source_ended(&self, at: Instant) {
        {
            let mut state = self.state.borrow_mut();
            state.invalidate();
            state.command = None;
            state.ended = true;
        }
        self.emit(ReaderWatchFact::Reconcile, at);
    }
}

#[cfg(test)]
mod tests {
    use super::super::watch_batch::BatchDisposition;
    use super::*;
    use std::rc::Rc;
    use std::time::Duration;

    fn rig() -> (WatchReader, Rc<RefCell<Vec<ReaderWatchEvent>>>, Instant) {
        let reader = WatchReader::default();
        let events = Rc::new(RefCell::new(Vec::new()));
        let log = events.clone();
        reader.connect(move |event| log.borrow_mut().push(event.clone()));
        (reader, events, Instant::now())
    }
    fn fresh(reader: &WatchReader, now: Instant) {
        reader.begin_dispatch(reader.fresh_origin(now), now);
    }

    #[test]
    fn three_parser_stretches_are_one_real_read_fact() {
        let (reader, events, now) = rig();
        fresh(&reader, now);
        reader.command_started(now);
        for _ in 0..3 {
            reader.note_output();
        }
        reader.end_dispatch(now);
        let events = events.borrow();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].fact, ReaderWatchFact::TrustedStart));
        assert!(matches!(
            events[1].fact,
            ReaderWatchFact::Output(BatchFact {
                sequence: 0,
                disposition: BatchDisposition::Accepted,
                ..
            })
        ));
    }

    #[test]
    fn finish_then_new_command_cannot_borrow_old_output() {
        let (reader, events, now) = rig();
        fresh(&reader, now);
        reader.command_started(now);
        let old = reader.snapshot();
        reader.note_output();
        reader.command_finished(now);
        assert_eq!(reader.snapshot().identity, None);
        reader.command_started(now);
        reader.end_dispatch(now);
        assert_ne!(old.identity, reader.snapshot().identity);
        let events = events.borrow();
        assert!(matches!(
            events.last().unwrap().fact,
            ReaderWatchFact::Output(BatchFact {
                disposition: BatchDisposition::Quarantined,
                ..
            })
        ));
    }

    #[test]
    fn replay_lifecycle_never_emits_trusted_start_or_output_and_finished_stays_finished() {
        let (reader, events, now) = rig();
        reader.set_held(true, now);
        let held = reader.snapshot();
        let _parked_sequence = reader.fresh_origin(now);
        reader.begin_dispatch(Some(ReaderBatchOrigin::BufferedReplay), now);
        reader.command_started(now);
        reader.note_output();
        reader.command_finished(now);
        reader.end_dispatch(now);
        reader.set_held(false, now);
        assert_ne!(held.revision, reader.snapshot().revision);
        assert!(reader.snapshot().available);
        assert_eq!(reader.snapshot().identity, None);
        assert!(events
            .borrow()
            .iter()
            .all(|e| e.fact == ReaderWatchFact::Reconcile));
    }

    #[test]
    fn empty_hold_and_alt_change_context_without_new_command() {
        let (reader, _, now) = rig();
        fresh(&reader, now);
        reader.command_started(now);
        reader.end_dispatch(now);
        let original = reader.snapshot().identity.unwrap();
        reader.set_held(true, now);
        assert!(!reader.snapshot().available);
        reader.set_held(false, now);
        let restored = reader.snapshot().identity.unwrap();
        assert_eq!(original.command, restored.command);
        assert_ne!(original.context, restored.context);
        let stable = reader.snapshot();
        reader.set_held(false, now);
        assert_eq!(reader.snapshot(), stable);
        reader.set_alt(true, now);
        assert!(!reader.snapshot().available);
        reader.set_alt(false, now);
        assert_ne!(reader.snapshot(), stable);
    }

    #[test]
    fn unscoped_start_is_attach_fact_and_control_only_read_has_no_pulse() {
        let (reader, events, now) = rig();
        reader.command_started(now);
        fresh(&reader, now);
        reader.end_dispatch(now);
        assert_eq!(events.borrow().len(), 1);
        assert_eq!(events.borrow()[0].fact, ReaderWatchFact::Reconcile);
    }

    #[test]
    fn context_and_read_exhaustion_fail_closed_without_identity_wrap() {
        let (reader, events, now) = rig();
        reader.state.borrow_mut().revision = u64::MAX;
        reader.command_started(now);
        assert!(!reader.snapshot().available);
        assert_eq!(reader.snapshot().identity, None);
        assert_eq!(reader.fresh_origin(now), None);
        assert!(events
            .borrow()
            .iter()
            .all(|e| e.fact == ReaderWatchFact::Reconcile));
    }

    #[test]
    fn reset_and_exit_retire_before_observer_fanout() {
        let (reader, events, now) = rig();
        reader.command_started(now);
        reader.reset(now + Duration::from_secs(1));
        assert_eq!(events.borrow().last().unwrap().snapshot.identity, None);
        reader.command_started(now);
        reader.source_ended(now);
        assert!(!reader.snapshot().available);
        assert_eq!(reader.fresh_origin(now), None);
    }
    #[test]
    fn final_batch_sequence_is_issued_once_then_native_source_stays_closed() {
        let (reader, events, now) = rig();
        reader.state.borrow_mut().sequence = ReadBatchSequence::at_max_for_test();
        assert_eq!(
            reader.fresh_origin(now),
            Some(ReaderBatchOrigin::Fresh { sequence: u64::MAX })
        );
        assert_eq!(reader.fresh_origin(now), None);
        assert!(!reader.snapshot().available);
        reader.reset(now);
        reader.command_started(now);
        assert_eq!(reader.fresh_origin(now), None);
        assert!(events
            .borrow()
            .iter()
            .all(|event| event.fact == ReaderWatchFact::Reconcile));
    }

    #[test]
    fn listeners_can_register_during_fanout_without_refcell_reentry() {
        let reader = Rc::new(WatchReader::default());
        let weak = Rc::downgrade(&reader);
        reader.connect(move |_| {
            weak.upgrade().unwrap().connect(|_| {});
        });
        reader.command_started(Instant::now());
        reader.command_finished(Instant::now());
    }
}
