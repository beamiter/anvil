//! Relm4 component for filling workflow parameters.
//!
//! The fill model itself is `jterm_core::workflows::ArgsForm`. anvil used to
//! keep a `HashMap<String, String>` here and seed it with
//! `arg.default.unwrap_or_default()` for *every* declared argument, which is
//! what made `render()`'s missing-value guard unreachable: `kill -9 {pid}` with
//! an untouched Pid field rendered `kill -9 ` and was inserted at the prompt.
//! All four terminals did this, and three of them unit-tested the guard they
//! were defeating. `ArgsForm` keeps "untouched and undefaulted" apart from
//! "deliberately emptied", so the guard fires; what is left in this file is the
//! adw widgetry and the mapping between it and the form.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender, FactoryVecDeque};
use relm4::gtk;
use relm4::prelude::*;

use crate::workflows::{ArgsForm, Workflow};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

fn is_current_opening(current: &Option<Arc<()>>, expected: &Arc<()>) -> bool {
    current
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, expected))
}

/// One row's initial state, taken from the form rather than re-derived from
/// the argument, so what the entry shows and what the form will render are the
/// same value.
#[derive(Debug)]
struct WorkflowArgInit {
    opening: Arc<()>,
    name: String,
    description: String,
    value: String,
}

#[derive(Debug)]
struct WorkflowArgRow {
    opening: Arc<()>,
    name: String,
    description: String,
    value: String,
}

#[derive(Debug)]
enum WorkflowArgOutput {
    Changed(Arc<()>, String, String),
    Submit(Arc<()>),
}

#[relm4::factory]
impl FactoryComponent for WorkflowArgRow {
    type Init = WorkflowArgInit;
    type Input = ();
    type Output = WorkflowArgOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::Box;

    view! {
        root = adw::ActionRow {
            set_title: &escape_markup(&self.name),
            set_subtitle: &escape_markup(&self.description),

            add_suffix = &gtk::Entry {
                set_text: &self.value,
                set_hexpand: true,
                set_valign: gtk::Align::Center,
                connect_changed[sender, name = self.name.clone(), opening = self.opening.clone()] => move |entry| {
                    let _ = sender.output(WorkflowArgOutput::Changed(
                        opening.clone(),
                        name.clone(),
                        entry.text().to_string(),
                    ));
                },
                connect_activate[sender, opening = self.opening.clone()] => move |_| {
                    let _ = sender.output(WorkflowArgOutput::Submit(opening.clone()));
                },
            },
        }
    }

    fn init_model(init: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self {
            opening: init.opening,
            name: init.name,
            description: init.description,
            value: init.value,
        }
    }
}

#[derive(Debug)]
pub(crate) enum WorkflowMsg {
    Open { workflow: Workflow, pane_id: u64 },
    Changed(Arc<()>, String, String),
    Submit(Arc<()>),
    Close(Arc<()>),
}

#[derive(Debug)]
pub(crate) enum WorkflowOutput {
    Command { pane_id: u64, command: String },
}

pub(crate) struct WorkflowModel {
    parent: adw::ApplicationWindow,
    form: Option<ArgsForm>,
    target_pane: Option<u64>,
    opening: Rc<RefCell<Option<Arc<()>>>>,
    rows: FactoryVecDeque<WorkflowArgRow>,
}

#[relm4::component(pub(crate))]
impl Component for WorkflowModel {
    type Init = adw::ApplicationWindow;
    type Input = WorkflowMsg;
    type Output = WorkflowOutput;
    type CommandOutput = ();

    view! {
        root = adw::Dialog {
            add_css_class: "workspace-dialog",
            set_content_width: 520,
            set_content_height: 0,

            #[wrap(Some)]
            set_child = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},

                #[wrap(Some)]
                set_content = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 8,
                    set_margin_all: 12,

                    #[name(description)]
                    gtk::Label {
                        set_halign: gtk::Align::Start,
                        set_wrap: true,
                        add_css_class: "dim-label",
                    },

                    #[name(preview)]
                    gtk::Label {
                        set_halign: gtk::Align::Start,
                        set_wrap: true,
                        set_selectable: true,
                    },

                    #[local_ref]
                    args_box -> gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                    },

                    #[name(empty_label)]
                    gtk::Label {
                        set_label: "This workflow has no parameters.",
                        set_halign: gtk::Align::Start,
                        add_css_class: "dim-label",
                    },

                    #[name(error_label)]
                    gtk::Label {
                        set_halign: gtk::Align::Start,
                        set_wrap: true,
                        set_visible: false,
                        add_css_class: "error",
                    },

                    #[name(actions_box)]
                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 6,
                        set_halign: gtk::Align::End,
                    },
                },
            },
        }
    }

    fn init(
        parent: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let rows =
            FactoryVecDeque::builder()
                .launch_default()
                .forward(sender.input_sender(), |output| match output {
                    WorkflowArgOutput::Changed(opening, name, value) => {
                        WorkflowMsg::Changed(opening, name, value)
                    }
                    WorkflowArgOutput::Submit(opening) => WorkflowMsg::Submit(opening),
                });
        let model = Self {
            parent,
            form: None,
            target_pane: None,
            opening: Rc::new(RefCell::new(None)),
            rows,
        };
        let args_box = model.rows.widget();
        let widgets = view_output!();
        // AdwDialog emits closed synchronously when closing begins. Capture
        // that opening here, not when the queued Relm message is handled.
        widgets.root.connect_closed({
            let opening = model.opening.clone();
            move |_| {
                let current = opening.borrow().clone();
                if let Some(current) = current {
                    sender.input(WorkflowMsg::Close(current));
                }
            }
        });
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        let expected = match &msg {
            WorkflowMsg::Open { .. } => None,
            WorkflowMsg::Changed(opening, ..)
            | WorkflowMsg::Submit(opening)
            | WorkflowMsg::Close(opening) => Some(opening),
        };
        if expected.is_some_and(|expected| !is_current_opening(&self.opening.borrow(), expected)) {
            return;
        }
        match msg {
            WorkflowMsg::Open { workflow, pane_id } => {
                self.target_pane = Some(pane_id);
                let opening = Arc::new(());
                *self.opening.borrow_mut() = Some(opening.clone());
                root.set_title(&format!("Workflow: {}", workflow.name));
                widgets.description.set_label(&workflow.description);
                widgets
                    .description
                    .set_visible(!workflow.description.is_empty());
                widgets
                    .preview
                    .set_markup(&format!("<tt>{}</tt>", escape_markup(&workflow.command)));
                widgets.empty_label.set_visible(workflow.args.is_empty());
                widgets.error_label.set_visible(false);

                let form = ArgsForm::new(workflow);
                let mut rows = self.rows.guard();
                rows.clear();
                for (index, arg) in form.args().iter().enumerate() {
                    rows.push_back(WorkflowArgInit {
                        opening: opening.clone(),
                        name: arg.name.clone(),
                        description: arg.description.clone(),
                        // Empty for an argument the file gives no default for —
                        // which is also what it *means*, and why Insert will
                        // report it rather than substituting a blank.
                        value: form.value(index).to_string(),
                    });
                }
                drop(rows);
                self.form = Some(form);
                // Recreate controls with this opening, so an old button gesture
                // cannot submit or close a replacement form after it is opened.
                while let Some(child) = widgets.actions_box.first_child() {
                    widgets.actions_box.remove(&child);
                }
                let cancel = gtk::Button::with_label("Cancel");
                cancel.connect_clicked({
                    let sender = sender.clone();
                    let opening = opening.clone();
                    move |_| sender.input(WorkflowMsg::Close(opening.clone()))
                });
                let run_button = gtk::Button::with_label("Insert command");
                run_button.add_css_class("suggested-action");
                run_button.connect_clicked({
                    let opening = opening.clone();
                    move |_| sender.input(WorkflowMsg::Submit(opening.clone()))
                });
                widgets.actions_box.append(&cancel);
                widgets.actions_box.append(&run_button);
                root.present(Some(&self.parent));
                if self.rows.is_empty() {
                    run_button.grab_focus();
                }
            }
            WorkflowMsg::Changed(_, name, value) => {
                // Rows are keyed by argument name because `validate` rejects a
                // workflow with duplicate argument names (and, since the
                // consolidation, one whose name is not equal to its own trim),
                // so name to row is one-to-one for every workflow that can be
                // opened here at all.
                let Some(form) = self.form.as_mut() else {
                    return;
                };
                if let Some(index) = form.args().iter().position(|arg| arg.name == name) {
                    form.set(index, value);
                }
            }
            WorkflowMsg::Submit(_) => {
                let Some(form) = self.form.as_ref() else {
                    return;
                };
                let Some(pane_id) = self.target_pane else {
                    return;
                };
                match form.render() {
                    Ok(command) => {
                        self.opening.borrow_mut().take();
                        self.form = None;
                        self.target_pane = None;
                        root.force_close();
                        let _ = sender.output(WorkflowOutput::Command { pane_id, command });
                    }
                    Err(error) => {
                        // Includes "missing values: <names>" for an argument
                        // the file declares no default for and the user left
                        // blank. That message existed and was unit-tested
                        // before; it could not be reached from this dialog.
                        widgets.error_label.set_label(&error);
                        widgets.error_label.set_visible(true);
                        log::warn!("workflow render failed: {error}");
                    }
                }
            }
            WorkflowMsg::Close(_) => {
                self.opening.borrow_mut().take();
                self.form = None;
                self.target_pane = None;
                root.force_close();
            }
        }
    }
}

fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::is_current_opening;
    use std::sync::Arc;

    #[test]
    fn workflow_factory_messages_satisfy_the_forwarding_send_contract() {
        fn assert_send<T: Send>() {}
        assert_send::<super::WorkflowArgOutput>();
        assert_send::<super::WorkflowMsg>();
        assert_send::<super::WorkflowOutput>();
    }

    #[test]
    fn replaced_workflow_rejects_old_row_submit_and_close_tokens() {
        let old = Arc::new(());
        let old_row = old.clone();
        let old_submit = old.clone();
        let old_close = old.clone();
        let current = Arc::new(());
        let active = Some(current.clone());
        for token in [old_row, old_submit, old_close] {
            assert!(!is_current_opening(&active, &token));
        }
        assert!(is_current_opening(&active, &current));
        assert!(!Arc::ptr_eq(&old, &current));
    }

    #[test]
    fn retiring_a_workflow_rejects_duplicate_submit_and_late_callbacks() {
        let opening = Arc::new(());
        let mut active = Some(opening.clone());
        assert!(is_current_opening(&active, &opening));
        active.take();
        assert!(!is_current_opening(&active, &opening));
        let reopened = Arc::new(());
        active = Some(reopened.clone());
        assert!(!is_current_opening(&active, &opening));
        assert!(is_current_opening(&active, &reopened));
    }

    #[test]
    fn workflow_callbacks_keep_their_opening_through_factory_and_native_close_queues() {
        let source = include_str!("workflow.rs")
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .unwrap();
        assert!(source.contains("WorkflowMsg::Changed(opening, name, value)"));
        assert!(
            source.contains("WorkflowArgOutput::Submit(opening) => WorkflowMsg::Submit(opening)")
        );
        let update = source.split("    fn update_with_view(").nth(1).unwrap();
        assert!(update.find("!is_current_opening").unwrap() < update.find("match msg {").unwrap());
        let open = update
            .split("WorkflowMsg::Open { workflow, pane_id } => {")
            .nth(1)
            .unwrap();
        assert!(open.find("Some(opening.clone())").unwrap() < open.find("rows.clear()").unwrap());
        assert!(open.contains("opening: opening.clone()"));
        assert!(open.contains("widgets.actions_box.remove(&child)"));
        assert!(open.contains("gtk::Button::with_label(\"Cancel\")"));
        assert!(open.contains("move |_| sender.input(WorkflowMsg::Close(opening.clone()))"));
        assert!(open.contains("move |_| sender.input(WorkflowMsg::Submit(opening.clone()))"));
        let closed = source.split("root.connect_closed({").nth(1).unwrap();
        assert!(
            closed
                .find("let current = opening.borrow().clone()")
                .unwrap()
                < closed
                    .find("sender.input(WorkflowMsg::Close(current))")
                    .unwrap()
        );
    }

    #[test]
    fn successful_workflow_submit_retires_before_output_but_errors_keep_the_draft() {
        let source = include_str!("workflow.rs")
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .unwrap();
        let submit = source.split("WorkflowMsg::Submit(_) => {").nth(1).unwrap();
        let success = submit
            .split("Ok(command) => {")
            .nth(1)
            .unwrap()
            .split("Err(error) => {")
            .next()
            .unwrap();
        assert!(
            success.find("self.opening.borrow_mut().take()").unwrap()
                < success.find("root.force_close()").unwrap()
        );
        assert!(
            success.find("self.form = None").unwrap()
                < success
                    .find("sender.output(WorkflowOutput::Command { pane_id, command })")
                    .unwrap()
        );
        let error = submit
            .split("Err(error) => {")
            .nth(1)
            .unwrap()
            .split("WorkflowMsg::Close(_) => {")
            .next()
            .unwrap();
        assert!(error.contains("widgets.error_label.set_visible(true)"));
        assert!(!error.contains(".take()"));
        assert!(!error.contains("self.form ="));
        let close = submit.split("WorkflowMsg::Close(_) => {").nth(1).unwrap();
        assert!(
            close.find("self.opening.borrow_mut().take()").unwrap()
                < close.find("root.force_close()").unwrap()
        );
        assert!(
            close.find("self.form = None").unwrap() < close.find("root.force_close()").unwrap()
        );
    }
}
