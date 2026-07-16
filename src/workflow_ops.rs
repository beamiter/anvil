//! Workflow discovery, lookup, rendering, and dialog dispatch.
//!
//! These remain inherent operations on the existing Relm4 `AppModel`; this
//! module only separates workflow responsibilities from the component lifecycle.

use super::*;

impl AppModel {
    /// Re-scan the user's workflow directory. Called before each palette
    /// open so users see new/edited TOML/YAML files without a restart. Cheap: a few
    /// short files, parsed once.
    pub(crate) fn reload_workflows(&self) {
        let dirs = workflows::workflow_dirs();
        let loaded = workflows::load_all(&dirs);
        *self.workflows.borrow_mut() = loaded;
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
