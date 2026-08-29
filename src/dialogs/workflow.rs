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

/// One row's initial state, taken from the form rather than re-derived from
/// the argument, so what the entry shows and what the form will render are the
/// same value.
#[derive(Debug)]
struct WorkflowArgInit {
    name: String,
    description: String,
    value: String,
}

#[derive(Debug)]
struct WorkflowArgRow {
    name: String,
    description: String,
    value: String,
}

#[derive(Debug)]
enum WorkflowArgOutput {
    Changed(String, String),
    Submit,
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
                connect_changed[sender, name = self.name.clone()] => move |entry| {
                    let _ = sender.output(WorkflowArgOutput::Changed(
                        name.clone(),
                        entry.text().to_string(),
                    ));
                },
                connect_activate[sender] => move |_| {
                    let _ = sender.output(WorkflowArgOutput::Submit);
                },
            },
        }
    }

    fn init_model(init: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self {
            name: init.name,
            description: init.description,
            value: init.value,
        }
    }
}

#[derive(Debug)]
pub(crate) enum WorkflowMsg {
    Open(Workflow),
    Changed(String, String),
    Submit,
    Close,
}

#[derive(Debug)]
pub(crate) enum WorkflowOutput {
    Command(String),
}

pub(crate) struct WorkflowModel {
    parent: adw::ApplicationWindow,
    form: Option<ArgsForm>,
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

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 6,
                        set_halign: gtk::Align::End,

                        gtk::Button {
                            set_label: "Cancel",
                            connect_clicked => WorkflowMsg::Close,
                        },

                        #[name(run_button)]
                        gtk::Button {
                            set_label: "Insert command",
                            add_css_class: "suggested-action",
                            connect_clicked => WorkflowMsg::Submit,
                        },
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
                    WorkflowArgOutput::Changed(name, value) => WorkflowMsg::Changed(name, value),
                    WorkflowArgOutput::Submit => WorkflowMsg::Submit,
                });
        let model = Self {
            parent,
            form: None,
            rows,
        };
        let args_box = model.rows.widget();
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match msg {
            WorkflowMsg::Open(workflow) => {
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
                root.present(Some(&self.parent));
                if self.rows.is_empty() {
                    widgets.run_button.grab_focus();
                }
            }
            WorkflowMsg::Changed(name, value) => {
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
            WorkflowMsg::Submit => {
                let Some(form) = self.form.as_ref() else {
                    return;
                };
                match form.render() {
                    Ok(command) => {
                        root.force_close();
                        let _ = sender.output(WorkflowOutput::Command(command));
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
            WorkflowMsg::Close => root.force_close(),
        }
    }
}

fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
