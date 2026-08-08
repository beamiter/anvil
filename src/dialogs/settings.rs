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
    pub(crate) ai_enabled: bool,
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
    Name,
    Host,
    User,
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
    TerminalMode(u32),
    BlockCompact(bool),
    CommandHistory(bool),
    AiEnabled(bool),
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
    /// Commit the form: append a new host, or replace the one being edited.
    RemoteHostAdd,
    /// Load an existing host into the form instead of starting a new one.
    RemoteHostEdit(usize),
    /// Abandon an in-progress edit and leave the saved host as it was.
    RemoteHostCancelEdit,
    RemoteHostRemove(usize),
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
    AiEnabled(bool),
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
                        set_model: Some(&gtk::StringList::new(&["Block", "VTE compatibility"])),
                        set_selected: model.values.terminal_mode,
                        connect_selected_notify[sender] => move |row| {
                            sender.input(SettingsMsg::TerminalMode(row.selected()));
                        },
                    },

                    #[name(block_compact_row)]
                    adw::SwitchRow {
                        set_title: "Compact Block Layout",
                        set_subtitle: "Use denser spacing in new Block panes",
                        set_active: model.values.block_compact,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::BlockCompact(row.is_active()));
                        },
                    },

                    #[name(command_history_row)]
                    adw::SwitchRow {
                        set_title: "Command History Index",
                        set_subtitle: "Store commands and status, never output",
                        set_active: model.values.command_history,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::CommandHistory(row.is_active()));
                        },
                    },
                },

                // Host rows are managed imperatively (`rebuild_remote_rows`):
                // the view! macro cannot express a list that grows and shrinks.
                #[name(remote_hosts_group)]
                adw::PreferencesGroup {
                    set_title: "Remote Hosts",
                    set_description: Some("Saved ssh and Docker targets for the remote picker"),
                },

                // Doubles as the edit form: picking a host above loads it here
                // and retitles the group, so there is one set of rows to keep
                // in step with `parse_remote_hosts` rather than two.
                #[name(remote_form_group)]
                adw::PreferencesGroup {
                    set_title: "Add Remote Host",

                    #[name(remote_name_row)]
                    adw::EntryRow {
                        set_title: "Name (defaults to host)",
                        connect_changed[sender] => move |row| {
                            sender.input(SettingsMsg::RemoteHostName(row.text().to_string()));
                        },
                    },

                    #[name(remote_host_row)]
                    adw::EntryRow {
                        set_title: "Host or container name",
                        connect_changed[sender] => move |row| {
                            sender.input(SettingsMsg::RemoteHostHost(row.text().to_string()));
                        },
                    },

                    #[name(remote_user_row)]
                    adw::EntryRow {
                        set_title: "User (optional)",
                        connect_changed[sender] => move |row| {
                            sender.input(SettingsMsg::RemoteHostUser(row.text().to_string()));
                        },
                    },

                    #[name(remote_docker_row)]
                    adw::SwitchRow {
                        set_title: "Docker Container",
                        set_subtitle: "Attach to a running container instead of using ssh",
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::RemoteHostDocker(row.is_active()));
                        },
                    },

                    #[name(remote_deploy_row)]
                    adw::ComboRow {
                        set_title: "Deploy jsh",
                        set_subtitle: "Place a verified jsh on the destination for the session",
                        set_model: Some(&gtk::StringList::new(&["Off", "Persist", "Incognito"])),
                        connect_selected_notify[sender] => move |row| {
                            sender.input(SettingsMsg::RemoteHostDeploy(row.selected()));
                        },
                    },

                    #[name(remote_add_row)]
                    adw::ActionRow {
                        set_title: "Add Host",
                        set_activatable: true,
                        add_suffix = &gtk::Image {
                            set_icon_name: Some("list-add-symbolic"),
                        },
                        connect_activated[sender] => move |_| {
                            sender.input(SettingsMsg::RemoteHostAdd);
                        },
                    },

                    // Only shown while editing: without it there is no way back
                    // to composing a new host except saving one you did not mean
                    // to change.
                    #[name(remote_cancel_row)]
                    adw::ActionRow {
                        set_title: "Cancel Edit",
                        set_activatable: true,
                        set_visible: false,
                        add_suffix = &gtk::Image {
                            set_icon_name: Some("edit-undo-symbolic"),
                        },
                        connect_activated[sender] => move |_| {
                            sender.input(SettingsMsg::RemoteHostCancelEdit);
                        },
                    },
                },

                adw::PreferencesGroup {
                    set_title: &gtk::glib::markup_escape_text("Features & Privacy"),

                    #[name(ai_enabled_row)]
                    adw::SwitchRow {
                        set_title: "AI Features",
                        set_subtitle: "Requests follow explicit AI actions; enabled correction may use a fallback request after a narrow failure",
                        set_active: model.values.ai_enabled,
                        set_sensitive: !model.values.safe_mode,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiEnabled(row.is_active()));
                        },
                    },

                    #[name(agent_enabled_row)]
                    adw::SwitchRow {
                        set_title: "Shell Agent",
                        set_subtitle: "Commands always require approval",
                        set_active: model.values.agent_enabled,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AgentEnabled(row.is_active()));
                        },
                    },

                    adw::SwitchRow {
                        set_title: "Automatic Agent Approval Retired",
                        set_subtitle: "Always off; every Agent proposal requires explicit approval",
                        set_active: false,
                        set_sensitive: false,
                    },

                    #[name(command_correction_row)]
                    adw::SwitchRow {
                        set_title: "AI Command Correction",
                        set_subtitle: "Offer editable fixes; only exact host-verified candidates can be explicitly run",
                        set_active: model.values.command_correction_enabled,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::CommandCorrection(row.is_active()));
                        },
                    },

                    #[name(ai_provider_row)]
                    adw::ComboRow {
                        set_title: "AI Provider",
                        set_subtitle: "Key from a private key file or environment variables",
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
                        set_title: "AI Model",
                        set_text: &model.values.ai_model,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_changed[sender] => move |row| {
                            sender.input(SettingsMsg::AiModel(row.text().to_string()));
                        },
                    },

                    #[name(ai_base_url_row)]
                    adw::EntryRow {
                        set_title: "AI Base URL",
                        set_text: &model.values.ai_base_url,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_changed[sender] => move |row| {
                            sender.input(SettingsMsg::AiBaseUrl(row.text().to_string()));
                        },
                    },

                    #[name(ai_api_key_row)]
                    adw::PasswordEntryRow {
                        set_title: "API Key",
                        set_show_apply_button: true,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_apply[sender] => move |row| {
                            sender.input(SettingsMsg::AiApiKeyStore(row.text().to_string()));
                        },
                    },

                    #[name(ai_max_tokens_row)]
                    adw::SpinRow::new(
                        Some(&gtk::Adjustment::new(
                            model.values.ai_max_tokens, 1.0, 32_768.0, 64.0, 512.0, 0.0
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

                    #[name(ai_redact_secrets_row)]
                    adw::SwitchRow {
                        set_title: "Redact Common Secrets",
                        set_subtitle: "Apply before terminal context is sent",
                        set_active: model.values.ai_redact_secrets,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiRedactSecrets(row.is_active()));
                        },
                    },

                    #[name(ai_stream_row)]
                    adw::SwitchRow {
                        set_title: "Stream AI Panel Replies",
                        set_subtitle: "Show the answer while it is being generated",
                        set_active: model.values.ai_stream,
                        set_sensitive: !model.values.safe_mode && model.values.ai_enabled,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AiStream(row.is_active()));
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
                        set_title: "Shell Agent Turn Limit",
                        set_sensitive: !model.values.safe_mode
                            && model.values.ai_enabled
                            && model.values.agent_enabled,
                        connect_value_notify[sender] => move |row| {
                            sender.input(SettingsMsg::AgentMaxTurns(row.value()));
                        },
                    },

                    #[name(notifications_row)]
                    adw::SwitchRow {
                        set_title: "Long-command Notifications",
                        set_active: model.values.notifications,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::Notifications(row.is_active()));
                        },
                    },

                    #[name(remote_clipboard_row)]
                    adw::SwitchRow {
                        set_title: "Allow OSC 52 Clipboard Writes",
                        set_subtitle: "Enable only for trusted local and remote programs",
                        set_active: model.values.remote_clipboard,
                        connect_active_notify[sender] => move |row| {
                            sender.input(SettingsMsg::RemoteClipboard(row.is_active()));
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
        let mut model = Self {
            theme_names: init.theme_names,
            font_names: init.font_names,
            values: init.values,
            remote_draft: RemoteDraft::default(),
            remote_editing: None,
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
            SettingsMsg::Toggle(values, parent) => {
                if root.parent().is_some() {
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
                widgets
                    .terminal_mode_row
                    .set_selected(self.values.terminal_mode);
                widgets
                    .block_compact_row
                    .set_active(self.values.block_compact);
                widgets
                    .command_history_row
                    .set_active(self.values.command_history);
                widgets.ai_enabled_row.set_active(self.values.ai_enabled);
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
                widgets.ai_api_key_row.set_title("API Key");
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
                    .remote_clipboard_row
                    .set_active(self.values.remote_clipboard);
                // A reopened dialog starts on a fresh host: the index an edit
                // was holding may not survive whatever changed the list while
                // the dialog was closed.
                self.remote_editing = None;
                self.remote_draft = RemoteDraft::default();
                Self::fill_remote_form(widgets, &RemoteDraft::default());
                Self::sync_remote_form_mode(widgets, None);
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
            SettingsMsg::AiEnabled(enabled) => {
                self.values.ai_enabled = enabled;
                let sensitive = !self.values.safe_mode && enabled;
                widgets.agent_enabled_row.set_sensitive(sensitive);
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
            SettingsMsg::RemoteHostAdd => {
                Self::clear_remote_errors(widgets);
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
                        self.remote_editing = None;
                        self.remote_draft = RemoteDraft::default();
                        Self::fill_remote_form(widgets, &RemoteDraft::default());
                        Self::sync_remote_form_mode(widgets, None);
                        self.rebuild_remote_rows(&widgets.remote_hosts_group, &sender);
                        let _ = sender.output(SettingsOutput::RemoteHosts(
                            self.values.remote_hosts.clone(),
                        ));
                    }
                    Err((field, message)) => {
                        let row = match field {
                            RemoteField::Name => &widgets.remote_name_row,
                            RemoteField::Host => &widgets.remote_host_row,
                            RemoteField::User => &widgets.remote_user_row,
                        };
                        row.add_css_class("error");
                        row.grab_focus();
                        widgets.remote_add_row.set_subtitle(message);
                    }
                }
            }
            SettingsMsg::RemoteHostEdit(index) => {
                if let Some(host) = self.values.remote_hosts.get(index) {
                    Self::clear_remote_errors(widgets);
                    self.remote_draft = RemoteDraft::from_host(host);
                    self.remote_editing = Some(index);
                    Self::fill_remote_form(widgets, &self.remote_draft);
                    Self::sync_remote_form_mode(widgets, Some(host.name.clone()));
                    // Redraw so the row under edit is the one marked as such.
                    self.rebuild_remote_rows(&widgets.remote_hosts_group, &sender);
                    widgets.remote_host_row.grab_focus();
                }
            }
            SettingsMsg::RemoteHostCancelEdit => {
                Self::clear_remote_errors(widgets);
                self.remote_editing = None;
                self.remote_draft = RemoteDraft::default();
                Self::fill_remote_form(widgets, &RemoteDraft::default());
                Self::sync_remote_form_mode(widgets, None);
                self.rebuild_remote_rows(&widgets.remote_hosts_group, &sender);
            }
            SettingsMsg::RemoteHostRemove(index) => {
                if index < self.values.remote_hosts.len() {
                    self.values.remote_hosts.remove(index);
                    // Every later index just shifted. Rather than renumber an
                    // edit that is mid-flight, drop it: silently retargeting the
                    // form at a different host is the worse outcome.
                    if self.remote_editing.is_some() {
                        self.remote_editing = None;
                        self.remote_draft = RemoteDraft::default();
                        Self::fill_remote_form(widgets, &RemoteDraft::default());
                        Self::sync_remote_form_mode(widgets, None);
                    }
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
    fn clear_remote_errors(widgets: &<Self as Component>::Widgets) {
        for row in [
            &widgets.remote_name_row,
            &widgets.remote_host_row,
            &widgets.remote_user_row,
        ] {
            row.remove_css_class("error");
        }
    }

    /// Push draft state into the form. A default draft clears it.
    fn fill_remote_form(widgets: &<Self as Component>::Widgets, draft: &RemoteDraft) {
        widgets.remote_name_row.set_text(&draft.name);
        widgets.remote_host_row.set_text(&draft.host);
        widgets.remote_user_row.set_text(&draft.user);
        widgets.remote_docker_row.set_active(draft.docker);
        widgets.remote_deploy_row.set_selected(draft.deploy);
        widgets.remote_add_row.set_subtitle("");
    }

    /// Retitle the form for whichever host it is about to write. `editing`
    /// carries the display name so the group says which one, which matters once
    /// several hosts differ only in a field the row subtitle truncates.
    fn sync_remote_form_mode(widgets: &<Self as Component>::Widgets, editing: Option<String>) {
        match editing {
            Some(name) => {
                widgets.remote_form_group.set_title("Edit Remote Host");
                widgets
                    .remote_form_group
                    .set_description(Some(&format!("Editing “{name}”")));
                widgets.remote_add_row.set_title("Save Changes");
                widgets.remote_cancel_row.set_visible(true);
            }
            None => {
                widgets.remote_form_group.set_title("Add Remote Host");
                widgets.remote_form_group.set_description(None);
                widgets.remote_add_row.set_title("Add Host");
                widgets.remote_cancel_row.set_visible(false);
            }
        }
    }

    fn rebuild_remote_rows(
        &mut self,
        group: &adw::PreferencesGroup,
        sender: &ComponentSender<Self>,
    ) {
        for row in self.remote_rows.drain(..) {
            group.remove(&row);
        }
        for (index, host) in self.values.remote_hosts.iter().enumerate() {
            let row = adw::ActionRow::new();
            row.set_use_markup(false);
            row.set_title(if host.name.is_empty() {
                &host.host
            } else {
                &host.name
            });
            // The docker user is `docker exec -u`, not part of the target.
            let target = if host.docker {
                host.host.clone()
            } else {
                match &host.user {
                    Some(user) => format!("{user}@{}", host.host),
                    None => host.host.clone(),
                }
            };
            let transport = if host.docker { "docker" } else { "ssh" };
            let mut subtitle = format!("{transport} · {target} · deploy {}", host.deploy.as_str());
            // The form has no widget for these, so say they are there rather
            // than let an edit look like it silently dropped them.
            if !host.ssh_args.is_empty() {
                subtitle.push_str(&format!(" · ssh_args {}", host.ssh_args.join(" ")));
            }
            if self.remote_editing == Some(index) {
                subtitle.push_str(" · editing");
            }
            row.set_subtitle(&subtitle);
            let edit = gtk::Button::from_icon_name("document-edit-symbolic");
            edit.set_valign(gtk::Align::Center);
            edit.add_css_class("flat");
            edit.set_tooltip_text(Some("Edit host"));
            edit.connect_clicked({
                let sender = sender.clone();
                move |_| sender.input(SettingsMsg::RemoteHostEdit(index))
            });
            row.add_suffix(&edit);
            let remove = gtk::Button::from_icon_name("user-trash-symbolic");
            remove.set_valign(gtk::Align::Center);
            remove.add_css_class("flat");
            remove.add_css_class("destructive-action");
            remove.set_tooltip_text(Some("Remove host"));
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

#[cfg(test)]
mod tests {
    use super::*;

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
                ai_enabled: false,
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
}
