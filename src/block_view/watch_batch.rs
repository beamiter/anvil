//! Private content-free reader-batch normalization proposal.
//! No timer, cadence threshold, idle inference or parser trust decision lives here.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderBatchOrigin {
    /// Allocated once at the real nonempty PTY reader callback, before any hold.
    Fresh { sequence: u64 },
    /// Selection/backlog replay has no fresh batch identity and produces no pulse.
    BufferedReplay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchDisposition {
    Accepted,
    Quarantined,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchFact<I> {
    pub identity: I,
    pub sequence: u64,
    pub disposition: BatchDisposition,
}

/// One bounded collector per fresh reader invocation, spanning reset-splitter and
/// parser stretches. Trusted lifecycle callbacks update the caller's identity;
/// a raw OSC parser event alone is never authority to start an observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchBatch<I> {
    origin: ReaderBatchOrigin,
    first_output_identity: Option<I>,
    unattributed_or_mixed: bool,
    saw_output: bool,
}

impl<I: Eq + Clone> WatchBatch<I> {
    pub fn new(origin: ReaderBatchOrigin) -> Self {
        Self {
            origin,
            first_output_identity: None,
            unattributed_or_mixed: false,
            saw_output: false,
        }
    }

    /// Called for a nonempty output stretch at the existing accepted reader state,
    /// with the exact observation identity current at dispatch. Does not count it.
    pub fn note_output(&mut self, identity: Option<&I>) {
        if self.origin == ReaderBatchOrigin::BufferedReplay {
            return;
        }
        self.saw_output = true;
        let Some(identity) = identity else {
            self.unattributed_or_mixed = true;
            return;
        };
        match &self.first_output_identity {
            None => self.first_output_identity = Some(identity.clone()),
            Some(first) if first != identity => self.unattributed_or_mixed = true,
            Some(_) => {}
        }
    }

    /// Produce at most one fact, after the entire reader batch has dispatched.
    /// A finished observation never lends its bytes to a new command. Mixed
    /// ownership can only quarantine the surviving observation, not count it.
    pub fn finish(self, current_identity: Option<&I>) -> Option<BatchFact<I>> {
        let ReaderBatchOrigin::Fresh { sequence } = self.origin else {
            return None;
        };
        let current = current_identity?;
        if !self.saw_output {
            return None;
        }
        let disposition = if !self.unattributed_or_mixed
            && self.first_output_identity.as_ref() == Some(current)
        {
            BatchDisposition::Accepted
        } else {
            BatchDisposition::Quarantined
        };
        Some(BatchFact {
            identity: current.clone(),
            sequence,
            disposition,
        })
    }
}

/// Native-source-local issuance only. Never reset this for a parser segment or
/// replay. Exhaustion retires Watch until the native source epoch is replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadBatchSequence {
    next: Option<u64>,
}

impl Default for ReadBatchSequence {
    fn default() -> Self {
        Self { next: Some(0) }
    }
}

impl ReadBatchSequence {
    #[cfg(test)]
    pub(super) fn at_max_for_test() -> Self {
        Self {
            next: Some(u64::MAX),
        }
    }

    pub fn issue(&mut self) -> Option<u64> {
        let current = self.next?;
        self.next = current.checked_add(1);
        Some(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_issuance_never_wraps() {
        let mut sequence = ReadBatchSequence {
            next: Some(u64::MAX),
        };
        assert_eq!(sequence.issue(), Some(u64::MAX));
        assert_eq!(sequence.issue(), None);
        assert_eq!(sequence.issue(), None);
    }
}
