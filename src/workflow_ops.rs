//! Workflow discovery scheduling, lookup, and dialog dispatch.
//!
//! These remain inherent operations on the existing Relm4 `AppModel`; this
//! module only separates workflow responsibilities from the component
//! lifecycle. Reading and parsing belong to `jterm_core::workflows` (see
//! `crate::workflows`); what is anvil's is the decision to do it off the GTK
//! thread, behind one reservation, keeping the previous cache when a scan
//! fails — the only refresh strategy of the four that does not stall a UI on a
//! cold or networked home directory.

use super::*;

/// Admission gate shared by startup prewarming and every palette entry point.
///
/// The invariant — at most one scan in flight, and a completed scan re-arms —
/// is `jterm_core::workflows::RefreshLatch`; the thread, the panic containment
/// and the keep-the-old-cache policy below stay here, because they are GTK's
/// and the other three terminals are single-threaded immediate-mode.
pub(crate) use jterm_core::workflows::RefreshLatch as WorkflowRefreshState;

fn available_workflow_target(
    pane_id: Option<u64>,
    exists: impl FnOnce(u64) -> bool,
) -> Option<u64> {
    pane_id.filter(|id| exists(*id))
}

impl AppModel {
    pub(crate) fn workflow_target_pane(&self) -> Option<u64> {
        self.tabs
            .get(self.active)
            .and_then(|tab| tab.panes.get(tab.active_pane))
            .map(|pane| pane.id)
    }

    pub(crate) fn insert_workflow_command(&self, pane_id: u64, command: &str) {
        if available_workflow_target(Some(pane_id), |id| self.find_pane(id).is_some()).is_none() {
            self.show_toast(
                "Workflow was not inserted: its original terminal is no longer available.",
            );
            return;
        }
        // This only dispatches validated review text to the original component;
        // it is not an acknowledgment of PTY queue admission or delivery.
        self.insert_review_text_into_pane(pane_id, command);
    }

    /// Refresh the shared workflow cache without delaying palette presentation.
    /// At most one scan can be in flight; callers immediately keep using the
    /// last completed cache while this worker discovers and parses updates.
    pub(crate) fn refresh_workflows_async(&mut self, sender: &ComponentSender<AppModel>) {
        if !self.workflow_refresh.begin() {
            return;
        }

        let dirs = workflows::workflow_dirs();
        let reply = sender.clone();
        let spawn = std::thread::Builder::new()
            .name("anvil-workflow-refresh".to_string())
            .spawn(move || {
                let result = std::panic::catch_unwind(|| workflows::scan(&dirs))
                    .map_err(|_| "workflow loader panicked".to_string());
                reply.input(AppMsg::WorkflowRefreshFinished(result));
            });
        if let Err(error) = spawn {
            self.workflow_refresh.finish();
            let error = crate::review_input::safe_inline_display(&error.to_string(), 1024);
            log::warn!("could not start workflow refresh worker: {error}");
            self.show_toast(format!(
                "Workflows could not refresh because the background worker could not start: {error}"
            ));
        }
    }

    /// Install one completed snapshot, then notify the palette component. It
    /// checks its own presentation state, so a completion after Close updates
    /// only the cache and creates no hidden GTK rows.
    pub(crate) fn finish_workflow_refresh(
        &mut self,
        result: Result<workflows::LibraryScan, String>,
    ) {
        self.workflow_refresh.finish();
        match result {
            Ok(scan) => {
                self.report_refused_workflows(&scan.refused);
                *self.workflows.borrow_mut() = scan.workflows;
                self.command_palette
                    .emit(dialogs::command_palette::PaletteMsg::WorkflowsChanged);
            }
            Err(error) => {
                log::error!("workflow refresh failed: {error}");
                self.show_toast(
                    "Workflows could not be refreshed. The previous cache is still available.",
                );
            }
        }
    }

    /// Say once, per change, which files the loader turned down.
    ///
    /// anvil gained `O_NOFOLLOW` when the loader moved into `jterm_core`, so a
    /// symlinked workflow file it used to load is now refused — and a user who
    /// symlinked one out of a dotfiles checkout would otherwise see the entry
    /// simply not exist. The loader's log line is not a user-visible surface;
    /// this is.
    fn report_refused_workflows(&mut self, refused: &[(std::path::PathBuf, String)]) {
        if !refusals_changed(&self.workflow_refusals, refused) {
            return;
        }
        self.workflow_refusals = refused.iter().map(|(path, _)| path.clone()).collect();
        if let Some(message) = refusal_toast(refused) {
            self.show_toast(message);
        }
    }

    /// Look up a workflow by source path (the palette gives us a path, not
    /// an index, because the workflow list can be rebuilt between
    /// gather() and accept). If the workflow has no args, render and type
    /// immediately; otherwise open the param-fill dialog.
    pub(crate) fn run_workflow_from_path(
        &self,
        path: std::path::PathBuf,
        pane_id: Option<u64>,
        sender: &ComponentSender<AppModel>,
    ) {
        let Some(pane_id) = available_workflow_target(pane_id, |id| self.find_pane(id).is_some())
        else {
            self.show_toast(
                "Workflow was not opened: its original terminal is no longer available.",
            );
            return;
        };
        let workflow = self
            .workflows
            .borrow()
            .iter()
            .find(|w| w.source_path.as_deref() == Some(path.as_path()))
            .cloned();
        let Some(workflow) = workflow else {
            // The path came off disk, so its bytes are its author's: sanitise
            // it for the same reason the loader sanitises the paths it logs.
            let path = crate::review_input::safe_inline_display(
                &path.to_string_lossy(),
                TOAST_FIELD_BYTES,
            );
            log::warn!("workflow not found: {path}");
            self.show_toast(format!("Workflow not found: {path}"));
            return;
        };
        if workflow.args.is_empty() {
            // The same render path the dialog uses, so the literal-brace escape
            // and the review-input gate apply to a zero-argument workflow too.
            match workflows::ArgsForm::new(workflow).render() {
                Ok(command) => sender.input(AppMsg::WorkflowInsertCommand { pane_id, command }),
                Err(e) => {
                    log::warn!("workflow render failed: {e}");
                    self.show_toast(format!("Workflow could not be rendered: {e}"));
                }
            }
            return;
        }
        self.workflow_dialog
            .emit(dialogs::workflow::WorkflowMsg::Open { workflow, pane_id });
    }
}

/// Whether this scan's refusals differ from the ones already reported.
///
/// Keyed on the set of paths rather than a count, and in order, so a palette
/// opened twice over the same broken file is silent, a newly-broken file is
/// not, and fixing one of two files still reports the one that remains. The
/// *reason* is deliberately not part of the key: a file that is still refused
/// for a different reason is still the same refused file.
fn refusals_changed(
    reported: &[std::path::PathBuf],
    refused: &[(std::path::PathBuf, String)],
) -> bool {
    !refused.iter().map(|(path, _)| path).eq(reported.iter())
}

/// The toast for one scan's refusals, or `None` when nothing was refused.
///
/// Both halves are untrusted text: an attacker who can drop a file in a
/// scanned directory picks its name, and a parse error quotes the offending
/// source line back verbatim, so an unterminated `command = "echo <ESC>]0;…`
/// would otherwise put an OSC sequence into a GTK toast.
fn refusal_toast(refused: &[(std::path::PathBuf, String)]) -> Option<String> {
    let (path, reason) = refused.first()?;
    let path = crate::review_input::safe_inline_display(&path.to_string_lossy(), TOAST_FIELD_BYTES);
    let reason = crate::review_input::safe_inline_display(reason, TOAST_FIELD_BYTES);
    Some(if refused.len() == 1 {
        format!("Workflow file skipped — {path}: {reason}")
    } else {
        format!(
            "{} workflow files skipped, including {path}: {reason}",
            refused.len()
        )
    })
}

/// A toast is one line in a window, not a log: bound each untrusted half well
/// below the loader's own logging budget.
const TOAST_FIELD_BYTES: usize = 256;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn workflow_target_survives_reordering_but_never_falls_back_after_removal() {
        let captured = Some(42);
        assert_eq!(
            available_workflow_target(captured, |id| [42, 9].contains(&id)),
            Some(42)
        );
        assert_eq!(
            available_workflow_target(captured, |id| [9, 42].contains(&id)),
            Some(42)
        );
        assert_eq!(
            available_workflow_target(captured, |id| [9].contains(&id)),
            None
        );
        assert_eq!(
            available_workflow_target(None, |_| panic!("no target to check")),
            None
        );
    }

    #[test]
    fn workflow_target_is_carried_from_every_palette_entry_to_targeted_dispatch() {
        let action = include_str!("action_ops.rs");
        assert_eq!(action.matches("PaletteMsg::Toggle {").count(), 4);
        assert_eq!(
            action
                .matches("workflow_target: self.workflow_target_pane()")
                .count(),
            4
        );
        let palette = include_str!("dialogs/command_palette.rs")
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .unwrap();
        assert!(palette.contains("self.workflow_target = workflow_target"));
        assert!(palette.contains("PaletteOutput::RunWorkflow(path, workflow_target)"));
        let main = include_str!("main.rs");
        assert!(main.contains("AppMsg::PaletteRunWorkflow(path, pane_id)"));
        assert!(main.contains("self.run_workflow_from_path(path, pane_id, &sender)"));
        assert!(main.contains("self.insert_workflow_command(pane_id, &command)"));
        let workflow = include_str!("dialogs/workflow.rs")
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .unwrap();
        assert!(workflow.contains("self.target_pane = Some(pane_id)"));
        assert!(workflow.contains("let Some(pane_id) = self.target_pane"));
        assert!(workflow.contains("WorkflowOutput::Command { pane_id, command }"));
        let source = include_str!("workflow_ops.rs")
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .unwrap();
        let dispatch = source
            .split("pub(crate) fn insert_workflow_command(")
            .nth(1)
            .unwrap()
            .split("pub(crate) fn refresh_workflows_async(")
            .next()
            .unwrap();
        assert!(dispatch.contains("available_workflow_target(Some(pane_id)"));
        assert!(dispatch.contains("self.insert_review_text_into_pane(pane_id, command)"));
        assert!(!dispatch.contains("self.active"));
        let run = source
            .split("pub(crate) fn run_workflow_from_path(")
            .nth(1)
            .unwrap();
        assert!(run.contains("available_workflow_target(pane_id"));
        assert!(run.contains("AppMsg::WorkflowInsertCommand { pane_id, command }"));
        assert!(run.contains("WorkflowMsg::Open { workflow, pane_id }"));
        assert!(!run.contains("PaletteTypeCommand"));
        assert!(!run.contains("self.active"));
    }

    fn refusal(path: &str, reason: &str) -> (PathBuf, String) {
        (PathBuf::from(path), reason.to_string())
    }

    /// The single-flight latch itself is `jterm_core`'s and is tested there.
    /// What anvil owns is when the user is told a file was refused — anvil is
    /// the terminal that gained `O_NOFOLLOW`, so it is the one whose users can
    /// see a workflow they had yesterday disappear.
    #[test]
    fn refusals_are_announced_once_per_change_and_again_when_the_set_changes() {
        let broken = vec![refusal("/w/a.yaml", "parse YAML: bad")];
        assert!(refusals_changed(&[], &broken));

        let reported: Vec<PathBuf> = broken.iter().map(|(path, _)| path.clone()).collect();
        assert!(
            !refusals_changed(&reported, &broken),
            "reopening the palette over the same broken file must stay silent"
        );
        assert!(
            !refusals_changed(
                &reported,
                &[refusal(
                    "/w/a.yaml",
                    "read: Too many levels of symbolic links"
                )]
            ),
            "the same file refused for a new reason is still the same refusal"
        );
        assert!(
            refusals_changed(&reported, &[refusal("/w/b.yaml", "parse YAML: bad")]),
            "a different file must be announced"
        );
        assert!(
            refusals_changed(&reported, &[]),
            "clearing the last refusal is a change, and reporting nothing"
        );
        assert!(refusal_toast(&[]).is_none());
    }

    /// A refused file's name and the loader's reason both carry bytes their
    /// author chose. They must not reach a toast — or the terminal behind it —
    /// as control sequences.
    #[test]
    fn a_refusal_toast_names_the_file_without_replaying_its_bytes() {
        let toast = refusal_toast(&[refusal(
            "/w/\u{1b}]0;PWNED\u{7}.yaml",
            "parse TOML: at line 2\n  |\n2 | command = \"echo \u{202e}",
        )])
        .expect("one refusal produces one toast");
        assert!(toast.starts_with("Workflow file skipped — "), "{toast}");
        assert!(!toast.contains('\u{1b}'), "{toast}");
        assert!(!toast.contains('\u{7}'), "{toast}");
        assert!(!toast.contains('\u{202e}'), "{toast}");
        assert!(!toast.contains('\n'), "{toast}");

        let many = refusal_toast(&[
            refusal("/w/a.yaml", "parse YAML: bad"),
            refusal("/w/b.yaml", "read: Too many levels of symbolic links"),
        ])
        .unwrap();
        assert!(
            many.starts_with("2 workflow files skipped, including /w/a.yaml"),
            "{many}"
        );
    }
}
