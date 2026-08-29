//! anvil's binding to the shared correction engine in
//! `jterm_core::command_correction`.
//!
//! Every jterm terminal grew its own copy of the "that command failed, here is
//! a fix" flow, and the engine half of those files contained no toolkit code at
//! all, so the copies drifted in both directions. The union — classification,
//! token extraction, ranking, the safety gate, the prompt, the reply parser,
//! the probe layer and the resolvers — now lives in
//! `jterm_core::command_correction`. What is left here is anvil's presentation
//! and submission channel: the inline `CommandReviewCard` in the Block
//! conversation, the per-pane session map that owns that widget's lifetime, and
//! the relm4 message plumbing that connects them.
//!
//! What anvil still decides for itself, and states out loud below:
//!
//! - **Local evidence.** Under Flatpak anvil has no host bridge for helpers, so
//!   nothing local can be proven; off Flatpak it keeps its PATH scan. Both are
//!   [`LocalEvidence`] arguments now instead of an `is_flatpak()` probe buried
//!   in the engine, where one app's answer would have become everyone's.
//! - **Consent.** `ai_share_command_context` gates the provider fallback. anvil
//!   honoured that switch in `task_ops`, `ai_palette_ops` and `agent_task_ui`
//!   but not here, on the surface with the largest payload of the four; the
//!   engine now demands a [`ConsentProof`](jterm_core::command_correction) that
//!   only a consenting policy can produce.
//! - **Launch mode.** `--safe-mode` suppresses the whole monitor. That is
//!   anvil's meaning of the flag — forge's is narrower and the other two apps
//!   have no such flag — so it feeds the engine's `enabled` bool from here
//!   rather than being hardcoded in shared code.
//!
//! The engine's own semantics are covered by its unit tests. What is tested
//! here is the wiring: that anvil states each of those choices, that the card's
//! primary label and what accepting actually does are one decision, and that a
//! completion anvil did not see the shell report never raises a card.

use super::*;
use crate::command_review::{CommandReviewCard, CommandReviewSpec, ReviewPresentation};
use jterm_core::block_contract::CompletionProvenance;
use jterm_core::command_correction::{
    correction_monitor_enabled, correction_prompt, deterministic_candidate, parse_ai_reply,
    should_start, CompletionFacts, ContextSharing, CorrectionPolicy, CorrectionProposal,
    CorrectionRequest, HelperStrategy, LocalEvidence, Original, CORRECTION_REQUEST_TIMEOUT,
};
use std::ffi::OsStr;
use std::time::Instant;

/// The engine's candidate type travels through `AppMsg` from the probe thread.
pub(crate) use jterm_core::command_correction::CorrectionCandidate;

/// Names the probe's stdout reader thread, so a reader stuck on a descendant
/// that kept the pipe open is attributable to anvil in `ps`.
const PROBE_THREAD_NAME: &str = "anvil-command-correction-probe";

/// Where anvil may look for evidence about the environment a failed command
/// actually ran in.
///
/// Under Flatpak the command ran on the host while this process's PATH
/// describes the sandbox, so sandbox executables are not evidence about it.
/// anvil ships no host bridge for automatic helpers (forge does), so the honest
/// answer is [`LocalEvidence::Unavailable`]: deterministic target-output
/// corrections still work, APT and PATH evidence does not. That is what anvil
/// already did, but as an early `return Vec::new()` inside the PATH walk, which
/// also silently abandoned the `compgen` probe that a bridge would have made
/// work. Stating it here means adopting `LocalEvidence::Bridged` later is a
/// one-line change in this function.
///
/// Off Flatpak anvil keeps the PATH scan it always had — dropping it would cost
/// APT and PATH evidence on every non-FHS host, `nix develop` included. What
/// changes is the trust predicate. anvil's own asked
/// `owner_uid == euid || mode & 0o022 != 0`, which called a binary owned by a
/// *third* user trusted: `/opt/vendor/bin/bash` owned by `builder`, mode 0755,
/// ahead of `/usr/bin` on a shared box was spawned automatically by any failed
/// command. The same expression called every system binary untrusted when anvil
/// itself runs as root, which silently killed APT-verified corrections in
/// containers. [`HelperStrategy::TrustedPathScan`] resolves through
/// `jterm_core::helper` instead, which is the family's one answer to that
/// question and has the rationale written out.
fn local_evidence(flatpak: bool, search_path: Option<&OsStr>) -> LocalEvidence {
    if flatpak {
        return LocalEvidence::Unavailable;
    }
    LocalEvidence::SameNamespace {
        search_path: search_path
            .map(|path| std::env::split_paths(path).collect())
            .unwrap_or_default(),
        helpers: HelperStrategy::TrustedPathScan,
    }
}

/// Whether this failure's command, working directory and terminal output may
/// leave the machine.
///
/// The correction gate already requires `ai_enabled`, so the consent switch is
/// the whole question here.
fn context_sharing(config: &Config) -> ContextSharing {
    if config.ai_share_command_context {
        ContextSharing::Consented
    } else {
        ContextSharing::Withheld
    }
}

/// anvil's complete answer to everything the engine refuses to guess.
///
/// Built per request rather than at startup: both halves are live config
/// values, and the consent switch in particular must be read at the moment the
/// payload would be sent, not at the moment the app launched.
fn correction_policy(config: &Config) -> CorrectionPolicy {
    CorrectionPolicy::new(
        local_evidence(
            crate::host::is_flatpak(),
            std::env::var_os("PATH").as_deref(),
        ),
        context_sharing(config),
        PROBE_THREAD_NAME,
    )
}

/// The engine's `enabled` bool, as anvil computes it.
///
/// `--safe-mode` is anvil's own suppression and stays here: a user reaching for
/// it after a crash is not expecting background AI requests, while forge's flag
/// with the same name means only "default config, no session restore".
fn correction_enabled(safe_mode: bool, config: &Config, agent_active: bool) -> bool {
    !safe_mode
        && correction_monitor_enabled(
            config.ai_enabled,
            config.command_correction_enabled,
            agent_active,
        )
}

/// Whether the shell itself reported this completion's status.
///
/// A block closed by boundary inference — a later prompt forced it shut, the
/// end mark never arrived — attributes stale scrollback and, because
/// `pending_exit_code` is cleared at the reset boundaries rather than at
/// finalize, potentially a *previous* command's status to the command being
/// classified. The classifier would then read "command not found" out of the
/// wrong output and every later step would be built on that misattribution.
/// anvil used to gate only on an exit code being present, which held solely
/// because those two paths happen to clear the code as well.
fn completion_is_trusted(provenance: CompletionProvenance) -> bool {
    matches!(provenance, CompletionProvenance::ShellReported)
}

/// What the finished-block bridge knows about a command that just ended.
///
/// Grouped rather than passed positionally: `agent_issued` and
/// `trusted_completion` are adjacent bools with opposite meanings, and a swap
/// would compile.
pub(crate) struct FinishedBlock {
    pub(crate) command: String,
    /// `None` means the shell reported no status. Not a failure signal.
    pub(crate) exit_code: Option<i32>,
    pub(crate) output: String,
    pub(crate) agent_issued: bool,
    pub(crate) completion_provenance: CompletionProvenance,
}

/// One pane's live correction, from classification to teardown.
///
/// The generation is anvil's epoch: a newer failure in the same pane replaces
/// this session, and a reply that arrives against a retired generation is
/// dropped. Single consumption is the map removal in
/// [`AppModel::close_command_correction_for_pane`], which also removes the
/// inline widget — the two must stay one step, which is why anvil keeps its own
/// map rather than adopting the engine's `CorrectionRequestState`.
pub(crate) struct CorrectionSession {
    generation: u64,
    request: CorrectionRequest,
    deadline: Instant,
    resolving: bool,
    proposal: Option<CorrectionProposal>,
    card: Option<gtk::Widget>,
    review: Option<CommandReviewCard>,
    local_cancellation: ai::AiCancellationToken,
    in_flight: Option<ai::AiHandle>,
}

fn primary_label(run_directly: bool) -> &'static str {
    if run_directly {
        "Run verified command"
    } else {
        "Insert for review"
    }
}

/// The proposal as it stands against the live field text.
///
/// The `gtk::Entry` owns the draft here, so every decision re-syncs a copy of
/// the session's proposal from it and asks the engine. Both decisions — the
/// label the primary button carries and what pressing it actually does — then
/// come from one `CorrectionProposal`, which is what stops a button reading
/// "Insert for review" while the shim submits. It also puts this surface's own
/// 16 KiB budget on the accepted draft: anvil validated it with
/// `review_input::validate` alone, whose limit is 256 KiB, so a paste into the
/// correction field could queue a 200 KiB command from a surface that declares
/// a 16 KiB limit.
fn live_proposal(proposal: &CorrectionProposal, draft: &str) -> CorrectionProposal {
    let mut live = proposal.clone();
    draft.clone_into(live.draft_mut());
    live
}

impl AppModel {
    pub(crate) fn maybe_start_command_correction(
        &self,
        pane_id: u64,
        block: FinishedBlock,
        sender: &ComponentSender<AppModel>,
    ) {
        self.close_command_correction_for_pane(pane_id);
        let (enabled, policy) = {
            let config = self.config.borrow();
            let enabled = correction_enabled(
                self.safe_mode,
                &config,
                self.active_agent.borrow().is_some(),
            );
            // Building the policy reads PATH, so skip it when nothing will run.
            (enabled, enabled.then(|| correction_policy(&config)))
        };
        let Some(policy) = policy else {
            return;
        };
        let Some((tab_index, pane_index)) = self.find_pane(pane_id) else {
            return;
        };
        let pane = &self.tabs[tab_index].panes[pane_index];
        if !pane.terminal.supports_inline_notices() {
            log::debug!("pane has no inline card surface: skipping command correction");
            return;
        }
        // The engine samples the block output itself before classifying it, so
        // the whole event sample goes in: sampling twice elides real content
        // out of the middle of the first pass.
        let Some(request) = should_start(
            enabled,
            CompletionFacts {
                command: block.command,
                exit_code: block.exit_code,
                output: block.output,
                cwd: Some(pane.cwd.clone().unwrap_or_else(|| ".".to_string())),
                remote: pane.cwd_external,
                agent_issued: block.agent_issued,
                trusted_completion: completion_is_trusted(block.completion_provenance),
            },
        ) else {
            return;
        };
        let generation = self
            .command_correction_generation
            .get()
            .checked_add(1)
            .unwrap_or(1);
        self.command_correction_generation.set(generation);
        let cancellation = ai::AiCancellationToken::new();
        let deadline = Instant::now() + CORRECTION_REQUEST_TIMEOUT;
        self.command_corrections.borrow_mut().insert(
            pane_id,
            CorrectionSession {
                generation,
                request: request.clone(),
                deadline,
                resolving: true,
                proposal: None,
                card: None,
                review: None,
                local_cancellation: cancellation.clone(),
                in_flight: None,
            },
        );
        let reply_sender = sender.clone();
        let spawn = std::thread::Builder::new()
            .name("anvil-command-correction-local".to_string())
            .spawn(move || {
                let candidate = deterministic_candidate(&policy, &request, &cancellation, deadline);
                reply_sender.input(AppMsg::CommandCorrectionLocalReply {
                    pane_id,
                    generation,
                    candidate,
                });
            });
        if let Err(error) = spawn {
            log::warn!("could not start local correction probe: {error}");
            self.close_command_correction_generation(pane_id, generation);
            return;
        }
        let timeout_sender = sender.clone();
        gtk::glib::timeout_add_local_once(CORRECTION_REQUEST_TIMEOUT, move || {
            timeout_sender.input(AppMsg::CommandCorrectionTimeout {
                pane_id,
                generation,
            });
        });
    }

    pub(crate) fn command_correction_local_reply(
        &self,
        pane_id: u64,
        generation: u64,
        candidate: Option<CorrectionCandidate>,
        sender: &ComponentSender<AppModel>,
    ) {
        let current = self
            .command_corrections
            .borrow()
            .get(&pane_id)
            .is_some_and(|session| {
                session.generation == generation && Instant::now() < session.deadline
            });
        if !current
            || self.active_agent.borrow().is_some()
            || !self.config.borrow().command_correction_enabled
        {
            self.close_command_correction_generation(pane_id, generation);
            return;
        }
        if let Some(candidate) = candidate {
            self.render_command_correction(pane_id, generation, candidate, sender);
            return;
        }
        // Local evidence never left the machine. The provider fallback ships
        // the failed command, the working directory and up to 8 KiB of terminal
        // output, so it runs only with consent stated, and the engine will not
        // build the payload without the witness that says so.
        let Some(consent) = correction_policy(&self.config.borrow()).consent() else {
            log::debug!(
                "command correction stopped after local evidence: \
                 sending command context needs ai_share_command_context"
            );
            self.close_command_correction_generation(pane_id, generation);
            return;
        };
        let client = match ai::client_from_config(&self.config.borrow()) {
            Ok(client) => client,
            Err(error) => {
                log::warn!("command correction provider unavailable: {error}");
                self.close_command_correction_generation(pane_id, generation);
                return;
            }
        };
        let (system, user) = {
            let sessions = self.command_corrections.borrow();
            let Some(session) = sessions
                .get(&pane_id)
                .filter(|session| session.generation == generation)
            else {
                return;
            };
            correction_prompt(consent, &session.request)
        };
        let sender = sender.clone();
        let handle = ai::ask(client, system, user, move |reply| {
            sender.input(AppMsg::CommandCorrectionAiReply {
                pane_id,
                generation,
                reply,
            });
        });
        let mut handle = Some(handle);
        if let Some(session) = self
            .command_corrections
            .borrow_mut()
            .get_mut(&pane_id)
            .filter(|session| session.generation == generation)
        {
            session.in_flight = handle.take();
        }
        drop(handle);
    }

    pub(crate) fn command_correction_ai_reply(
        &self,
        pane_id: u64,
        generation: u64,
        reply: Result<String, String>,
        sender: &ComponentSender<AppModel>,
    ) {
        let current = self
            .command_corrections
            .borrow()
            .get(&pane_id)
            .is_some_and(|session| {
                session.generation == generation && Instant::now() < session.deadline
            });
        if !current {
            self.close_command_correction_generation(pane_id, generation);
            return;
        }
        let original = {
            let mut sessions = self.command_corrections.borrow_mut();
            let Some(session) = sessions
                .get_mut(&pane_id)
                .filter(|session| session.generation == generation)
            else {
                return;
            };
            session.in_flight.take();
            session.request.command().to_string()
        };
        if self.active_agent.borrow().is_some() || !self.config.borrow().command_correction_enabled
        {
            self.close_command_correction_generation(pane_id, generation);
            return;
        }
        let candidate = match reply.and_then(|raw| {
            parse_ai_reply(Original(&original), &raw).map_err(|error| error.to_string())
        }) {
            Ok(Some(candidate)) => candidate,
            Ok(None) => {
                self.close_command_correction_generation(pane_id, generation);
                return;
            }
            Err(error) => {
                log::debug!("command correction produced no safe candidate: {error}");
                self.close_command_correction_generation(pane_id, generation);
                return;
            }
        };
        self.render_command_correction(pane_id, generation, candidate, sender);
    }

    fn render_command_correction(
        &self,
        pane_id: u64,
        generation: u64,
        candidate: CorrectionCandidate,
        sender: &ComponentSender<AppModel>,
    ) {
        let compact = self.config.borrow().block_compact;
        let mut sessions = self.command_corrections.borrow_mut();
        let Some(session) = sessions
            .get_mut(&pane_id)
            .filter(|session| session.generation == generation)
        else {
            return;
        };
        let proposal = CorrectionProposal::new(candidate);
        let direct_run = proposal.run_allowed();
        // Every string on the card comes from the engine already sanitised:
        // the model's prose is not kept in raw form at all, and the failed
        // command is collapsed to one bounded line.
        let review = CommandReviewCard::new(CommandReviewSpec {
            presentation: ReviewPresentation::Standalone,
            compact,
            icon_name: "tools-check-spelling-symbolic",
            title: proposal.candidate().display_title().to_string(),
            badge: proposal
                .candidate()
                .display_badge(session.request.exit_code()),
            description: proposal
                .candidate()
                .display_description(session.request.command()),
            command: proposal.draft().to_string(),
            primary_label: primary_label(direct_run).to_string(),
            primary_executes: direct_run,
            auxiliary_label: None,
            secondary_label: Some("Dismiss".to_string()),
            close_button: true,
        });
        {
            let sender = sender.clone();
            review.primary.connect_clicked(move |_| {
                sender.input(AppMsg::CommandCorrectionAccept {
                    pane_id,
                    generation,
                });
            });
        }
        {
            let sender = sender.clone();
            review.entry.connect_activate(move |_| {
                sender.input(AppMsg::CommandCorrectionAccept {
                    pane_id,
                    generation,
                });
            });
        }
        {
            let primary = review.primary_controller();
            let proposal = proposal.clone();
            review.entry.connect_changed(move |entry| {
                let command = entry.text();
                let executable = live_proposal(&proposal, &command).run_allowed();
                primary.set(primary_label(executable), executable, &command);
            });
        }
        if let Some(dismiss) = review.secondary.as_ref() {
            let sender = sender.clone();
            dismiss.connect_clicked(move |_| {
                sender.input(AppMsg::CommandCorrectionDismiss {
                    pane_id,
                    generation,
                });
            });
        }
        if let Some(close) = review.close.as_ref() {
            let sender = sender.clone();
            close.connect_clicked(move |_| {
                sender.input(AppMsg::CommandCorrectionDismiss {
                    pane_id,
                    generation,
                });
            });
        }
        review.root.add_css_class("block-correction");
        let card: gtk::Widget = review.root.clone().upcast();
        let keys = gtk::EventControllerKey::new();
        {
            let sender = sender.clone();
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    sender.input(AppMsg::CommandCorrectionDismiss {
                        pane_id,
                        generation,
                    });
                    gtk::glib::Propagation::Stop
                } else {
                    gtk::glib::Propagation::Proceed
                }
            });
        }
        review.root.add_controller(keys);
        session.resolving = false;
        session.proposal = Some(proposal);
        session.card = Some(card.clone());
        let focus_review = self
            .correction_terminal(pane_id)
            .is_some_and(|terminal| terminal.command_prompt_status().is_ready());
        session.review = Some(review);
        drop(sessions);
        let inserted = self
            .correction_terminal(pane_id)
            .is_some_and(|terminal| terminal.insert_inline_notice(&card));
        if !inserted {
            self.close_command_correction_generation(pane_id, generation);
            return;
        }
        if focus_review {
            if let Some(review) = self
                .command_corrections
                .borrow()
                .get(&pane_id)
                .and_then(|session| session.review.as_ref())
            {
                review.focus();
            }
        }
    }

    pub(crate) fn accept_command_correction(&self, pane_id: u64, generation: u64) {
        let (command, run) = {
            let sessions = self.command_corrections.borrow();
            let Some(session) = sessions
                .get(&pane_id)
                .filter(|session| session.generation == generation)
            else {
                return;
            };
            let (Some(review), Some(proposal)) =
                (session.review.as_ref(), session.proposal.as_ref())
            else {
                return;
            };
            match live_proposal(proposal, &review.entry.text()).accept() {
                Ok(accepted) => (accepted.command, accepted.run_directly),
                Err(error) => {
                    review.show_error(&format!("Cannot accept correction: {error}"));
                    return;
                }
            }
        };
        let status = self
            .correction_terminal(pane_id)
            .map(TermCtl::command_prompt_status);
        if !status.is_some_and(|status| status.is_ready()) {
            if let Some(review) = self
                .command_corrections
                .borrow()
                .get(&pane_id)
                .and_then(|session| session.review.as_ref())
            {
                review.show_error(
                    status
                        .map(|status| status.blocked_message())
                        .unwrap_or("The target Block pane no longer exists."),
                );
            }
            return;
        }
        let queued = self.correction_terminal(pane_id).is_some_and(|terminal| {
            if run {
                terminal.try_run_review_command(&command)
            } else {
                terminal.try_insert_agent_command(&command)
            }
        });
        if queued {
            if let Some(view) = self
                .correction_terminal(pane_id)
                .and_then(TermCtl::term_view)
            {
                self.organism_hub
                    .correction_signal()
                    .note_accepted(crate::organism_ui::pane_token(&view));
            }
            if let Some(terminal) = self.correction_terminal(pane_id) {
                terminal.emit(VteInput::GrabFocus);
            }
            self.close_command_correction_generation(pane_id, generation);
        } else if let Some(review) = self
            .command_corrections
            .borrow()
            .get(&pane_id)
            .and_then(|session| session.review.as_ref())
        {
            review.show_error("The target prompt changed before the command could be queued.");
        }
    }

    pub(crate) fn command_correction_timeout(&self, pane_id: u64, generation: u64) {
        let current = self
            .command_corrections
            .borrow()
            .get(&pane_id)
            .is_some_and(|session| session.generation == generation && session.resolving);
        if current {
            log::warn!(
                "command correction timed out after {} seconds",
                CORRECTION_REQUEST_TIMEOUT.as_secs()
            );
            self.close_command_correction_generation(pane_id, generation);
        }
    }

    pub(crate) fn close_command_correction_generation(&self, pane_id: u64, generation: u64) {
        let matches = self
            .command_corrections
            .borrow()
            .get(&pane_id)
            .is_some_and(|session| session.generation == generation);
        if matches {
            self.close_command_correction_for_pane(pane_id);
        }
    }

    pub(crate) fn dismiss_command_correction(&self, pane_id: u64, generation: u64) {
        let matches = self
            .command_corrections
            .borrow()
            .get(&pane_id)
            .is_some_and(|session| session.generation == generation);
        if matches {
            self.organism_hub.correction_signal().note_dismissed();
            self.close_command_correction_for_pane(pane_id);
        }
    }

    pub(crate) fn close_command_correction_for_pane(&self, pane_id: u64) {
        let Some(mut session) = self.command_corrections.borrow_mut().remove(&pane_id) else {
            return;
        };
        session.local_cancellation.cancel();
        session.in_flight.take();
        if let Some(card) = session.card.take() {
            if let Some(terminal) = self.correction_terminal(pane_id) {
                terminal.remove_inline_notice(&card);
            }
        }
    }

    pub(crate) fn close_all_command_corrections(&self) {
        let pane_ids = self
            .command_corrections
            .borrow()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for pane_id in pane_ids {
            self.close_command_correction_for_pane(pane_id);
        }
    }

    fn correction_terminal(&self, pane_id: u64) -> Option<&TermCtl> {
        let (tab_index, pane_index) = self.find_pane(pane_id)?;
        Some(&self.tabs[tab_index].panes[pane_index].terminal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jterm_core::command_correction::MAX_CORRECTION_COMMAND_BYTES;

    /// An AI-unverified candidate, which is the only kind a test can mint
    /// without a real probe. Verified evidence needs the APT index or the
    /// executable PATH, and the engine covers those.
    fn ai_candidate(original: &str, command: &str) -> CorrectionCandidate {
        let reply = serde_json::json!({
            "action": "suggest",
            "command": command,
            "message": "Fix the typo.",
        })
        .to_string();
        parse_ai_reply(Original(original), &reply)
            .expect("a strict-JSON suggestion parses")
            .expect("a suggest reply carries a candidate")
    }

    /// anvil's own launch-mode suppression. `--safe-mode` is a diagnosis mode:
    /// it must not spawn probes or contact a provider, whatever the config
    /// file says, and the engine deliberately does not know the flag exists.
    #[test]
    fn safe_mode_suppresses_the_monitor_that_config_alone_would_enable() {
        let mut config = Config::safe_defaults();
        config.ai_enabled = true;
        config.command_correction_enabled = true;

        assert!(correction_enabled(false, &config, false));
        assert!(!correction_enabled(true, &config, false));
        // An Agent session owns the prompt; correcting its command would fight
        // it. This half is the family-wide rule, applied through the engine.
        assert!(!correction_enabled(false, &config, true));
        config.ai_enabled = false;
        assert!(!correction_enabled(false, &config, false));
    }

    /// The consent switch anvil honoured everywhere except here.
    ///
    /// With it off, the policy yields no `ConsentProof`, and the engine's
    /// payload builder cannot be called without one — so the AI fallback in
    /// `command_correction_local_reply` is unreachable by construction rather
    /// than by a call-site check somebody can forget to write.
    #[test]
    fn the_provider_payload_is_unreachable_without_stated_consent() {
        let mut config = Config::safe_defaults();
        config.ai_enabled = true;
        config.command_correction_enabled = true;

        assert_eq!(context_sharing(&config), ContextSharing::Withheld);
        assert!(correction_policy(&config).consent().is_none());

        config.ai_share_command_context = true;
        assert_eq!(context_sharing(&config), ContextSharing::Consented);
        assert!(correction_policy(&config).consent().is_some());
    }

    /// anvil's local-evidence answer, stated rather than probed.
    ///
    /// Off Flatpak anvil keeps a PATH scan — but through `jterm_core::helper`'s
    /// predicate, not the hand-rolled one that trusted a third user's binary
    /// and distrusted every helper under euid 0. Under Flatpak nothing local is
    /// provable, because anvil has no host bridge for helpers.
    #[test]
    fn local_evidence_is_a_stated_choice_per_packaging() {
        let native = local_evidence(false, Some(OsStr::new("/usr/bin:relative:/opt/bin")));
        let LocalEvidence::SameNamespace {
            search_path,
            helpers,
        } = native
        else {
            panic!("a native anvil owns the namespace its commands ran in");
        };
        assert_eq!(helpers, HelperStrategy::TrustedPathScan);
        assert_eq!(search_path.len(), 3);

        assert!(matches!(
            local_evidence(true, Some(OsStr::new("/usr/bin"))),
            LocalEvidence::Unavailable
        ));
        assert!(matches!(
            local_evidence(false, None),
            LocalEvidence::SameNamespace { .. }
        ));
    }

    /// Only a status the shell itself reported may raise a card.
    #[test]
    fn only_a_shell_reported_completion_raises_a_card() {
        assert!(completion_is_trusted(CompletionProvenance::ShellReported));
        for provenance in [
            CompletionProvenance::BoundaryInferred,
            CompletionProvenance::JournalRecovered,
            CompletionProvenance::Unknown,
        ] {
            assert!(!completion_is_trusted(provenance));
            assert!(should_start(
                true,
                CompletionFacts {
                    command: "carog check".to_string(),
                    exit_code: Some(127),
                    output: "bash: carog: command not found".to_string(),
                    cwd: None,
                    remote: false,
                    agent_issued: false,
                    trusted_completion: completion_is_trusted(provenance),
                },
            )
            .is_none());
        }
    }

    /// The card's label and its action are one decision, taken against the
    /// live field text.
    ///
    /// `live_proposal` is the only place anvil turns entry text into an answer,
    /// and both the `connect_changed` handler and `accept_command_correction`
    /// go through it. An unverified candidate can never run, so the label must
    /// say "Insert for review" and accepting must agree.
    #[test]
    fn the_primary_label_and_the_accept_decision_come_from_one_proposal() {
        let proposal = CorrectionProposal::new(ai_candidate("git statsu", "git status"));
        for draft in ["git status", "git status --short", "  git status  "] {
            let live = live_proposal(&proposal, draft);
            let accepted = live.accept().expect("a one-line command is acceptable");
            assert_eq!(live.run_allowed(), accepted.run_directly);
            assert!(!accepted.run_directly, "an AI candidate is never verified");
            assert_eq!(primary_label(accepted.run_directly), "Insert for review");
        }
    }

    /// The accepted draft is bounded by this surface's own budget.
    ///
    /// anvil validated it with `review_input::validate` alone, whose limit is
    /// 256 KiB, so a paste into the correction field could queue a command an
    /// order of magnitude past the 16 KiB this surface declares. The engine's
    /// `accept` enforces the surface budget, and the refusal reaches the card
    /// through `show_error`.
    #[test]
    fn an_oversized_pasted_draft_cannot_be_queued_from_this_surface() {
        let proposal = CorrectionProposal::new(ai_candidate("git statsu", "git status"));
        let oversized = format!("echo {}", "x".repeat(MAX_CORRECTION_COMMAND_BYTES));
        let refusal = live_proposal(&proposal, &oversized)
            .accept()
            .expect_err("a draft past the surface budget is refused");
        assert!(
            refusal.to_string().contains("command limit"),
            "the refusal must name the budget: {refusal}"
        );
        assert!(live_proposal(&proposal, "echo ok").accept().is_ok());
    }
}
