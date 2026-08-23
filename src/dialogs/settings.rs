//! Relm4 component for the live settings dialog.
//!
//! Keeping the dialog's transient UI state and signal handling here means the
//! application model only consumes typed outputs.  GTK remains the rendering
//! backend, but it no longer owns application state through closure captures.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::prelude::*;

use crate::config::{remote_text_is_safe, RemoteHost};

const EDIT_REMOTE_HOST_LABEL: &str = "Edit remote host";
const REMOVE_REMOTE_HOST_LABEL: &str = "Remove remote host";

#[derive(Debug, Clone)]
pub(crate) struct SettingsValues {
    pub(crate) theme: u32,
    pub(crate) font: u32,
    pub(crate) font_size: f64,
    pub(crate) font_scale: f64,
    pub(crate) opacity: f64,
    pub(crate) scrollback: f64,
    pub(crate) terminal_mode: u32,
    pub(crate) block_compact: bool,
    pub(crate) command_history: bool,
    pub(crate) ascii_organism_enabled: bool,
    /// 0 automatic, 1 full, 2 calm, 3 static.
    pub(crate) ascii_organism_motion: u32,
    pub(crate) ai_enabled: bool,
    pub(crate) ai_panel_visible: bool,
    pub(crate) ai_panel_width: f64,
    pub(crate) agent_enabled: bool,
    pub(crate) command_correction_enabled: bool,
    pub(crate) ai_provider: u32,
    pub(crate) ai_model: String,
    pub(crate) ai_base_url: String,
    /// TOML-configured key path (never the environment override); the write
    /// target when the API Key row stores a pasted key.
    pub(crate) ai_api_key_file: Option<String>,
    pub(crate) ai_max_tokens: f64,
    pub(crate) ai_redact_secrets: bool,
    pub(crate) ai_stream: bool,
    pub(crate) agent_max_turns: f64,
    pub(crate) safe_mode: bool,
    pub(crate) notifications: bool,
    pub(crate) remote_clipboard: bool,
    pub(crate) remote_hosts: Vec<RemoteHost>,
}

/// Unsubmitted host-form state, for a new entry or an existing one under edit.
/// Lives beside `SettingsValues` rather than inside it so the two construction
/// sites only carry persisted state.
#[derive(Debug, Default)]
struct RemoteDraft {
    name: String,
    host: String,
    user: String,
    docker: bool,
    /// Index into the deploy combo: 0 off, 1 persist, 2 incognito.
    deploy: u32,
}

impl RemoteDraft {
    /// The form as it should read while `host` is being edited. Only the fields
    /// the form owns are copied; `ssh_args`, `session`, `remote_shell`,
    /// `login_shell`, `multiplex` and `deploy_artifact` have no widget here and
    /// are carried over untouched when the edit is saved.
    fn from_host(host: &RemoteHost) -> Self {
        Self {
            name: host.name.clone(),
            host: host.host.clone(),
            user: host.user.clone().unwrap_or_default(),
            docker: host.docker,
            deploy: match host.deploy {
                jterm_core::jsh_remote::Deploy::Persist => 1,
                jterm_core::jsh_remote::Deploy::Incognito => 2,
                _ => 0,
            },
        }
    }
}

/// Which form row carries the validation error, so only that entry turns red.
#[derive(Clone, Copy, Debug)]
enum RemoteField {
    Form,
    Name,
    Host,
    User,
}

/// Widgets owned by the independent Add/Edit dialog. Keeping them together
/// lets the Relm4 update loop validate and focus fields without putting
/// application state in GTK signal closures.
struct RemoteDialogUi {
    dialog: adw::Dialog,
    name: adw::EntryRow,
    host: adw::EntryRow,
    user: adw::EntryRow,
    error: gtk::Label,
}

pub(crate) struct SettingsInit {
    pub(crate) theme_names: Vec<String>,
    pub(crate) font_names: Vec<String>,
    pub(crate) values: SettingsValues,
}

/// Build the font list around the description that is actually active.
///
/// Pango's generic `Monospace` family and configured fonts that are not
/// installed locally do not necessarily appear in `list_families()`. Keeping
/// the active family in the list prevents a size-only edit from silently
/// selecting whichever installed family happened to sort first.
pub(crate) fn font_choices(
    mut available_families: Vec<String>,
    current_family: &str,
) -> (Vec<String>, u32) {
    let current_family = match current_family.trim() {
        "" => "Monospace",
        family => family,
    };
    available_families.retain(|family| !family.trim().is_empty());
    if !available_families
        .iter()
        .any(|family| family.eq_ignore_ascii_case(current_family))
    {
        available_families.push(current_family.to_string());
    }
    available_families.sort_by_cached_key(|family| family.to_lowercase());
    available_families.dedup_by(|left, right| left.eq_ignore_ascii_case(right));

    let selected = available_families
        .iter()
        .position(|family| family.eq_ignore_ascii_case(current_family))
        .expect("the active font family was inserted above") as u32;
    (available_families, selected)
}

fn font_desc_for_choice(font_names: &[String], selected: u32, size: f64) -> String {
    let family = font_names
        .get(selected as usize)
        .map(String::as_str)
        .unwrap_or("Monospace");
    format!("{family} {}", size as i32)
}

#[derive(Debug)]
pub(crate) enum SettingsMsg {
    Toggle(SettingsValues, Vec<String>, adw::ApplicationWindow),
    Theme(u32),
    Font(u32),
    FontSize(f64),
    FontScale(f64),
    Opacity(f64),
    Scrollback(f64),
    TerminalMode(u32),
    BlockCompact(bool),
    CommandHistory(bool),
    AsciiOrganism(bool),
    AsciiOrganismMotion(u32),
    AiEnabled(bool),
    AiPanelVisible(bool),
    AiPanelWidth(f64),
    AgentEnabled(bool),
    CommandCorrection(bool),
    AiProvider(u32),
    AiModel(String),
    AiBaseUrl(String),
    AiApiKeyStore(String),
    AiMaxTokens(f64),
    AiRedactSecrets(bool),
    AiStream(bool),
    AgentMaxTurns(f64),
    Notifications(bool),
    RemoteClipboard(bool),
    RemoteHostName(String),
    RemoteHostHost(String),
    RemoteHostUser(String),
    RemoteHostDocker(bool),
    RemoteHostDeploy(u32),
    RemoteHostOpenAdd,
    /// Commit the independent dialog: append or replace in place.
    RemoteHostSave,
    /// Load an existing host into the form instead of starting a new one.
    RemoteHostEdit(usize),
    /// Abandon an in-progress edit and leave the saved host as it was.
    RemoteHostCancel,
    RemoteHostRemove(usize),
    RemoteHostRemoveConfirmed {
        index: usize,
        name: String,
    },
}

#[derive(Debug)]
pub(crate) enum SettingsOutput {
    Theme(usize),
    FontDesc(String),
    FontScale(f64),
    Opacity(f64),
    Scrollback(u32),
    TerminalMode(usize),
    BlockCompact(bool),
    CommandHistory(bool),
    AsciiOrganism(bool),
    AsciiOrganismMotion(u32),
    AiEnabled(bool),
    AiPanelVisible(bool),
    AiPanelWidth(u32),
    AgentEnabled(bool),
    CommandCorrection(bool),
    AiProvider(usize),
    AiModel(String),
    AiBaseUrl(String),
    /// A key was stored into this path; the app records and persists it.
    AiApiKeyFile(String),
    AiMaxTokens(u32),
    AiRedactSecrets(bool),
    AiStream(bool),
    AgentMaxTurns(u32),
    Notifications(bool),
    RemoteClipboard(bool),
    /// The full list after any add or remove; the app replaces and persists it.
    RemoteHosts(Vec<RemoteHost>),
}

pub(crate) struct SettingsModel {
    theme_names: Vec<String>,
    font_names: Vec<String>,
    values: SettingsValues,
    remote_draft: RemoteDraft,
    /// Index of the saved host the form is editing; `None` while the form is
    /// composing a new one.
    remote_editing: Option<usize>,
    remote_dialog: Option<RemoteDialogUi>,
    /// Host rows currently added to the "Remote Hosts" group. The view! macro
    /// cannot express a dynamic list, so these are rebuilt imperatively.
    remote_rows: Vec<adw::ActionRow>,
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
                    set_title: "Appearance",

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
                            set_draw_value: true,
                            set_value_pos: gtk::PositionType::Left,
                            set_format_value_func: |_, value| format!("{:.0}%", value * 100.0),
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

                adw::PreferencesGroup {
                    set_title: &gtk::glib::markup_escape_text("Terminal & Blocks"),

                    #[name(terminal_mode_row)]
                    adw::ComboRow {
                        set_title: "Terminal Backend",
                        set_subtitle: "Applies to new and restored local panes",
                        set_model: Some(&gtk::StringList::new(&[
                            "Block",
                            "VTE compatibility",
                            "Unified (experimental)",
                        ])),
                        set_selected: model.values.terminal_mode,
                        set_sensitive: !model.values.safe_mode,
                        connect_selected_notify[sender] => move |row| {
                            sender.input(SettingsMsg::TerminalMode(row.selected()));
                        },
                    },

                    #[name(block_compact_row)]
                    adw::SwitchRow {
                        set_title: "Compact Block Layout",
                        set_subtitle: "Denser spacing for blocks and the input cell",
                        set_active: model.values.block_compact,
                        set_sensitive: !model.values.safe_mode,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::BlockCompact(row.is_active()));
                        },
                    },

                    #[name(command_history_row)]
                    adw::SwitchRow {
                        set_title: "Command History Index",
                        set_subtitle: "Store commands, cwd and status; never terminal output",
                        set_active: model.values.command_history,
                        set_sensitive: !model.values.safe_mode,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::CommandHistory(row.is_active()));
                        },
                    },

                    #[name(ascii_organism_row)]
                    adw::SwitchRow {
                        set_title: "ASCII Organism",
                        set_subtitle: "Show the local, no-LLM organism in new Block panes",
                        set_active: model.values.ascii_organism_enabled,
                        set_sensitive: !model.values.safe_mode,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AsciiOrganism(row.is_active()));
                        },
                    },

                    #[name(ascii_organism_motion_row)]
                    adw::ComboRow {
                        set_title: "Organism Motion",
                        set_subtitle: "Automatic follows the desktop animation preference",
                        set_model: Some(&gtk::StringList::new(
                            &["Automatic", "Full", "Calm", "Static"]
                        )),
                        set_selected: model.values.ascii_organism_motion,
                        set_sensitive: !model.values.safe_mode
                            && model.values.ascii_organism_enabled,
                        connect_selected_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AsciiOrganismMotion(row.selected()));
                        },
                    },
                },

                adw::PreferencesGroup {
                    set_title: &gtk::glib::markup_escape_text("Features & Privacy"),

                    #[name(notifications_row)]
                    adw::SwitchRow {
                        set_title: "Long-command Notifications",
                        set_active: model.values.notifications,
                        set_sensitive: !model.values.safe_mode,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::Notifications(row.is_active()));
                        },
                    },

                    #[name(remote_clipboard_row)]
                    adw::SwitchRow {
                        set_title: "Allow OSC 52 Clipboard Writes",
                        set_subtitle: "Enable only for trusted local and remote programs",
                        set_active: model.values.remote_clipboard,
                        set_sensitive: !model.values.safe_mode,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::RemoteClipboard(row.is_active()));
                        },
                    },
                },

                adw::PreferencesGroup {
                    set_title: &gtk::glib::markup_escape_text("AI & Agent"),
                    set_description: Some(
                        "Environment variables take priority. Keys entered here are stored in a private ai.key file, never in config.toml"
                    ),

                    #[name(ai_enabled_row)]
                    adw::SwitchRow {
                        set_title: "Enable AI Features",
                        set_active: model.values.ai_enabled,
                        set_sensitive: !model.values.safe_mode,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiEnabled(row.is_active()));
                        },
                    },

                    #[name(ai_panel_visible_row)]
                    adw::SwitchRow {
                        set_title: "Show AI Chats at Startup",
                        set_subtitle: "Keep the persistent right-side chat panel open",
                        set_active: model.values.ai_panel_visible,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiPanelVisible(row.is_active()));
                        },
                    },

                    #[name(ai_panel_width_row)]
                    adw::SpinRow::new(
                        Some(&gtk::Adjustment::new(
                            model.values.ai_panel_width, 240.0, 1_200.0, 10.0, 50.0, 0.0
                        )),
                        10.0,
                        0,
                    ) {
                        set_title: "AI Chats Width",
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_value_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiPanelWidth(row.value()));
                        },
                    },

                    #[name(agent_enabled_row)]
                    adw::SwitchRow {
                        set_title: "Enable Approval-gated Agent",
                        set_subtitle: "Every proposed command remains editable and requires approval",
                        set_active: model.values.agent_enabled,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AgentEnabled(row.is_active()));
                        },
                    },

                    adw::SwitchRow {
                        set_title: "Automatic Agent Execution Retired",
                        set_subtitle: "Every proposal requires explicit approval; command text cannot prove what aliases, helpers, or flags will execute",
                        set_active: false,
                        set_sensitive: false,
                    },

                    #[name(command_correction_row)]
                    adw::SwitchRow {
                        set_title: "Correct Mistyped Block Commands",
                        set_subtitle: "Offer an editable correction after typo-like failures; never run automatically",
                        set_active: model.values.command_correction_enabled,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::CommandCorrection(row.is_active()));
                        },
                    },

                    #[name(ai_provider_row)]
                    adw::ComboRow {
                        set_title: "Provider",
                        set_model: Some(&gtk::StringList::new(
                            &["Anthropic", "OpenAI-compatible", "Ollama"]
                        )),
                        set_selected: model.values.ai_provider,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_selected_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiProvider(row.selected()));
                        },
                    },

                    #[name(ai_model_row)]
                    adw::EntryRow {
                        set_title: "Model",
                        set_text: &model.values.ai_model,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_changed[sender] => move |row| {
                            sender.input(SettingsMsg::AiModel(row.text().to_string()));
                        },
                    },

                    #[name(ai_base_url_row)]
                    adw::EntryRow {
                        set_title: "Base URL",
                        set_text: &model.values.ai_base_url,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_changed[sender] => move |row| {
                            sender.input(SettingsMsg::AiBaseUrl(row.text().to_string()));
                        },
                    },

                    #[name(ai_api_key_row)]
                    adw::PasswordEntryRow {
                        set_title: "API Key — enter a new value and press Apply",
                        set_show_apply_button: true,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_apply[sender] => move |row| {
                            sender.input(SettingsMsg::AiApiKeyStore(row.text().to_string()));
                        },
                    },

                    #[name(ai_max_tokens_row)]
                    adw::SpinRow::new(
                        Some(&gtk::Adjustment::new(
                            model.values.ai_max_tokens, 64.0, 32_768.0, 64.0, 512.0, 0.0
                        )),
                        64.0,
                        0,
                    ) {
                        set_title: "Maximum Response Tokens",
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_value_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiMaxTokens(row.value()));
                        },
                    },

                    #[name(agent_max_turns_row)]
                    adw::SpinRow::new(
                        Some(&gtk::Adjustment::new(
                            model.values.agent_max_turns, 1.0, 100.0, 1.0, 5.0, 0.0
                        )),
                        1.0,
                        0,
                    ) {
                        set_title: "Agent Turn Limit",
                        set_sensitive: !model.values.safe_mode
                            && model.values.ai_enabled
                            && model.values.agent_enabled,
                        connect_value_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AgentMaxTurns(row.value()));
                        },
                    },

                    #[name(ai_stream_row)]
                    adw::SwitchRow {
                        set_title: "Stream Chat Responses",
                        set_subtitle: "Show AI chat replies incrementally while they are generated",
                        set_active: model.values.ai_stream,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiStream(row.is_active()));
                        },
                    },

                    #[name(ai_redact_secrets_row)]
                    adw::SwitchRow {
                        set_title: "Redact Common Secrets",
                        set_subtitle: "Apply before terminal context is sent to a provider",
                        set_active: model.values.ai_redact_secrets,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiRedactSecrets(row.is_active()));
                        },
                    },

                },

                // Host rows are managed imperatively (`rebuild_remote_rows`):
                // the view! macro cannot express a list that grows and shrinks.
                #[name(remote_hosts_group)]
                adw::PreferencesGroup {
                    set_title: "Remote Hosts",
                    set_description: Some(
                        "Targets for the Ctrl+Shift+S picker. Advanced fields (ssh_args, session, deploy_artifact) are edited in config.toml"
                    ),
                    set_sensitive: !model.values.safe_mode,

                    #[wrap(Some)]
                    set_header_suffix = &gtk::Button {
                        set_icon_name: "list-add-symbolic",
                        add_css_class: "flat",
                        set_valign: gtk::Align::Center,
                        set_tooltip_text: Some("Add Remote Host"),
                        update_property: &[gtk::accessible::Property::Label("Add Remote Host")],
                        connect_clicked => SettingsMsg::RemoteHostOpenAdd,
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
        let mut model = Self {
            theme_names: init.theme_names,
            font_names: init.font_names,
            values: init.values,
            remote_draft: RemoteDraft::default(),
            remote_editing: None,
            remote_dialog: None,
            remote_rows: Vec::new(),
        };
        let widgets = view_output!();
        model.rebuild_remote_rows(&widgets.remote_hosts_group, &sender);
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
            SettingsMsg::Toggle(values, font_names, parent) => {
                if root.parent().is_some() {
                    root.force_close();
                    return;
                }
                self.values = values;
                self.font_names = font_names;
                let font_notify_guard = widgets.font_row.freeze_notify();
                widgets.font_row.set_model(Some(&gtk::StringList::new(
                    &self
                        .font_names
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                )));
                widgets.theme_row.set_selected(self.values.theme);
                widgets.font_row.set_selected(self.values.font);
                drop(font_notify_guard);
                widgets.font_size_row.set_value(self.values.font_size);
                widgets.font_scale_row.set_value(self.values.font_scale);
                widgets.opacity_scale.set_value(self.values.opacity);
                widgets.scrollback_row.set_value(self.values.scrollback);
                widgets
                    .terminal_mode_row
                    .set_selected(self.values.terminal_mode);
                widgets
                    .terminal_mode_row
                    .set_sensitive(!self.values.safe_mode);
                widgets
                    .block_compact_row
                    .set_active(self.values.block_compact);
                widgets
                    .block_compact_row
                    .set_sensitive(!self.values.safe_mode);
                widgets
                    .command_history_row
                    .set_active(self.values.command_history);
                widgets
                    .command_history_row
                    .set_sensitive(!self.values.safe_mode);
                widgets
                    .ascii_organism_row
                    .set_active(self.values.ascii_organism_enabled);
                widgets
                    .ascii_organism_motion_row
                    .set_selected(self.values.ascii_organism_motion);
                widgets
                    .ascii_organism_row
                    .set_sensitive(!self.values.safe_mode);
                widgets
                    .ascii_organism_motion_row
                    .set_sensitive(!self.values.safe_mode && self.values.ascii_organism_enabled);
                widgets.ai_enabled_row.set_active(self.values.ai_enabled);
                widgets
                    .ai_panel_visible_row
                    .set_active(self.values.ai_panel_visible);
                widgets
                    .ai_panel_width_row
                    .set_value(self.values.ai_panel_width);
                widgets
                    .agent_enabled_row
                    .set_active(self.values.agent_enabled);
                widgets
                    .command_correction_row
                    .set_active(self.values.command_correction_enabled);
                widgets
                    .ai_provider_row
                    .set_selected(self.values.ai_provider);
                widgets.ai_model_row.set_text(&self.values.ai_model);
                widgets.ai_base_url_row.set_text(&self.values.ai_base_url);
                widgets.ai_api_key_row.set_text("");
                widgets
                    .ai_api_key_row
                    .set_title("API Key — enter a new value and press Apply");
                widgets
                    .ai_max_tokens_row
                    .set_value(self.values.ai_max_tokens);
                widgets
                    .ai_redact_secrets_row
                    .set_active(self.values.ai_redact_secrets);
                widgets.ai_stream_row.set_active(self.values.ai_stream);
                widgets
                    .agent_max_turns_row
                    .set_value(self.values.agent_max_turns);
                let ai_sensitive = !self.values.safe_mode && self.values.ai_enabled;
                widgets.ai_enabled_row.set_sensitive(!self.values.safe_mode);
                widgets.ai_panel_visible_row.set_sensitive(ai_sensitive);
                widgets.ai_panel_width_row.set_sensitive(ai_sensitive);
                widgets.agent_enabled_row.set_sensitive(ai_sensitive);
                widgets.command_correction_row.set_sensitive(ai_sensitive);
                widgets.ai_provider_row.set_sensitive(ai_sensitive);
                widgets.ai_model_row.set_sensitive(ai_sensitive);
                widgets.ai_base_url_row.set_sensitive(ai_sensitive);
                widgets.ai_api_key_row.set_sensitive(ai_sensitive);
                widgets.ai_max_tokens_row.set_sensitive(ai_sensitive);
                widgets.ai_redact_secrets_row.set_sensitive(ai_sensitive);
                widgets.ai_stream_row.set_sensitive(ai_sensitive);
                widgets
                    .agent_max_turns_row
                    .set_sensitive(ai_sensitive && self.values.agent_enabled);
                widgets
                    .notifications_row
                    .set_active(self.values.notifications);
                widgets
                    .notifications_row
                    .set_sensitive(!self.values.safe_mode);
                widgets
                    .remote_clipboard_row
                    .set_active(self.values.remote_clipboard);
                widgets
                    .remote_clipboard_row
                    .set_sensitive(!self.values.safe_mode);
                // A reopened dialog starts on a fresh host: the index an edit
                // was holding may not survive whatever changed the list while
                // the dialog was closed.
                if let Some(ui) = self.remote_dialog.take() {
                    ui.dialog.close();
                }
                self.remote_editing = None;
                self.remote_draft = RemoteDraft::default();
                self.rebuild_remote_rows(&widgets.remote_hosts_group, &sender);
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
            SettingsMsg::TerminalMode(mode) => {
                self.values.terminal_mode = mode;
                let _ = sender.output(SettingsOutput::TerminalMode(mode as usize));
            }
            SettingsMsg::BlockCompact(enabled) => {
                self.values.block_compact = enabled;
                let _ = sender.output(SettingsOutput::BlockCompact(enabled));
            }
            SettingsMsg::CommandHistory(enabled) => {
                self.values.command_history = enabled;
                let _ = sender.output(SettingsOutput::CommandHistory(enabled));
            }
            SettingsMsg::AsciiOrganism(enabled) => {
                self.values.ascii_organism_enabled = enabled;
                widgets
                    .ascii_organism_motion_row
                    .set_sensitive(!self.values.safe_mode && enabled);
                let _ = sender.output(SettingsOutput::AsciiOrganism(enabled));
            }
            SettingsMsg::AsciiOrganismMotion(motion) => {
                self.values.ascii_organism_motion = motion;
                let _ = sender.output(SettingsOutput::AsciiOrganismMotion(motion));
            }
            SettingsMsg::AiEnabled(enabled) => {
                self.values.ai_enabled = enabled;
                let sensitive = !self.values.safe_mode && enabled;
                widgets.agent_enabled_row.set_sensitive(sensitive);
                widgets.ai_panel_visible_row.set_sensitive(sensitive);
                widgets.ai_panel_width_row.set_sensitive(sensitive);
                widgets.command_correction_row.set_sensitive(sensitive);
                widgets.ai_provider_row.set_sensitive(sensitive);
                widgets.ai_model_row.set_sensitive(sensitive);
                widgets.ai_base_url_row.set_sensitive(sensitive);
                widgets.ai_api_key_row.set_sensitive(sensitive);
                widgets.ai_max_tokens_row.set_sensitive(sensitive);
                widgets.ai_redact_secrets_row.set_sensitive(sensitive);
                widgets.ai_stream_row.set_sensitive(sensitive);
                widgets
                    .agent_max_turns_row
                    .set_sensitive(sensitive && self.values.agent_enabled);
                let _ = sender.output(SettingsOutput::AiEnabled(enabled));
            }
            SettingsMsg::AiPanelVisible(visible) => {
                self.values.ai_panel_visible = visible;
                let _ = sender.output(SettingsOutput::AiPanelVisible(visible));
            }
            SettingsMsg::AiPanelWidth(width) => {
                self.values.ai_panel_width = width;
                let _ = sender.output(SettingsOutput::AiPanelWidth(width as u32));
            }
            SettingsMsg::AgentEnabled(enabled) => {
                self.values.agent_enabled = enabled;
                widgets
                    .agent_max_turns_row
                    .set_sensitive(!self.values.safe_mode && self.values.ai_enabled && enabled);
                let _ = sender.output(SettingsOutput::AgentEnabled(enabled));
            }
            SettingsMsg::CommandCorrection(enabled) => {
                self.values.command_correction_enabled = enabled;
                let _ = sender.output(SettingsOutput::CommandCorrection(enabled));
            }
            SettingsMsg::AiProvider(provider) => {
                self.values.ai_provider = provider;
                let _ = sender.output(SettingsOutput::AiProvider(provider as usize));
            }
            SettingsMsg::AiModel(model) => {
                self.values.ai_model = model.clone();
                let _ = sender.output(SettingsOutput::AiModel(model));
            }
            SettingsMsg::AiBaseUrl(base_url) => {
                self.values.ai_base_url = base_url.clone();
                let _ = sender.output(SettingsOutput::AiBaseUrl(base_url));
            }
            SettingsMsg::AiApiKeyStore(key) => {
                // Same write-target rule as the rest of the family: the
                // configured path, else the per-app default. The environment
                // override stays read-only and is never written to.
                let path = self
                    .values
                    .ai_api_key_file
                    .clone()
                    .unwrap_or_else(jterm_core::ai::default_api_key_path);
                match jterm_core::ai::write_api_key_file(&path, &key) {
                    Ok(()) => {
                        widgets.ai_api_key_row.set_text("");
                        widgets
                            .ai_api_key_row
                            .set_title("API Key stored — enter a new value to replace it");
                        self.values.ai_api_key_file = Some(path.clone());
                        let _ = sender.output(SettingsOutput::AiApiKeyFile(path));
                    }
                    Err(error) => {
                        widgets
                            .ai_api_key_row
                            .set_title(&format!("API Key not saved: {error}"));
                    }
                }
            }
            SettingsMsg::AiMaxTokens(max_tokens) => {
                self.values.ai_max_tokens = max_tokens;
                let _ = sender.output(SettingsOutput::AiMaxTokens(max_tokens as u32));
            }
            SettingsMsg::AiRedactSecrets(enabled) => {
                self.values.ai_redact_secrets = enabled;
                let _ = sender.output(SettingsOutput::AiRedactSecrets(enabled));
            }
            SettingsMsg::AiStream(enabled) => {
                self.values.ai_stream = enabled;
                let _ = sender.output(SettingsOutput::AiStream(enabled));
            }
            SettingsMsg::AgentMaxTurns(turns) => {
                self.values.agent_max_turns = turns;
                let _ = sender.output(SettingsOutput::AgentMaxTurns(turns as u32));
            }
            SettingsMsg::Notifications(enabled) => {
                self.values.notifications = enabled;
                let _ = sender.output(SettingsOutput::Notifications(enabled));
            }
            SettingsMsg::RemoteClipboard(enabled) => {
                self.values.remote_clipboard = enabled;
                let _ = sender.output(SettingsOutput::RemoteClipboard(enabled));
            }
            SettingsMsg::RemoteHostName(name) => self.remote_draft.name = name,
            SettingsMsg::RemoteHostHost(host) => self.remote_draft.host = host,
            SettingsMsg::RemoteHostUser(user) => self.remote_draft.user = user,
            SettingsMsg::RemoteHostDocker(docker) => self.remote_draft.docker = docker,
            SettingsMsg::RemoteHostDeploy(mode) => self.remote_draft.deploy = mode,
            SettingsMsg::RemoteHostOpenAdd => {
                self.present_remote_host_dialog(None, root, &sender);
            }
            SettingsMsg::RemoteHostSave => {
                self.clear_remote_errors();
                match self.validate_remote_draft() {
                    Ok(host) => {
                        match self.remote_editing {
                            // Replace in place so the host keeps its position in
                            // the picker; a remove-then-push would move it to the
                            // end on every edit.
                            Some(index) if index < self.values.remote_hosts.len() => {
                                self.values.remote_hosts[index] = host;
                            }
                            _ => self.values.remote_hosts.push(host),
                        }
                        if let Some(ui) = self.remote_dialog.take() {
                            ui.dialog.close();
                        }
                        self.remote_editing = None;
                        self.remote_draft = RemoteDraft::default();
                        self.rebuild_remote_rows(&widgets.remote_hosts_group, &sender);
                        let _ = sender.output(SettingsOutput::RemoteHosts(
                            self.values.remote_hosts.clone(),
                        ));
                    }
                    Err((field, message)) => {
                        self.show_remote_error(field, message);
                    }
                }
            }
            SettingsMsg::RemoteHostEdit(index) => {
                self.present_remote_host_dialog(Some(index), root, &sender);
            }
            SettingsMsg::RemoteHostCancel => {
                if let Some(ui) = self.remote_dialog.take() {
                    ui.dialog.close();
                }
                self.remote_editing = None;
                self.remote_draft = RemoteDraft::default();
            }
            SettingsMsg::RemoteHostRemove(index) => {
                if let Some(host) = self.values.remote_hosts.get(index) {
                    let name = host.name.clone();
                    let display = crate::review_input::safe_inline_display(&name, 1_024);
                    let dialog = adw::AlertDialog::new(
                        Some("Remove this host?"),
                        Some(&format!(
                            "“{display}” will be removed from config.toml. Nothing on the destination is touched."
                        )),
                    );
                    dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
                    dialog.set_default_response(Some("cancel"));
                    dialog.set_close_response("cancel");
                    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                    let sender = sender.clone();
                    dialog.connect_response(None, move |_, response| {
                        if response == "remove" {
                            sender.input(SettingsMsg::RemoteHostRemoveConfirmed {
                                index,
                                name: name.clone(),
                            });
                        }
                    });
                    dialog.present(Some(root));
                }
            }
            SettingsMsg::RemoteHostRemoveConfirmed { index, name } => {
                let removed = remove_remote_host(&mut self.values.remote_hosts, index, &name);
                if removed {
                    self.rebuild_remote_rows(&widgets.remote_hosts_group, &sender);
                    let _ = sender.output(SettingsOutput::RemoteHosts(
                        self.values.remote_hosts.clone(),
                    ));
                }
            }
        }
    }
}

impl SettingsModel {
    /// Drop the red outline left by the previous failed submit, so an error is
    /// only ever pointing at the field the user is being told about now.
    fn clear_remote_errors(&self) {
        let Some(ui) = self.remote_dialog.as_ref() else {
            return;
        };
        for row in [&ui.name, &ui.host, &ui.user] {
            row.remove_css_class("error");
        }
        ui.error.set_visible(false);
    }

    fn show_remote_error(&self, field: RemoteField, message: &str) {
        let Some(ui) = self.remote_dialog.as_ref() else {
            return;
        };
        let row = match field {
            RemoteField::Form => None,
            RemoteField::Name => Some(&ui.name),
            RemoteField::Host => Some(&ui.host),
            RemoteField::User => Some(&ui.user),
        };
        if let Some(row) = row {
            row.add_css_class("error");
            row.grab_focus();
        }
        ui.error.set_label(message);
        ui.error.set_visible(true);
    }

    fn present_remote_host_dialog(
        &mut self,
        editing: Option<usize>,
        parent: &adw::PreferencesDialog,
        sender: &ComponentSender<Self>,
    ) {
        if let Some(ui) = self.remote_dialog.take() {
            ui.dialog.close();
        }
        let existing = editing.and_then(|index| {
            self.values
                .remote_hosts
                .get(index)
                .cloned()
                .map(|host| (index, host))
        });
        if editing.is_some() && existing.is_none() {
            return;
        }
        self.remote_editing = existing.as_ref().map(|(index, _)| *index);
        self.remote_draft = existing
            .as_ref()
            .map(|(_, host)| RemoteDraft::from_host(host))
            .unwrap_or_default();

        let dialog = adw::Dialog::builder()
            .title(if existing.is_some() {
                "Edit Remote Host"
            } else {
                "Add Remote Host"
            })
            .content_width(420)
            .build();
        let name = adw::EntryRow::new();
        name.set_title("Name (optional)");
        name.set_text(&self.remote_draft.name);
        let host = adw::EntryRow::new();
        host.set_title("Host / container");
        host.set_text(&self.remote_draft.host);
        let user = adw::EntryRow::new();
        user.set_title("User (optional)");
        user.set_text(&self.remote_draft.user);
        let docker = adw::SwitchRow::builder()
            .title("Docker Container")
            .subtitle("Attach to a running container with docker exec instead of ssh")
            .active(self.remote_draft.docker)
            .build();
        let deploy_model = gtk::StringList::new(&["Off", "Persist", "Incognito"]);
        let deploy = adw::ComboRow::builder()
            .title("Deploy jsh")
            .subtitle("Put a jsh on the destination for the life of the session")
            .model(&deploy_model)
            .selected(self.remote_draft.deploy)
            .build();

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("boxed-list");
        list.append(&name);
        list.append(&host);
        list.append(&user);
        list.append(&docker);
        list.append(&deploy);

        let error = gtk::Label::new(None);
        error.add_css_class("error");
        error.set_wrap(true);
        error.set_xalign(0.0);
        error.set_visible(false);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_all(12);
        content.append(&list);
        if let Some((_, existing)) = existing.as_ref() {
            if let Some(note) = advanced_fields_note(existing) {
                let note = crate::review_input::safe_inline_display(&note, 4 * 1024);
                let label = gtk::Label::new(Some(&note));
                label.add_css_class("dim-label");
                label.set_wrap(true);
                label.set_xalign(0.0);
                content.append(&label);
            }
        }
        content.append(&error);

        let header = adw::HeaderBar::new();
        header.set_show_start_title_buttons(false);
        header.set_show_end_title_buttons(false);
        let cancel = gtk::Button::with_label("Cancel");
        let save = gtk::Button::with_label(if existing.is_some() { "Save" } else { "Add" });
        save.add_css_class("suggested-action");
        header.pack_start(&cancel);
        header.pack_end(&save);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));
        dialog.set_child(Some(&toolbar));

        {
            let sender = sender.clone();
            name.connect_changed(move |row| {
                sender.input(SettingsMsg::RemoteHostName(row.text().to_string()));
            });
        }
        {
            let sender = sender.clone();
            host.connect_changed(move |row| {
                sender.input(SettingsMsg::RemoteHostHost(row.text().to_string()));
            });
        }
        {
            let sender = sender.clone();
            user.connect_changed(move |row| {
                sender.input(SettingsMsg::RemoteHostUser(row.text().to_string()));
            });
        }
        {
            let sender = sender.clone();
            docker.connect_active_notify(move |row| {
                sender.input(SettingsMsg::RemoteHostDocker(row.is_active()));
            });
        }
        {
            let sender = sender.clone();
            deploy.connect_selected_notify(move |row| {
                sender.input(SettingsMsg::RemoteHostDeploy(row.selected()));
            });
        }
        {
            let sender = sender.clone();
            cancel.connect_clicked(move |_| sender.input(SettingsMsg::RemoteHostCancel));
        }
        {
            let sender = sender.clone();
            save.connect_clicked(move |_| sender.input(SettingsMsg::RemoteHostSave));
        }
        {
            let sender = sender.clone();
            dialog.connect_closed(move |_| sender.input(SettingsMsg::RemoteHostCancel));
        }

        self.remote_dialog = Some(RemoteDialogUi {
            dialog: dialog.clone(),
            name,
            host: host.clone(),
            user,
            error,
        });
        dialog.present(Some(parent));
        host.grab_focus();
    }

    fn rebuild_remote_rows(
        &mut self,
        group: &adw::PreferencesGroup,
        sender: &ComponentSender<Self>,
    ) {
        for row in self.remote_rows.drain(..) {
            group.remove(&row);
        }
        if self.values.remote_hosts.is_empty() {
            let row = adw::ActionRow::new();
            row.set_title("No remote hosts configured");
            row.set_subtitle("Add an ssh destination or a running container");
            row.set_sensitive(false);
            group.add(&row);
            self.remote_rows.push(row);
            return;
        }
        for (index, host) in self.values.remote_hosts.iter().enumerate() {
            let row = adw::ActionRow::new();
            row.set_use_markup(false);
            let title = if host.name.is_empty() {
                &host.host
            } else {
                &host.name
            };
            row.set_title(&crate::review_input::safe_inline_display(title, 1_024));
            let target = match &host.user {
                Some(user) => format!("{user}@{}", host.host),
                None => host.host.clone(),
            };
            let transport = if host.docker { "docker" } else { "ssh" };
            let mut subtitle = format!("{transport} · {target} · deploy {}", host.deploy.as_str());
            // The form has no widget for these, so say they are there rather
            // than let an edit look like it silently dropped them.
            if !host.ssh_args.is_empty() {
                subtitle.push_str(&format!(" · ssh_args {}", host.ssh_args.join(" ")));
            }
            row.set_subtitle(&crate::review_input::safe_inline_display(
                &subtitle,
                4 * 1024,
            ));
            let edit = gtk::Button::from_icon_name("document-edit-symbolic");
            edit.set_valign(gtk::Align::Center);
            edit.add_css_class("flat");
            edit.set_tooltip_text(Some("Edit Host"));
            edit.update_property(&[gtk::accessible::Property::Label(EDIT_REMOTE_HOST_LABEL)]);
            edit.connect_clicked({
                let sender = sender.clone();
                move |_| sender.input(SettingsMsg::RemoteHostEdit(index))
            });
            row.add_suffix(&edit);
            let remove = gtk::Button::from_icon_name("user-trash-symbolic");
            remove.set_valign(gtk::Align::Center);
            remove.add_css_class("flat");
            remove.add_css_class("destructive-action");
            remove.set_tooltip_text(Some("Remove Host"));
            remove.update_property(&[gtk::accessible::Property::Label(REMOVE_REMOTE_HOST_LABEL)]);
            remove.connect_clicked({
                let sender = sender.clone();
                move |_| sender.input(SettingsMsg::RemoteHostRemove(index))
            });
            row.add_suffix(&remove);
            group.add(&row);
            self.remote_rows.push(row);
        }
    }

    /// Mirror `parse_remote_hosts`' acceptance rules so a host added here
    /// always survives the next config load.
    fn validate_remote_draft(&self) -> Result<RemoteHost, (RemoteField, &'static str)> {
        if self.remote_editing.is_none()
            && self.values.remote_hosts.len() >= crate::config::MAX_REMOTE_HOSTS
        {
            return Err((RemoteField::Form, "The remote host limit is reached."));
        }
        let host = self.remote_draft.host.trim().to_string();
        if host.is_empty() {
            return Err((RemoteField::Host, "Host is required."));
        }
        // ssh and docker would both read a leading dash as an option.
        if host.starts_with('-') {
            return Err((RemoteField::Host, "Host must not start with \"-\"."));
        }
        if !remote_text_is_safe(&host, false, 1_024) {
            return Err((
                RemoteField::Host,
                "Host must not contain whitespace or control characters.",
            ));
        }
        let name = match self.remote_draft.name.trim() {
            "" => host.clone(),
            value => value.to_string(),
        };
        if !remote_text_is_safe(&name, true, 256) {
            return Err((
                RemoteField::Name,
                "Name must be at most 256 characters without control characters.",
            ));
        }
        // Session restore uses the display name as the stable profile
        // identifier; the parser rejects duplicates for the same reason. The
        // host being edited is not its own duplicate — otherwise no edit that
        // keeps the name could ever be saved.
        if self
            .values
            .remote_hosts
            .iter()
            .enumerate()
            .any(|(index, existing)| existing.name == name && Some(index) != self.remote_editing)
        {
            return Err((RemoteField::Name, "Another host already uses this name."));
        }
        let user = match self.remote_draft.user.trim() {
            "" => None,
            value => {
                if value.contains('@') || !remote_text_is_safe(value, false, 256) {
                    return Err((
                        RemoteField::User,
                        "User must not contain \"@\", whitespace, or control characters.",
                    ));
                }
                Some(value.to_string())
            }
        };
        let deploy = match self.remote_draft.deploy {
            1 => jterm_core::jsh_remote::Deploy::Persist,
            2 => jterm_core::jsh_remote::Deploy::Incognito,
            _ => jterm_core::jsh_remote::Deploy::Off,
        };
        // An edit keeps everything the form cannot show. Rebuilding the entry
        // from the visible rows alone would quietly delete a `-p 2222`, a
        // pinned session id or a deploy_artifact the moment someone fixed a
        // typo in the name — the config.toml-only fields are exactly the ones
        // nobody would think to check afterwards.
        let existing = self
            .remote_editing
            .and_then(|index| self.values.remote_hosts.get(index));
        Ok(RemoteHost {
            name,
            host,
            user,
            docker: self.remote_draft.docker,
            deploy_artifact: existing.and_then(|h| h.deploy_artifact.clone()),
            remote_shell: existing
                .map(|h| h.remote_shell.clone())
                .unwrap_or_else(|| "jsh".to_string()),
            session: existing.and_then(|h| h.session.clone()),
            ssh_args: existing.map(|h| h.ssh_args.clone()).unwrap_or_default(),
            login_shell: existing.is_none_or(|h| h.login_shell),
            multiplex: existing.is_none_or(|h| h.multiplex),
            deploy,
        })
    }

    fn output_font(&self, sender: &ComponentSender<Self>) {
        let _ = sender.output(SettingsOutput::FontDesc(font_desc_for_choice(
            &self.font_names,
            self.values.font,
            self.values.font_size,
        )));
    }
}

fn advanced_fields_note(host: &RemoteHost) -> Option<String> {
    let mut kept = Vec::new();
    if !host.ssh_args.is_empty() {
        kept.push(format!("ssh_args = {:?}", host.ssh_args));
    }
    if let Some(session) = &host.session {
        kept.push(format!("session = {session:?}"));
    }
    if host.remote_shell != "jsh" {
        kept.push(format!("remote_shell = {:?}", host.remote_shell));
    }
    if !host.login_shell {
        kept.push("login_shell = false".into());
    }
    if !host.multiplex {
        kept.push("multiplex = false".into());
    }
    if let Some(artifact) = &host.deploy_artifact {
        kept.push(format!("deploy_artifact = {artifact:?}"));
    }
    (!kept.is_empty()).then(|| format!("Kept as configured: {}", kept.join(", ")))
}

fn remove_remote_host(hosts: &mut Vec<RemoteHost>, index: usize, name: &str) -> bool {
    if hosts.get(index).is_some_and(|host| host.name == name) {
        hosts.remove(index);
        return true;
    }
    let before = hosts.len();
    hosts.retain(|host| host.name != name);
    before != hosts.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_host_icon_buttons_have_distinct_accessible_labels() {
        assert!(!EDIT_REMOTE_HOST_LABEL.is_empty());
        assert!(!REMOVE_REMOTE_HOST_LABEL.is_empty());
        assert_ne!(EDIT_REMOTE_HOST_LABEL, REMOVE_REMOTE_HOST_LABEL);
    }

    #[test]
    fn missing_generic_family_stays_selected_for_size_changes() {
        let (font_names, selected) = font_choices(vec!["DejaVu Sans Mono".into()], "Monospace");

        assert_eq!(font_names[selected as usize], "Monospace");
        assert_eq!(
            font_desc_for_choice(&font_names, selected, 18.0),
            "Monospace 18"
        );
    }

    #[test]
    fn configured_nerd_font_is_kept_when_pango_does_not_list_it() {
        let configured = "SauceCodePro Nerd Font Mono";
        let (font_names, selected) = font_choices(vec!["DejaVu Sans Mono".into()], configured);

        assert_eq!(font_names[selected as usize], configured);
        assert_eq!(
            font_desc_for_choice(&font_names, selected, 16.0),
            "SauceCodePro Nerd Font Mono 16"
        );
    }

    #[test]
    fn an_existing_current_family_is_not_duplicated() {
        let (font_names, selected) = font_choices(
            vec!["monospace".into(), "DejaVu Sans Mono".into()],
            "Monospace",
        );

        assert_eq!(font_names[selected as usize], "monospace");
        assert_eq!(
            font_names
                .iter()
                .filter(|family| family.eq_ignore_ascii_case("Monospace"))
                .count(),
            1
        );
    }

    fn host_with_hidden_fields() -> RemoteHost {
        RemoteHost {
            name: "dev-60".to_string(),
            host: "10.68.18.60".to_string(),
            user: Some("root".to_string()),
            docker: false,
            deploy_artifact: Some("/opt/jsh/jsh".to_string()),
            remote_shell: "jsh".to_string(),
            session: Some("dev-main".to_string()),
            ssh_args: vec!["-p".to_string(), "2222".to_string()],
            login_shell: false,
            multiplex: false,
            deploy: jterm_core::jsh_remote::Deploy::Persist,
        }
    }

    /// A model with no widgets: `validate_remote_draft` reads only model state,
    /// so the form logic is testable without a display.
    fn model(hosts: Vec<RemoteHost>, editing: Option<usize>) -> SettingsModel {
        let draft = editing
            .and_then(|index| hosts.get(index))
            .map(RemoteDraft::from_host)
            .unwrap_or_default();
        SettingsModel {
            theme_names: Vec::new(),
            font_names: Vec::new(),
            values: SettingsValues {
                theme: 0,
                font: 0,
                font_size: 12.0,
                font_scale: 1.0,
                opacity: 1.0,
                scrollback: 5000.0,
                terminal_mode: 0,
                block_compact: false,
                command_history: true,
                ascii_organism_enabled: false,
                ascii_organism_motion: 0,
                ai_enabled: false,
                ai_panel_visible: false,
                ai_panel_width: 360.0,
                agent_enabled: false,
                command_correction_enabled: false,
                ai_provider: 0,
                ai_model: String::new(),
                ai_base_url: String::new(),
                ai_api_key_file: None,
                ai_max_tokens: 1024.0,
                ai_redact_secrets: true,
                ai_stream: true,
                agent_max_turns: 20.0,
                safe_mode: false,
                notifications: true,
                remote_clipboard: false,
                remote_hosts: hosts,
            },
            remote_draft: draft,
            remote_editing: editing,
            remote_dialog: None,
            remote_rows: Vec::new(),
        }
    }

    /// The form shows five fields; the entry has ten. Renaming through the form
    /// must not be a way to lose the other five, because nothing in the dialog
    /// would show that it happened.
    #[test]
    fn editing_preserves_fields_the_form_cannot_show() {
        let mut model = model(vec![host_with_hidden_fields()], Some(0));
        model.remote_draft.name = "prod-60".to_string();

        let edited = model.validate_remote_draft().expect("valid draft");
        assert_eq!(edited.name, "prod-60");
        assert_eq!(edited.ssh_args, ["-p", "2222"]);
        assert_eq!(edited.session.as_deref(), Some("dev-main"));
        assert_eq!(edited.deploy_artifact.as_deref(), Some("/opt/jsh/jsh"));
        assert!(!edited.login_shell);
        assert!(!edited.multiplex);
    }

    /// A new host gets the plain defaults rather than anything left over from a
    /// previously edited entry.
    #[test]
    fn adding_a_host_starts_from_the_defaults() {
        let mut model = model(vec![host_with_hidden_fields()], None);
        model.remote_draft.name = "staging".to_string();
        model.remote_draft.host = "staging.example.com".to_string();

        let added = model.validate_remote_draft().expect("valid draft");
        assert!(added.ssh_args.is_empty());
        assert_eq!(added.session, None);
        assert_eq!(added.deploy_artifact, None);
        assert!(added.login_shell);
        assert!(added.multiplex);
    }

    #[test]
    fn adding_is_refused_at_the_shared_remote_host_limit() {
        let hosts = (0..crate::config::MAX_REMOTE_HOSTS)
            .map(|index| {
                let mut host = host_with_hidden_fields();
                host.name = format!("host-{index}");
                host.host = format!("host-{index}.example");
                host
            })
            .collect();
        let mut model = model(hosts, None);
        model.remote_draft.host = "one-too-many.example".to_string();

        let (field, message) = model
            .validate_remote_draft()
            .expect_err("host limit must be enforced");
        assert!(matches!(field, RemoteField::Form));
        assert_eq!(message, "The remote host limit is reached.");
    }

    #[test]
    fn an_edit_that_keeps_the_name_is_not_a_duplicate() {
        let mut model = model(vec![host_with_hidden_fields()], Some(0));
        model.remote_draft.host = "10.68.18.61".to_string();

        let edited = model.validate_remote_draft().expect("valid draft");
        assert_eq!(edited.name, "dev-60");
        assert_eq!(edited.host, "10.68.18.61");
    }

    #[test]
    fn an_edit_may_not_take_another_hosts_name() {
        let mut other = host_with_hidden_fields();
        other.name = "myubuntu".to_string();
        other.host = "myubuntu".to_string();
        let mut model = model(vec![host_with_hidden_fields(), other], Some(0));
        model.remote_draft.name = "myubuntu".to_string();

        let (field, _) = model.validate_remote_draft().expect_err("duplicate name");
        assert!(matches!(field, RemoteField::Name));
    }

    #[test]
    fn edit_dialog_draft_loads_every_visible_field() {
        let host = host_with_hidden_fields();
        let draft = RemoteDraft::from_host(&host);

        assert_eq!(draft.name, "dev-60");
        assert_eq!(draft.host, "10.68.18.60");
        assert_eq!(draft.user, "root");
        assert!(!draft.docker);
        assert_eq!(draft.deploy, 1);
    }

    #[test]
    fn advanced_note_discloses_every_preserved_field() {
        let mut host = host_with_hidden_fields();
        host.remote_shell = "/bin/bash".to_string();
        let note = advanced_fields_note(&host).expect("host has advanced fields");

        for field in [
            "ssh_args",
            "session",
            "remote_shell",
            "login_shell",
            "multiplex",
            "deploy_artifact",
        ] {
            assert!(note.contains(field), "missing {field} in {note:?}");
        }
    }

    #[test]
    fn default_host_has_no_advanced_note() {
        let mut host = host_with_hidden_fields();
        host.ssh_args.clear();
        host.session = None;
        host.remote_shell = "jsh".to_string();
        host.login_shell = true;
        host.multiplex = true;
        host.deploy_artifact = None;

        assert_eq!(advanced_fields_note(&host), None);
    }

    #[test]
    fn confirmed_delete_uses_index_when_it_still_matches() {
        let first = host_with_hidden_fields();
        let mut second = host_with_hidden_fields();
        second.name = "staging".to_string();
        let mut hosts = vec![first, second];

        assert!(remove_remote_host(&mut hosts, 1, "staging"));
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "dev-60");
    }

    #[test]
    fn confirmed_delete_falls_back_to_name_after_list_changes() {
        let target = host_with_hidden_fields();
        let mut other = host_with_hidden_fields();
        other.name = "staging".to_string();
        let mut hosts = vec![other, target];

        // The confirmation captured index zero before another update changed
        // the ordering. The stable profile name must still identify the row.
        assert!(remove_remote_host(&mut hosts, 0, "dev-60"));
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "staging");
        assert!(!remove_remote_host(&mut hosts, 9, "missing"));
    }
}
