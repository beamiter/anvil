//! Workflow discovery, lookup, rendering, and dialog dispatch.
//!
//! These remain inherent operations on the existing Relm4 `AppModel`; this
//! module only separates workflow responsibilities from the component lifecycle.

use super::*;

/// Admission gate shared by startup prewarming and every palette entry point.
/// The worker completion always releases it on the GTK thread, so repeated
/// shortcuts can reuse the current cache without creating a thread per press.
#[derive(Debug, Default)]
pub(crate) struct WorkflowRefreshState {
    in_flight: bool,
}

impl WorkflowRefreshState {
    fn begin(&mut self) -> bool {
        if self.in_flight {
            return false;
        }
        self.in_flight = true;
        true
    }

    fn finish(&mut self) {
        self.in_flight = false;
    }
}

impl AppModel {
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
                let result = std::panic::catch_unwind(|| workflows::load_all(&dirs))
                    .map_err(|_| "workflow loader panicked".to_string());
                reply.input(AppMsg::WorkflowRefreshFinished(result));
            });
        if let Err(error) = spawn {
            self.workflow_refresh.finish();
            let error = crate::text_safety::bounded_display_text(&error.to_string(), 1024, false);
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
        result: Result<Vec<workflows::Workflow>, String>,
    ) {
        self.workflow_refresh.finish();
        match result {
            Ok(loaded) => {
                *self.workflows.borrow_mut() = loaded;
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

    /// Look up a workflow by source path (the palette gives us a path, not
    /// an index, because the workflow list can be rebuilt between
    /// gather() and accept). If the workflow has no args, render and type
    /// immediately; otherwise open the param-fill dialog.
    pub(crate) fn run_workflow_from_path(
        &self,
        path: std::path::PathBuf,
        sender: &ComponentSender<AppModel>,
    ) {
        let workflow = self
            .workflows
            .borrow()
            .iter()
            .find(|w| w.source_path.as_deref() == Some(path.as_path()))
            .cloned();
        let Some(workflow) = workflow else {
            log::warn!("workflow not found: {}", path.display());
            self.show_toast(format!("Workflow not found: {}", path.display()));
            return;
        };
        if workflow.args.is_empty() {
            match workflows::render(&workflow, &std::collections::HashMap::new()) {
                Ok(rendered) => sender.input(AppMsg::PaletteTypeCommand(rendered)),
                Err(e) => {
                    log::warn!("workflow render failed: {e}");
                    self.show_toast(format!("Workflow could not be rendered: {e}"));
                }
            }
            return;
        }
        self.workflow_dialog
            .emit(dialogs::workflow::WorkflowMsg::Open(workflow));
    }
}

#[cfg(test)]
mod tests {
    use super::WorkflowRefreshState;

    #[test]
    fn workflow_refresh_is_single_flight_and_rearms_after_completion() {
        let mut state = WorkflowRefreshState::default();

        assert!(state.begin());
        assert!(
            !state.begin(),
            "a second request must reuse the in-flight scan"
        );

        state.finish();
        assert!(state.begin(), "completion must allow a later refresh");
    }

    #[test]
    fn failed_spawn_can_release_the_single_flight_reservation() {
        let mut state = WorkflowRefreshState::default();

        assert!(state.begin());
        state.finish();

        assert!(state.begin());
        assert!(!state.begin());
    }
}
