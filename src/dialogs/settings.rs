//! Relm4 component for the live settings dialog.
//!
//! Keeping the dialog's transient UI state and signal handling here means the
//! application model only consumes typed outputs.  GTK remains the rendering
//! backend, but it no longer owns application state through closure captures.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::prelude::*;

#[derive(Debug, Clone)]
pub(crate) struct SettingsValues {
    pub(crate) theme: u32,
    pub(crate) font: u32,
    pub(crate) font_size: f64,
    pub(crate) font_scale: f64,
    pub(crate) opacity: f64,
    pub(crate) scrollback: f64,
}

pub(crate) struct SettingsInit {
    pub(crate) theme_names: Vec<String>,
    pub(crate) font_names: Vec<String>,
    pub(crate) values: SettingsValues,
}

#[derive(Debug)]
pub(crate) enum SettingsMsg {
    Toggle(SettingsValues, adw::ApplicationWindow),
    Theme(u32),
    Font(u32),
    FontSize(f64),
    FontScale(f64),
    Opacity(f64),
    Scrollback(f64),
}

#[derive(Debug)]
pub(crate) enum SettingsOutput {
    Theme(usize),
    FontDesc(String),
    FontScale(f64),
    Opacity(f64),
    Scrollback(u32),
}

pub(crate) struct SettingsModel {
    theme_names: Vec<String>,
    font_names: Vec<String>,
    values: SettingsValues,
}

#[relm4::component(pub(crate))]
impl Component for SettingsModel {
    type Init = SettingsInit;
    type Input = SettingsMsg;
    type Output = SettingsOutput;
    type CommandOutput = ();

    view! {
        root = adw::PreferencesDialog {
            set_title: "Settings",

            add = &adw::PreferencesPage {
                adw::PreferencesGroup {
                    #[name(theme_row)]
                    adw::ComboRow {
                        set_title: "Theme",
                        set_model: Some(&gtk::StringList::new(
                            &model.theme_names.iter().map(String::as_str).collect::<Vec<_>>()
                        )),
                        set_selected: model.values.theme,
                        connect_selected_notify[sender] => move |row| {
                            sender.input(SettingsMsg::Theme(row.selected()));
                        },
                    },

                    #[name(font_row)]
                    adw::ComboRow {
                        set_title: "Font",
                        set_model: Some(&gtk::StringList::new(
                            &model.font_names.iter().map(String::as_str).collect::<Vec<_>>()
                        )),
                        set_selected: model.values.font,
                        connect_selected_notify[sender] => move |row| {
                            sender.input(SettingsMsg::Font(row.selected()));
                        },
                    },

                    #[name(font_size_row)]
                    adw::SpinRow::new(
                        Some(&gtk::Adjustment::new(
                            model.values.font_size, 6.0, 72.0, 1.0, 4.0, 0.0
                        )),
                        1.0,
                        0,
                    ) {
                        set_title: "Font Size",
                        connect_value_notify[sender] => move |row| {
                            sender.input(SettingsMsg::FontSize(row.value()));
                        },
                    },

                    #[name(font_scale_row)]
                    adw::SpinRow::new(
                        Some(&gtk::Adjustment::new(
                            model.values.font_scale, 0.1, 10.0, 0.025, 0.1, 0.0
                        )),
                        0.025,
                        3,
                    ) {
                        set_title: "Font Scale",
                        connect_value_notify[sender] => move |row| {
                            sender.input(SettingsMsg::FontScale(row.value()));
                        },
                    },

                    adw::ActionRow {
                        set_title: "Opacity",

                        #[name(opacity_scale)]
                        add_suffix = &gtk::Scale::with_range(
                            gtk::Orientation::Horizontal, 0.01, 1.0, 0.025
                        ) {
                            set_value: model.values.opacity,
                            set_hexpand: true,
                            set_size_request: (180, -1),
                            connect_value_changed[sender] => move |scale| {
                                sender.input(SettingsMsg::Opacity(scale.value()));
                            },
                        },
                    },

                    #[name(scrollback_row)]
                    adw::SpinRow::new(
                        Some(&gtk::Adjustment::new(
                            model.values.scrollback, 0.0, 1_000_000.0, 100.0, 1000.0, 0.0
                        )),
                        100.0,
                        0,
                    ) {
                        set_title: "Scrollback Lines",
                        connect_value_notify[sender] => move |row| {
                            sender.input(SettingsMsg::Scrollback(row.value()));
                        },
                    },
                },
            },
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = Self {
            theme_names: init.theme_names,
            font_names: init.font_names,
            values: init.values,
        };
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
            SettingsMsg::Toggle(values, parent) => {
                if root.is_visible() {
                    root.force_close();
                    return;
                }
                self.values = values;
                widgets.theme_row.set_selected(self.values.theme);
                widgets.font_row.set_selected(self.values.font);
                widgets.font_size_row.set_value(self.values.font_size);
                widgets.font_scale_row.set_value(self.values.font_scale);
                widgets.opacity_scale.set_value(self.values.opacity);
                widgets.scrollback_row.set_value(self.values.scrollback);
                root.present(Some(&parent));
            }
            SettingsMsg::Theme(index) => {
                self.values.theme = index;
                let _ = sender.output(SettingsOutput::Theme(index as usize));
            }
            SettingsMsg::Font(index) => {
                self.values.font = index;
                self.output_font(&sender);
            }
            SettingsMsg::FontSize(size) => {
                self.values.font_size = size;
                self.output_font(&sender);
            }
            SettingsMsg::FontScale(scale) => {
                self.values.font_scale = scale;
                let _ = sender.output(SettingsOutput::FontScale(scale));
            }
            SettingsMsg::Opacity(opacity) => {
                self.values.opacity = opacity;
                let _ = sender.output(SettingsOutput::Opacity(opacity));
            }
            SettingsMsg::Scrollback(lines) => {
                self.values.scrollback = lines;
                let _ = sender.output(SettingsOutput::Scrollback(lines as u32));
            }
        }
    }
}

impl SettingsModel {
    fn output_font(&self, sender: &ComponentSender<Self>) {
        let family = self
            .font_names
            .get(self.values.font as usize)
            .map(String::as_str)
            .unwrap_or("Monospace");
        let _ = sender.output(SettingsOutput::FontDesc(format!(
            "{family} {}",
            self.values.font_size as i32
        )));
    }
}
