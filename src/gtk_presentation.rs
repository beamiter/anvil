//! Immutable after-paint identity for one native GTK frame. A later draw must
//! not replace the body submission which an earlier completed timing proves.
//! Construct only after painting; prepare only after actual complete GDK timing
//! with a nonzero presentation time, never from elapsed time or visibility.
use companion_host::{HostGate, Token};

/// A sequence local to one GUI, independent of any native clock's counter.
/// A stamp is created before painting and can only be captured after painting
/// the same native frame. Clock/surface replacement invalidates older stamps.
pub(crate) struct FrameSequence<C> {
    clock: Option<C>,
    generation: u64,
    serial: u64,
    current: Option<FrameStamp>,
    exhausted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameStamp {
    frame: i64,
    serial: u64,
    generation: u64,
}

impl<C> Default for FrameSequence<C> {
    fn default() -> Self {
        Self {
            clock: None,
            generation: 0,
            serial: 0,
            current: None,
            exhausted: false,
        }
    }
}

impl<C: PartialEq> FrameSequence<C> {
    pub(crate) fn select(&mut self, clock: Option<C>) -> bool {
        if self.clock == clock {
            return false;
        }
        self.current = None;
        self.clock = clock;
        self.advance_generation();
        true
    }

    fn advance_generation(&mut self) {
        match self.generation.checked_add(1) {
            Some(next) => self.generation = next,
            None => self.exhausted = true,
        }
    }

    pub(crate) fn begin(&mut self, clock: &C, frame: i64) -> Option<FrameStamp> {
        if self.exhausted || frame < 0 || self.clock.as_ref() != Some(clock) {
            return None;
        }
        if let Some(current) = self.current {
            if frame == current.frame {
                return Some(current);
            }
            if frame < current.frame {
                self.advance_generation();
            }
        }
        if self.exhausted {
            return None;
        }
        let Some(serial) = self.serial.checked_add(1) else {
            self.exhausted = true;
            self.current = None;
            return None;
        };
        self.serial = serial;
        let stamp = FrameStamp {
            frame,
            serial,
            generation: self.generation,
        };
        self.current = Some(stamp);
        Some(stamp)
    }

    pub(crate) fn after_paint(&self, clock: &C, frame: i64) -> Option<FrameStamp> {
        self.current
            .filter(|stamp| stamp.frame == frame && self.accepts(clock, *stamp))
    }

    pub(crate) fn accepts(&self, clock: &C, stamp: FrameStamp) -> bool {
        !self.exhausted
            && self.clock.as_ref() == Some(clock)
            && stamp.generation == self.generation
            && stamp.serial > 0
            && stamp.serial <= self.serial
    }
}

impl FrameStamp {
    pub(crate) fn serial(self) -> u64 {
        self.serial
    }
    pub(crate) fn generation(self) -> u64 {
        self.generation
    }
}

/// A new native surface cannot clear a body previously painted elsewhere.
/// Retain the original surface until the coordinator accepts its retirement.
pub(crate) struct SurfaceLease<S> {
    owner: Option<(Token, S)>,
}

impl<S> Default for SurfaceLease<S> {
    fn default() -> Self {
        Self { owner: None }
    }
}

impl<S: PartialEq> SurfaceLease<S> {
    pub(crate) fn refresh(&mut self, gate: &HostGate) {
        if self
            .owner
            .as_ref()
            .is_some_and(|(token, _)| gate.retirement_accepted(token))
        {
            self.owner = None;
        }
    }

    pub(crate) fn can_paint(&self, token: &Token, surface: &S) -> bool {
        self.owner
            .as_ref()
            .is_none_or(|(old, origin)| old == token && origin == surface)
    }

    pub(crate) fn record_body(&mut self, token: Token, surface: S) -> bool {
        if !self.can_paint(&token, &surface) {
            return false;
        }
        self.owner = Some((token, surface));
        true
    }

    pub(crate) fn can_clear(&self, token: &Token, surface: &S) -> bool {
        self.can_paint(token, surface)
    }
}

/// Keep the oldest outstanding evidence, rather than continually replacing it
/// with a newer paint. The GTK caller stores a strong originating frame-clock
/// reference as C, so equal counter values on different clocks cannot alias.
pub(crate) struct PendingFrames<C> {
    frames: Vec<(C, CapturedFrame)>,
}

impl<C> Default for PendingFrames<C> {
    fn default() -> Self {
        Self { frames: Vec::new() }
    }
}

impl<C: PartialEq> PendingFrames<C> {
    pub(crate) fn record(&mut self, clock: C, frame: CapturedFrame) -> bool {
        if self.frames.len() >= 8
            || self
                .frames
                .iter()
                .any(|(old_clock, old)| old_clock == &clock && old == &frame)
        {
            return false;
        }
        self.frames.push((clock, frame));
        true
    }

    pub(crate) fn take(&mut self) -> Vec<(C, CapturedFrame)> {
        std::mem::take(&mut self.frames)
    }

    pub(crate) fn clear(&mut self) {
        self.frames.clear();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Content {
    Body(Token),
    Cleared { token: Token, after: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapturedFrame {
    frame: i64,
    serial: u64,
    generation: u64,
    connection: u64,
    admission: u64,
    content: Content,
}

impl CapturedFrame {
    pub(crate) fn capture(stamp: FrameStamp, body_mapped: bool, gate: &HostGate) -> Option<Self> {
        let content = if body_mapped {
            if !gate.body_presentation_pending() {
                return None;
            }
            Content::Body(gate.token()?.clone())
        } else {
            let (token, after) = gate.pending_retirement_boundary()?;
            if stamp.serial <= after {
                return None;
            }
            Content::Cleared {
                token: token.clone(),
                after,
            }
        };
        Some(Self {
            frame: stamp.frame,
            serial: stamp.serial,
            generation: stamp.generation,
            connection: gate.connection_generation(),
            admission: gate.admission_generation(),
            content,
        })
    }

    pub(crate) fn frame(&self) -> i64 {
        self.frame
    }
    pub(crate) fn serial(&self) -> u64 {
        self.serial
    }
    pub(crate) fn stamp(&self) -> FrameStamp {
        FrameStamp {
            frame: self.frame,
            serial: self.serial,
            generation: self.generation,
        }
    }
    pub(crate) fn is_retirement(&self) -> bool {
        matches!(self.content, Content::Cleared { .. })
    }

    pub(crate) fn is_current(&self, gate: &HostGate) -> bool {
        if self.connection != gate.connection_generation()
            || self.admission != gate.admission_generation()
        {
            return false;
        }
        match &self.content {
            Content::Body(token) => {
                gate.body_presentation_pending() && gate.accepts_callback(token)
            }
            Content::Cleared { token, after } => {
                gate.pending_retirement_boundary() == Some((token, *after))
            }
        }
    }

    /// Bind the exact captured body serial immediately before presenting its
    /// native receipt. Both calls run in the same GTK-state borrow; no newer
    /// after-paint can overwrite this serial between them.
    pub(crate) fn prepare(&self, gate: &mut HostGate) -> bool {
        if !self.is_current(gate) {
            return false;
        }
        if matches!(self.content, Content::Body(_)) {
            gate.body_submitted(self.serial());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use companion_host::{Host, Reply, Request, RetirementCapability};

    fn stamp(frame: i64) -> FrameStamp {
        FrameStamp {
            frame,
            serial: frame as u64 + 1,
            generation: 1,
        }
    }
    fn token() -> Token {
        Token {
            companion_id: "eva".into(),
            host_id: "host".into(),
            incarnation: 1,
            surface_id: "surface".into(),
            boot_id: "boot".into(),
            epoch: 1,
        }
    }
    fn active() -> HostGate {
        let mut gate = HostGate::new(Host {
            id: "host".into(),
            incarnation: 1,
            surface_id: "surface".into(),
            retirement: RetirementCapability::NativePresented,
        });
        gate.receive(Reply::Accepted {
            boot_id: "boot".into(),
            companion_id: "eva".into(),
            session_id: "session".into(),
        });
        gate.receive(Reply::Grant {
            token: token(),
            revision: 1,
        });
        gate
    }
    #[test]
    fn completed_body_frame_is_not_starved_by_a_newer_draw() {
        let mut gate = active();
        let first = CapturedFrame::capture(stamp(1), true, &gate).unwrap();
        let newer = CapturedFrame::capture(stamp(2), true, &gate).unwrap();
        assert!(
            gate.frame_presented(first.serial()).is_none(),
            "capture is not proof"
        );
        assert!(first.prepare(&mut gate));
        assert!(matches!(
            gate.frame_presented(first.serial()),
            Some(Request::Presented { .. })
        ));
        assert!(
            !newer.prepare(&mut gate),
            "the token needs only one body receipt"
        );
        assert!(CapturedFrame::capture(stamp(3), true, &gate).is_none());
    }
    #[test]
    fn old_body_cannot_prove_clearance_or_a_replacement_token() {
        let mut gate = active();
        // A numerical serial alone must not turn a body frame into clearance,
        // even when it appears newer than the recorded revocation boundary.
        let old = CapturedFrame::capture(stamp(40), true, &gate).unwrap();
        gate.begin_frame(6);
        gate.receive(Reply::Revoke { token: token() });
        assert!(!old.prepare(&mut gate));
        assert!(
            CapturedFrame::capture(stamp(7), true, &gate).is_none(),
            "mapped body is not cleared"
        );
        assert!(
            CapturedFrame::capture(stamp(5), false, &gate).is_none(),
            "pre-boundary frame is not cleared"
        );
        let cleared = CapturedFrame::capture(stamp(7), false, &gate).unwrap();
        assert!(cleared.prepare(&mut gate));
        assert!(matches!(
            gate.frame_presented(cleared.serial()),
            Some(Request::Retired { .. })
        ));
        let mut next = token();
        next.epoch = 2;
        gate.receive(Reply::Grant {
            token: next,
            revision: 2,
        });
        assert!(!old.prepare(&mut gate));
        assert!(!cleared.prepare(&mut gate));
        let current = CapturedFrame::capture(stamp(8), true, &gate).unwrap();
        assert!(current.prepare(&mut gate));
        assert!(matches!(
            gate.frame_presented(current.serial()),
            Some(Request::Presented { .. })
        ));
    }
    #[test]
    fn connection_change_invalidates_a_captured_native_frame() {
        let mut gate = active();
        let old = CapturedFrame::capture(stamp(4), true, &gate).unwrap();
        gate.receive(Reply::Accepted {
            boot_id: "boot".into(),
            companion_id: "eva".into(),
            session_id: "session".into(),
        });
        assert!(!old.prepare(&mut gate));
        let mut sequence = FrameSequence::default();
        sequence.select(Some(1));
        assert!(sequence.begin(&1, -1).is_none());
        assert!(CapturedFrame::capture(stamp(5), false, &gate).is_none());
    }

    #[test]
    fn bounded_queue_keeps_an_older_completing_frame_and_its_clock() {
        let mut gate = active();
        let mut queue = PendingFrames::default();
        for frame in 1..=100 {
            queue.record(
                7,
                CapturedFrame::capture(stamp(frame), true, &gate).unwrap(),
            );
        }
        let frames = queue.take();
        assert_eq!(frames.len(), 8);
        assert_eq!(frames.first().unwrap().1.frame(), 1);
        assert_eq!(frames.last().unwrap().1.frame(), 8);
        assert!(frames[0].1.prepare(&mut gate));
        assert!(matches!(
            gate.frame_presented(frames[0].1.serial()),
            Some(Request::Presented { .. })
        ));

        let gate = active();
        let frame = CapturedFrame::capture(stamp(1), true, &gate).unwrap();
        assert!(queue.record(7, frame.clone()));
        assert!(!queue.record(7, frame.clone()));
        assert!(queue.record(8, frame));
        assert_eq!(
            queue.take().len(),
            2,
            "different originating clocks are distinct"
        );
    }

    #[test]
    fn new_clock_counter_cannot_reset_the_host_retirement_boundary() {
        let mut gate = active();
        let mut sequence = FrameSequence::default();
        sequence.select(Some(1));
        let old = sequence.begin(&1, 10_000).unwrap();
        gate.begin_frame(old.serial());
        let drawn = CapturedFrame::capture(old, true, &gate).unwrap();
        assert!(drawn.prepare(&mut gate));
        assert!(matches!(
            gate.frame_presented(drawn.serial()),
            Some(Request::Presented { .. })
        ));
        gate.receive(Reply::Revoke { token: token() });
        sequence.select(Some(2));
        assert!(!sequence.accepts(&1, old));
        assert!(
            sequence.after_paint(&2, 0).is_none(),
            "no before-paint, no captured native frame"
        );
        let fresh = sequence.begin(&2, 0).unwrap();
        assert!(fresh.serial() > old.serial());
        assert_eq!(sequence.after_paint(&2, 0), Some(fresh));
        gate.begin_frame(fresh.serial());
        let clear = CapturedFrame::capture(fresh, false, &gate).unwrap();
        assert!(clear.prepare(&mut gate));
        assert!(matches!(
            gate.frame_presented(clear.serial()),
            Some(Request::Retired { .. })
        ));
    }

    #[test]
    fn duplicate_begin_does_not_advance_and_revoke_in_same_frame_waits() {
        let mut gate = active();
        let mut sequence = FrameSequence::default();
        sequence.select(Some(1));
        let frame = sequence.begin(&1, 100).unwrap();
        assert_eq!(sequence.begin(&1, 100), Some(frame));
        gate.begin_frame(frame.serial());
        gate.receive(Reply::Revoke { token: token() });
        assert!(CapturedFrame::capture(frame, false, &gate).is_none());
        let next = sequence.begin(&1, 200).unwrap();
        assert_eq!(next.serial(), frame.serial() + 1);
        assert!(CapturedFrame::capture(next, false, &gate).is_some());
    }

    #[test]
    fn unmap_and_counter_rollback_invalidate_old_timings() {
        let mut sequence = FrameSequence::default();
        sequence.select(Some(7));
        let old = sequence.begin(&7, 20).unwrap();
        assert!(!sequence.select(Some(7)));
        assert!(sequence.accepts(&7, old));
        let reset = sequence.begin(&7, 0).unwrap();
        assert!(reset.serial() > old.serial());
        assert!(reset.generation() > old.generation());
        assert!(!sequence.accepts(&7, old));
        sequence.select(None);
        assert!(!sequence.accepts(&7, reset));
        assert!(sequence.begin(&7, 1).is_none());
        sequence.select(Some(7));
        assert!(sequence.after_paint(&7, 0).is_none());
        assert!(sequence.begin(&8, 0).is_none());
        assert!(sequence.begin(&7, -1).is_none());
        assert!(sequence.begin(&7, 0).unwrap().serial() > reset.serial());
    }

    #[test]
    fn sequence_exhaustion_fails_closed_without_reusing_a_stamp() {
        let mut sequence = FrameSequence::default();
        sequence.select(Some(1));
        let old = sequence.begin(&1, 0).unwrap();
        sequence.serial = u64::MAX;
        assert!(sequence.begin(&1, 1).is_none());
        assert!(!sequence.accepts(&1, old));
        sequence.select(Some(2));
        assert!(sequence.begin(&2, 0).is_none());

        let mut generation = FrameSequence {
            generation: u64::MAX,
            ..FrameSequence::default()
        };
        generation.select(Some(1));
        assert!(generation.begin(&1, 0).is_none());
    }

    #[test]
    fn a_new_surface_cannot_clear_or_repaint_an_unretired_old_body() {
        let mut gate = active();
        let mut lease = SurfaceLease::default();
        let old = token();
        let mut next = token();
        next.epoch += 1;
        assert!(lease.record_body(old.clone(), 41));
        assert!(lease.can_paint(&old, &41));
        assert!(!lease.can_paint(&old, &42));
        assert!(!lease.can_clear(&old, &42));
        assert!(!lease.record_body(next.clone(), 42));
        gate.begin_frame(6);
        gate.receive(Reply::Revoke { token: old.clone() });
        lease.refresh(&gate);
        assert!(
            !lease.can_clear(&old, &42),
            "revocation intent is not retirement"
        );
        let clear = CapturedFrame::capture(stamp(7), false, &gate).unwrap();
        assert!(clear.prepare(&mut gate));
        assert!(matches!(
            gate.frame_presented(clear.serial()),
            Some(Request::Retired { .. })
        ));
        lease.refresh(&gate);
        assert!(
            !lease.can_paint(&next, &42),
            "submitting retirement is not acknowledgement"
        );
        gate.receive(Reply::RetirementAccepted { token: old });
        lease.refresh(&gate);
        assert!(lease.record_body(next, 42));
    }
}
