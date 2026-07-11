//! Relm4 component for filling workflow parameters.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender, FactoryVecDeque};
use relm4::gtk;
use relm4::prelude::*;
use std::collections::HashMap;

use crate::workflows::{self, Workflow, WorkflowArg};

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
    type Init = WorkflowArg;
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

    fn init_model(arg: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self {
            name: arg.name,
            description: arg.description,
            value: arg.default.unwrap_or_default(),
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
    workflow: Option<Workflow>,
    values: HashMap<String, String>,
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
            workflow: None,
            values: HashMap::new(),
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

                self.values.clear();
                let mut rows = self.rows.guard();
                rows.clear();
                for arg in &workflow.args {
                    self.values
                        .insert(arg.name.clone(), arg.default.clone().unwrap_or_default());
                    rows.push_back(arg.clone());
                }
                drop(rows);
                self.workflow = Some(workflow);
                root.present(Some(&self.parent));
                if self.rows.is_empty() {
                    widgets.run_button.grab_focus();
                }
            }
            WorkflowMsg::Changed(name, value) => {
                self.values.insert(name, value);
            }
            WorkflowMsg::Submit => {
                let Some(workflow) = self.workflow.as_ref() else {
                    return;
                };
                match workflows::render(workflow, &self.values) {
                    Ok(command) => {
                        root.force_close();
                        let _ = sender.output(WorkflowOutput::Command(command));
                    }
                    Err(error) => {
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
