//! Evidence mapping for Anvil's existing prompt/command state.
//!
//! Greeting source policy belongs to shared LiveAttention. No missing record,
//! empty writer queue or quiet-output interval establishes an idle prompt.

use super::BlockState;
use jterm_core::organism_live::LiveActivityKnowledge;

#[derive(Clone, Copy, Debug)]
pub(super) struct ActivityFacts {
    pub state: BlockState,
    /// Existing PromptEnd anchor fence, clean draft, foreground ownership and
    /// live cursor/suffix verification all passed at this observation.
    pub prompt_verified_empty: bool,
    pub typed_shadow_empty: bool,
    pub reviewed_submission_pending: bool,
    pub agent_execution_pending: bool,
}

pub(super) fn activity_knowledge(facts: ActivityFacts) -> LiveActivityKnowledge {
    if facts.state == BlockState::CollectingOutput {
        LiveActivityKnowledge::Busy
    } else if facts.state == BlockState::AwaitingCommand
        && facts.prompt_verified_empty
        && facts.typed_shadow_empty
        && !facts.reviewed_submission_pending
        && !facts.agent_execution_pending
    {
        LiveActivityKnowledge::KnownIdle
    } else {
        LiveActivityKnowledge::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jterm_core::organism::{Behavior, BodyLanguage, RenderContext};
    use jterm_core::organism_live::{
        LiveAttention, LiveAttentionEvent, LiveAttentionInput, LiveAttentionOutcome,
        LiveAttentionRejection, LiveAttentionSource,
    };
    use std::time::Duration;

    fn clean_prompt() -> ActivityFacts {
        ActivityFacts {
            state: BlockState::AwaitingCommand,
            prompt_verified_empty: true,
            typed_shadow_empty: true,
            reviewed_submission_pending: false,
            agent_execution_pending: false,
        }
    }

    fn input(facts: ActivityFacts) -> LiveAttentionInput {
        LiveAttentionInput {
            base_context: RenderContext::new(Behavior::Idle, BodyLanguage::default(), false),
            activity: activity_knowledge(facts),
            hard_eligible: true,
            hover_available: true,
            pointer_inside: true,
        }
    }

    #[test]
    fn native_state_evidence_never_infers_idle_from_missing_running() {
        for state in [
            BlockState::Idle,
            BlockState::CollectingPrompt,
            BlockState::AwaitingCommand,
            BlockState::CollectingOutput,
            BlockState::AltScreen,
            BlockState::PostCommand,
            BlockState::RawFallback,
        ] {
            for bits in 0u8..16 {
                let facts = ActivityFacts {
                    state,
                    prompt_verified_empty: bits & 1 != 0,
                    typed_shadow_empty: bits & 2 != 0,
                    reviewed_submission_pending: bits & 4 != 0,
                    agent_execution_pending: bits & 8 != 0,
                };
                let expected = if state == BlockState::CollectingOutput {
                    LiveActivityKnowledge::Busy
                } else if state == BlockState::AwaitingCommand && bits == 3 {
                    LiveActivityKnowledge::KnownIdle
                } else {
                    LiveActivityKnowledge::Unknown
                };
                assert_eq!(activity_knowledge(facts), expected, "{facts:?}");
            }
        }
    }

    #[test]
    fn real_shared_model_retires_dirty_candidate_without_replaying_on_clean_prompt() {
        let mut core = LiveAttention::default();
        core.set_owner(Some(1));
        let known = input(clean_prompt());
        let ticket = core.entry_offer().unwrap();
        assert_eq!(
            core.advance(
                Duration::ZERO,
                &1,
                known,
                LiveAttentionEvent::PointerEntered { ticket }
            )
            .unwrap()
            .outcome,
            LiveAttentionOutcome::CandidateArmed
        );
        let dirty = input(ActivityFacts {
            prompt_verified_empty: false,
            ..clean_prompt()
        });
        let lost = core
            .advance(
                Duration::from_millis(599),
                &1,
                dirty,
                LiveAttentionEvent::Reconcile,
            )
            .unwrap();
        assert!(lost.candidate.is_none());
        assert!(core
            .advance(
                Duration::from_millis(600),
                &1,
                known,
                LiveAttentionEvent::Reconcile
            )
            .unwrap()
            .active
            .is_none());
        assert_eq!(
            core.advance(
                Duration::from_millis(600),
                &1,
                dirty,
                LiveAttentionEvent::Explicit
            )
            .unwrap()
            .outcome,
            LiveAttentionOutcome::Accepted(LiveAttentionSource::Explicit),
            "the rejected passive visit did not spend the window cooldown"
        );
    }

    #[test]
    fn running_and_agent_uncertainty_reach_real_shared_source_policy() {
        let mut core = LiveAttention::default();
        core.set_owner(Some(1));
        let uncertain = input(ActivityFacts {
            agent_execution_pending: true,
            ..clean_prompt()
        });
        let ticket = core.entry_offer().unwrap();
        assert_eq!(
            core.advance(
                Duration::ZERO,
                &1,
                uncertain,
                LiveAttentionEvent::PointerEntered { ticket }
            )
            .unwrap()
            .outcome,
            LiveAttentionOutcome::Rejected(LiveAttentionRejection::UnknownCommandState)
        );
        assert_eq!(
            core.advance(Duration::ZERO, &1, uncertain, LiveAttentionEvent::Explicit)
                .unwrap()
                .outcome,
            LiveAttentionOutcome::Accepted(LiveAttentionSource::Explicit)
        );
        let deadline = core
            .snapshot(Duration::ZERO, &1, uncertain)
            .unwrap()
            .active
            .unwrap()
            .deadline;
        assert_eq!(
            core.advance(
                Duration::from_millis(1),
                &1,
                uncertain,
                LiveAttentionEvent::Reconcile
            )
            .unwrap()
            .active
            .unwrap()
            .deadline,
            deadline
        );
        let running = input(ActivityFacts {
            state: BlockState::CollectingOutput,
            ..clean_prompt()
        });
        let stopped = core
            .advance(
                Duration::from_millis(2),
                &1,
                running,
                LiveAttentionEvent::Reconcile,
            )
            .unwrap();
        assert!(stopped.active.is_none());
        assert_eq!(
            core.advance(
                Duration::from_millis(3),
                &1,
                running,
                LiveAttentionEvent::Explicit
            )
            .unwrap()
            .outcome,
            LiveAttentionOutcome::Rejected(LiveAttentionRejection::BusyContext)
        );
    }
}
