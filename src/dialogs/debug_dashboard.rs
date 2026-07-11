//! Relm4 component for the read-only diagnostics dashboard.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::prelude::*;

pub(crate) type DebugInfo = Vec<(String, Vec<(String, String)>)>;

#[derive(Debug)]
pub(crate) enum DebugDashboardMsg {
    Toggle(DebugInfo),
    Close,
}

pub(crate) struct DebugDashboardModel {
    parent: adw::ApplicationWindow,
}

#[relm4::component(pub(crate))]
impl Component for DebugDashboardModel {
    type Init = adw::ApplicationWindow;
    type Input = DebugDashboardMsg;
    type Output = ();
    type CommandOutput = ();

    view! {
        root = adw::Dialog {
            set_title: "Debug Dashboard",
            set_content_width: 480,
            set_content_height: 560,

            #[wrap(Some)]
            set_child = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},

                #[wrap(Some)]
                set_content = &gtk::ScrolledWindow {
                    set_hexpand: true,
                    set_vexpand: true,

                    #[name(content)]
                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 18,
                        set_margin_all: 12,
                    },
                },
            },

            add_controller = gtk::EventControllerKey {
                set_propagation_phase: gtk::PropagationPhase::Capture,
                connect_key_pressed[sender] => move |_, key, _, _| {
                    if matches!(key, gtk::gdk::Key::Escape | gtk::gdk::Key::F12) {
                        sender.input(DebugDashboardMsg::Close);
                        gtk::glib::Propagation::Stop
                    } else {
                        gtk::glib::Propagation::Proceed
                    }
                },
            },
        }
    }

    fn init(
        parent: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = Self { parent };
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        _sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match msg {
            DebugDashboardMsg::Toggle(info) => {
                if root.is_visible() {
                    root.force_close();
                    return;
                }
                while let Some(child) = widgets.content.first_child() {
                    widgets.content.remove(&child);
                }
                for (section, rows) in info {
                    let group = adw::PreferencesGroup::new();
                    group.set_title(&section);
                    for (key, value) in rows {
                        let row = adw::ActionRow::builder().title(key).build();
                        let value_label = gtk::Label::new(Some(&value));
                        value_label.add_css_class("dim-label");
                        value_label.set_selectable(true);
                        value_label.set_xalign(1.0);
                        row.add_suffix(&value_label);
                        group.add(&row);
                    }
                    widgets.content.append(&group);
                }
                root.present(Some(&self.parent));
            }
            DebugDashboardMsg::Close => root.force_close(),
        }
    }
}
