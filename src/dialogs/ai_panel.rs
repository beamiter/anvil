//! Persistent Relm4 AI chat sidebar.
//!
//! The component stays mounted beside the terminal stack. Its pure
//! [`ChatStore`] owns every chat while GTK renders only the selected one.

use std::collections::HashMap;

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::prelude::*;

use super::ai_chat_store::{
    ChatStatus, ChatStore, ChatStoreError, RequestToken, MAX_LIVE_MESSAGE_BYTES,
};
use crate::{ai, palette};

const STOPPED_STATUS: &str = "Response stopped. You can retry when ready.";
const CHAT_PAGE: &str = "chat";
const LIBRARY_PAGE: &str = "library";
const NEW_CHAT_LABEL: &str = "New chat";
const CLOSE_AI_PANEL_LABEL: &str = "Close AI panel";
const CLEAR_BLOCK_CONTEXT_LABEL: &str = "Clear selected Block context";
const BACK_TO_CONVERSATION_LABEL: &str = "Back to conversation";
// The outer session JSON escapes this JSON string again. Keeping the inner
// value at 1 MiB leaves ample room below session.rs's 4 MiB hard limit.
const SESSION_SNAPSHOT_AI_BUDGET: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ComposerKeyAction {
    Send,
    Newline,
    Proceed,
}

fn classify_composer_key(key: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> ComposerKeyAction {
    use gtk::gdk::{Key, ModifierType};

    if !matches!(key, Key::Return | Key::KP_Enter) {
        return ComposerKeyAction::Proceed;
    }
    if state.contains(ModifierType::SHIFT_MASK) {
        ComposerKeyAction::Newline
    } else if state.intersects(
        ModifierType::ALT_MASK
            | ModifierType::SUPER_MASK
            | ModifierType::HYPER_MASK
            | ModifierType::META_MASK,
    ) {
        ComposerKeyAction::Proceed
    } else {
        ComposerKeyAction::Send
    }
}

pub(crate) struct AiPanelInit {
    pub(crate) redact_secrets: bool,
}

#[derive(Clone, Debug)]
struct RequestPayload {
    user_text: String,
    context: Option<ai::BlockContext>,
    restore_pending_as_draft: bool,
}

#[derive(Debug)]
pub(crate) enum AiPanelMsg {
    Open {
        history_path: Option<String>,
        client: ai::AiClient,
        stream: bool,
        redact_secrets: bool,
        initial_context: Option<ai::BlockContext>,
    },
    Restore(String),
    Ask,
    Stop,
    Retry,
    Delta {
        token: RequestToken,
        text: String,
    },
    Result {
        token: RequestToken,
        result: Result<String, String>,
    },
    DraftChanged(String),
    PublishDraft(u64),
    NewChat,
    SelectChat(u64),
    SearchChanged(String),
    ShowLibrary,
    ShowChat,
    Rename(String),
    ToggleArchive,
    Delete,
    DeleteConfirmed,
    ClearContext,
    IncludeRecent(bool),
    CopyFocused,
    PasteFocused,
    Close,
}

#[derive(Debug)]
pub(crate) enum AiPanelOutput {
    SnapshotChanged(String),
    CloseRequested,
}

pub(crate) struct AiPanelModel {
    history_path: Option<String>,
    client: Option<ai::AiClient>,
    stream: bool,
    redact_secrets: bool,
    store: ChatStore,
    requests: HashMap<RequestToken, ai::AiHandle>,
    retry_payloads: HashMap<u64, RequestPayload>,
    conversation_systems: HashMap<u64, String>,
    include_recent: HashMap<u64, bool>,
    search: String,
    draft_generation: u64,
    rendering: bool,
}

#[relm4::component(pub(crate))]
impl Component for AiPanelModel {
    type Init = AiPanelInit;
    type Input = AiPanelMsg;
    type Output = AiPanelOutput;
    type CommandOutput = ();

    view! {
        root = gtk::Box {
            set_orientation: gtk::Orientation::Vertical,
            set_width_request: 280,
            set_hexpand: false,
            set_vexpand: true,
            add_css_class: "ai-panel",

            gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 4,
                set_margin_all: 6,

                gtk::Button {
                    set_label: "Chats",
                    add_css_class: "flat",
                    set_tooltip_text: Some("Browse saved and archived chats"),
                    connect_clicked => AiPanelMsg::ShowLibrary,
                },

                #[name(title_entry)]
                gtk::Entry {
                    set_hexpand: true,
                    set_tooltip_text: Some("Rename this chat"),
                    connect_changed[sender] => move |entry| {
                        sender.input(AiPanelMsg::Rename(entry.text().to_string()));
                    },
                },

                gtk::Button {
                    set_icon_name: "list-add-symbolic",
                    add_css_class: "flat",
                    set_tooltip_text: Some("New chat"),
                    update_property: &[
                        gtk::accessible::Property::Label(NEW_CHAT_LABEL),
                    ],
                    connect_clicked => AiPanelMsg::NewChat,
                },

                gtk::Button {
                    set_icon_name: "window-close-symbolic",
                    add_css_class: "flat",
                    set_tooltip_text: Some("Close AI panel"),
                    update_property: &[
                        gtk::accessible::Property::Label(CLOSE_AI_PANEL_LABEL),
                    ],
                    connect_clicked => AiPanelMsg::Close,
                },
            },

            #[name(page_stack)]
            gtk::Stack {
                set_hexpand: true,
                set_vexpand: true,

                add_named[Some(CHAT_PAGE)] = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 6,
                    set_margin_all: 8,

                    #[name(context_row)]
                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 4,
                        set_visible: false,

                        #[name(context_label)]
                        gtk::Label {
                            set_halign: gtk::Align::Start,
                            set_hexpand: true,
                            set_ellipsize: gtk::pango::EllipsizeMode::Middle,
                            add_css_class: "dim-label",
                        },

                        gtk::Button {
                            set_icon_name: "edit-clear-symbolic",
                            add_css_class: "flat",
                            set_tooltip_text: Some("Clear selected Block context"),
                            update_property: &[
                                gtk::accessible::Property::Label(CLEAR_BLOCK_CONTEXT_LABEL),
                            ],
                            connect_clicked => AiPanelMsg::ClearContext,
                        },
                    },

                    gtk::ScrolledWindow {
                        set_hexpand: true,
                        set_vexpand: true,
                        set_policy: (gtk::PolicyType::Never, gtk::PolicyType::Automatic),

                        #[name(transcript)]
                        gtk::TextView {
                            set_editable: false,
                            set_cursor_visible: false,
                            set_wrap_mode: gtk::WrapMode::WordChar,
                            set_left_margin: 6,
                            set_right_margin: 6,
                            add_css_class: "ai-explain-body",
                        },
                    },

                    #[name(include_recent)]
                    gtk::CheckButton {
                        set_label: Some("Include recent shell context"),
                        set_active: true,
                    },

                    gtk::ScrolledWindow {
                        set_hexpand: true,
                        set_min_content_height: 72,
                        set_max_content_height: 160,

                        #[name(composer)]
                        gtk::TextView {
                            set_wrap_mode: gtk::WrapMode::WordChar,
                            set_height_request: 72,
                            add_css_class: "ai-panel-entry",
                        },
                    },

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 6,

                        #[name(spinner)]
                        gtk::Spinner {
                            set_visible: false,
                        },

                        #[name(status)]
                        gtk::Label {
                            set_halign: gtk::Align::Start,
                            set_hexpand: true,
                            set_wrap: true,
                            set_xalign: 0.0,
                            set_selectable: true,
                            add_css_class: "dim-label",
                        },

                        #[name(retry_button)]
                        gtk::Button {
                            set_label: "Retry",
                            set_visible: false,
                            connect_clicked => AiPanelMsg::Retry,
                        },

                        #[name(stop_button)]
                        gtk::Button {
                            set_label: "Stop",
                            set_visible: false,
                            add_css_class: "destructive-action",
                            connect_clicked => AiPanelMsg::Stop,
                        },

                        #[name(send_button)]
                        gtk::Button {
                            set_label: "Send",
                            set_tooltip_text: Some(
                                "Send (Enter / Ctrl+Enter) · New line (Shift+Enter)"
                            ),
                            add_css_class: "suggested-action",
                            connect_clicked => AiPanelMsg::Ask,
                        },
                    },

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 4,

                        #[name(archive_button)]
                        gtk::Button {
                            set_label: "Archive",
                            add_css_class: "flat",
                            connect_clicked => AiPanelMsg::ToggleArchive,
                        },

                        gtk::Button {
                            set_label: "Delete",
                            add_css_class: "flat",
                            add_css_class: "destructive-action",
                            connect_clicked => AiPanelMsg::Delete,
                        },
                    },
                },

                add_named[Some(LIBRARY_PAGE)] = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 6,
                    set_margin_all: 8,

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 6,

                        gtk::Button {
                            set_icon_name: "go-previous-symbolic",
                            add_css_class: "flat",
                            set_tooltip_text: Some("Back to conversation"),
                            update_property: &[
                                gtk::accessible::Property::Label(BACK_TO_CONVERSATION_LABEL),
                            ],
                            connect_clicked => AiPanelMsg::ShowChat,
                        },

                        #[name(search)]
                        gtk::SearchEntry {
                            set_hexpand: true,
                            set_placeholder_text: Some("Search chats"),
                            connect_search_changed[sender] => move |entry| {
                                sender.input(AiPanelMsg::SearchChanged(entry.text().to_string()));
                            },
                        },
                    },

                    gtk::ScrolledWindow {
                        set_hexpand: true,
                        set_vexpand: true,

                        #[name(chat_list)]
                        gtk::ListBox {
                            set_selection_mode: gtk::SelectionMode::None,
                            add_css_class: "boxed-list",
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
            history_path: None,
            client: None,
            stream: true,
            redact_secrets: init.redact_secrets,
            store: ChatStore::default(),
            requests: HashMap::new(),
            retry_payloads: HashMap::new(),
            conversation_systems: HashMap::new(),
            include_recent: HashMap::new(),
            search: String::new(),
            draft_generation: 0,
            rendering: false,
        };
        let widgets = view_output!();

        {
            let sender = sender.clone();
            widgets.composer.buffer().connect_changed(move |buffer| {
                let (start, end) = buffer.bounds();
                sender.input(AiPanelMsg::DraftChanged(
                    buffer.text(&start, &end, true).to_string(),
                ));
            });
        }
        {
            let key = gtk::EventControllerKey::new();
            key.set_propagation_phase(gtk::PropagationPhase::Capture);
            let sender = sender.clone();
            let composer = widgets.composer.clone();
            key.connect_key_pressed(move |controller, key, _, state| {
                let action = classify_composer_key(key, state);
                if action != ComposerKeyAction::Proceed {
                    if let Some(event) = controller.current_event() {
                        // An active IME candidate owns Enter before the chat
                        // composer can interpret it as Send or Newline.
                        if composer.im_context_filter_keypress(&event) {
                            return gtk::glib::Propagation::Stop;
                        }
                    }
                }
                match action {
                    ComposerKeyAction::Send => {
                        sender.input(AiPanelMsg::Ask);
                        gtk::glib::Propagation::Stop
                    }
                    ComposerKeyAction::Newline | ComposerKeyAction::Proceed => {
                        gtk::glib::Propagation::Proceed
                    }
                }
            });
            widgets.composer.add_controller(key);
        }
        {
            let sender = sender.clone();
            widgets.include_recent.connect_toggled(move |button| {
                sender.input(AiPanelMsg::IncludeRecent(button.is_active()));
            });
        }

        let mut parts = ComponentParts { model, widgets };
        parts.model.render_all(&mut parts.widgets, &sender);
        parts
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match msg {
            AiPanelMsg::Open {
                history_path,
                client,
                stream,
                redact_secrets,
                initial_context,
            } => {
                self.history_path = history_path;
                self.client = Some(client);
                self.stream = stream;
                self.redact_secrets = redact_secrets;
                widgets.page_stack.set_visible_child_name(CHAT_PAGE);
                if let Some(context) = initial_context {
                    if self.store.active_archived() {
                        let _ = self.store.new_chat();
                    }
                    if self.store.active_request_token().is_none() {
                        let prompt = if context.exit_code == 0 {
                            "Explain what this command does and what its output means."
                        } else {
                            "This command failed. Diagnose the error and suggest a fix."
                        };
                        self.start_request(
                            widgets,
                            &sender,
                            RequestPayload {
                                user_text: prompt.into(),
                                context: Some(context),
                                restore_pending_as_draft: false,
                            },
                            false,
                        );
                    }
                }
                self.render_all(widgets, &sender);
                widgets.composer.grab_focus();
            }
            AiPanelMsg::Restore(encoded) => match ai::ConversationSnapshot::from_json(&encoded) {
                Ok(snapshot) => {
                    self.cancel_all();
                    self.store = ChatStore::restore(snapshot);
                    self.retry_payloads.clear();
                    self.conversation_systems.clear();
                    self.render_all(widgets, &sender);
                }
                Err(error) => {
                    widgets
                        .status
                        .set_label(&format!("Saved AI chats were not restored: {error}"));
                }
            },
            AiPanelMsg::Ask => {
                if self.store.active_request_token().is_some() {
                    return;
                }
                let text = text_view_text(&widgets.composer);
                let text = text.trim().to_string();
                self.start_request(
                    widgets,
                    &sender,
                    RequestPayload {
                        user_text: text,
                        context: None,
                        restore_pending_as_draft: true,
                    },
                    true,
                );
            }
            AiPanelMsg::Stop => {
                let Some(token) = self.store.active_request_token() else {
                    return;
                };
                let Some(handle) = self.requests.remove(&token) else {
                    return;
                };
                handle.cancel();
                let _ = self.store.cancel_request(token, STOPPED_STATUS.to_string());
                self.render_all(widgets, &sender);
                self.publish_snapshot(&sender);
            }
            AiPanelMsg::Retry => {
                let id = self.store.active_id();
                let Some(payload) = self.retry_payloads.get(&id).cloned() else {
                    return;
                };
                let remaining =
                    draft_without_retry_message(&payload.user_text, self.store.active_draft());
                let original = self.store.active_draft().to_string();
                self.store.set_active_draft(remaining);
                if !self.start_request(widgets, &sender, payload, false) {
                    self.store.set_active_draft(original);
                    self.render_all(widgets, &sender);
                }
            }
            AiPanelMsg::Delta { token, text } => {
                if self.store.push_delta(token, &text) == Some(true) {
                    self.render_transcript(widgets);
                }
            }
            AiPanelMsg::Result { token, result } => {
                if self.requests.remove(&token).is_none() {
                    return;
                }
                let keep_failed_partial = result.is_err()
                    && self.store.active_request_token() == Some(token)
                    && !self.store.active_partial().is_empty();
                match result {
                    Ok(answer) => {
                        if self
                            .store
                            .complete_success(token, answer.trim().to_string())
                            .is_some()
                        {
                            self.retry_payloads.remove(&token.chat_id);
                        }
                    }
                    Err(error) => {
                        let error = crate::review_input::safe_inline_display(&error, 2 * 1024);
                        let _ = self
                            .store
                            .complete_error(token, format!("AI error: {error}"));
                    }
                }
                if keep_failed_partial {
                    // Preserve the streamed evidence already visible. The
                    // durable store has rolled the failed turn back into the
                    // draft; switching chats or retrying rematerializes from
                    // that authoritative state and removes this transient row.
                    self.rendering = true;
                    widgets
                        .composer
                        .buffer()
                        .set_text(self.store.active_draft());
                    self.rendering = false;
                    self.render_context(widgets);
                    self.render_status(widgets);
                    self.refresh_library(&widgets.chat_list, &sender);
                } else {
                    self.render_all(widgets, &sender);
                }
                self.publish_snapshot(&sender);
            }
            AiPanelMsg::DraftChanged(draft) => {
                if self.rendering || !self.store.set_active_draft(draft) {
                    return;
                }
                self.draft_generation = self.draft_generation.wrapping_add(1);
                let generation = self.draft_generation;
                let sender = sender.clone();
                gtk::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(250),
                    move || sender.input(AiPanelMsg::PublishDraft(generation)),
                );
            }
            AiPanelMsg::PublishDraft(generation) => {
                if generation == self.draft_generation {
                    self.publish_snapshot(&sender);
                    self.refresh_library(&widgets.chat_list, &sender);
                }
            }
            AiPanelMsg::NewChat => match self.store.new_chat() {
                Ok(_) => {
                    widgets.page_stack.set_visible_child_name(CHAT_PAGE);
                    self.render_all(widgets, &sender);
                    self.publish_snapshot(&sender);
                    widgets.composer.grab_focus();
                }
                Err(ChatStoreError::LimitReached) => widgets
                    .status
                    .set_label("50 chats are already saved. Delete one before creating another."),
                Err(_) => {}
            },
            AiPanelMsg::SelectChat(id) => {
                if self.store.select_chat(id) {
                    self.render_all(widgets, &sender);
                    self.publish_snapshot(&sender);
                }
                widgets.page_stack.set_visible_child_name(CHAT_PAGE);
            }
            AiPanelMsg::SearchChanged(query) => {
                self.search = query.chars().take(1_024).collect();
                self.refresh_library(&widgets.chat_list, &sender);
            }
            AiPanelMsg::ShowLibrary => {
                self.refresh_library(&widgets.chat_list, &sender);
                widgets.page_stack.set_visible_child_name(LIBRARY_PAGE);
                widgets.search.grab_focus();
            }
            AiPanelMsg::ShowChat => {
                widgets.page_stack.set_visible_child_name(CHAT_PAGE);
            }
            AiPanelMsg::Rename(title) => {
                if !self.rendering && self.store.rename_active(&title) {
                    self.refresh_library(&widgets.chat_list, &sender);
                    self.publish_snapshot(&sender);
                }
            }
            AiPanelMsg::ToggleArchive => match self.store.toggle_archive_active() {
                Ok(_) => {
                    self.render_all(widgets, &sender);
                    self.publish_snapshot(&sender);
                }
                Err(ChatStoreError::Busy) => {
                    widgets
                        .status
                        .set_label("Stop this response before archiving the chat.");
                }
                Err(_) => {}
            },
            AiPanelMsg::Delete => {
                let title =
                    crate::review_input::safe_inline_display(self.store.active_title(), 1_024);
                let dialog = adw::AlertDialog::new(
                    Some("Delete this chat?"),
                    Some(&format!(
                        "“{title}” and its saved messages will be permanently removed."
                    )),
                );
                dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");
                dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
                let sender = sender.clone();
                dialog.connect_response(None, move |_, response| {
                    if response == "delete" {
                        sender.input(AiPanelMsg::DeleteConfirmed);
                    }
                });
                dialog.present(Some(root));
            }
            AiPanelMsg::DeleteConfirmed => match self.store.delete_active() {
                Ok(id) => {
                    self.retry_payloads.remove(&id);
                    self.conversation_systems.remove(&id);
                    self.render_all(widgets, &sender);
                    self.publish_snapshot(&sender);
                }
                Err(ChatStoreError::Busy) => {
                    widgets
                        .status
                        .set_label("Stop this response before deleting the chat.");
                }
                Err(_) => {}
            },
            AiPanelMsg::ClearContext => match self.store.clear_active_context() {
                Ok(changed) => {
                    if let Some(payload) = self.retry_payloads.get_mut(&self.store.active_id()) {
                        payload.context = None;
                    }
                    self.render_context(widgets);
                    if changed {
                        self.publish_snapshot(&sender);
                    }
                }
                Err(ChatStoreError::Busy) => {
                    widgets
                        .status
                        .set_label("Stop this response before clearing its context.");
                }
                Err(_) => {}
            },
            AiPanelMsg::IncludeRecent(enabled) => {
                if !self.rendering {
                    self.include_recent.insert(self.store.active_id(), enabled);
                }
            }
            AiPanelMsg::CopyFocused => copy_focused_selection(widgets),
            AiPanelMsg::PasteFocused => {
                paste_focused_text(widgets, self.store.active_archived());
            }
            AiPanelMsg::Close => {
                self.publish_snapshot(&sender);
                let _ = sender.output(AiPanelOutput::CloseRequested);
            }
        }
    }
}

fn editable_text_delegate(editable: &impl IsA<gtk::Editable>) -> Option<gtk::Text> {
    editable.delegate()?.downcast::<gtk::Text>().ok()
}

fn editable_has_focus(editable: &(impl IsA<gtk::Editable> + IsA<gtk::Widget>)) -> bool {
    editable.has_focus() || editable_text_delegate(editable).is_some_and(|text| text.has_focus())
}

fn copy_focused_selection(widgets: &AiPanelModelWidgets) {
    if editable_has_focus(&widgets.title_entry) {
        if let Some(text) = editable_text_delegate(&widgets.title_entry) {
            text.emit_copy_clipboard();
        }
        return;
    }
    if editable_has_focus(&widgets.search) {
        if let Some(text) = editable_text_delegate(&widgets.search) {
            text.emit_copy_clipboard();
        }
        return;
    }
    if widgets.composer.has_focus() {
        widgets.composer.emit_copy_clipboard();
    } else if widgets.transcript.has_focus() {
        widgets.transcript.emit_copy_clipboard();
    } else if widgets.status.has_focus() {
        widgets.status.emit_copy_clipboard();
    }
}

fn paste_focused_text(widgets: &AiPanelModelWidgets, archived: bool) {
    if editable_has_focus(&widgets.title_entry) {
        if let Some(text) = editable_text_delegate(&widgets.title_entry) {
            text.emit_paste_clipboard();
        }
        return;
    }
    if editable_has_focus(&widgets.search) {
        if let Some(text) = editable_text_delegate(&widgets.search) {
            text.emit_paste_clipboard();
        }
        return;
    }
    if widgets.composer.has_focus() && !archived {
        widgets.composer.emit_paste_clipboard();
    }
}

impl AiPanelModel {
    fn start_request(
        &mut self,
        widgets: &mut AiPanelModelWidgets,
        sender: &ComponentSender<Self>,
        mut payload: RequestPayload,
        clear_composer: bool,
    ) -> bool {
        if payload.user_text.trim().is_empty() {
            widgets.status.set_label("Message is empty.");
            return false;
        }
        if payload.user_text.len() > MAX_LIVE_MESSAGE_BYTES {
            widgets
                .status
                .set_label("Message is too large (64 KiB limit).");
            return false;
        }
        let Some(client) = self.client.clone() else {
            widgets.status.set_label("No AI provider is configured.");
            return false;
        };
        payload.user_text = payload.user_text.trim().to_string();
        let provider = crate::review_input::safe_inline_display(&client.display_name(), 256);
        let start = match self.store.begin_turn(
            payload.user_text.clone(),
            payload.context.clone(),
            format!("Thinking… ({provider})"),
            payload.restore_pending_as_draft,
        ) {
            Ok(start) => start,
            Err(ChatStoreError::Archived) => {
                widgets
                    .status
                    .set_label("Unarchive this chat before sending.");
                return false;
            }
            Err(ChatStoreError::Busy) => return false,
            Err(ChatStoreError::EmptyMessage) => {
                widgets.status.set_label("Message is empty.");
                return false;
            }
            Err(ChatStoreError::MessageTooLarge) => {
                widgets
                    .status
                    .set_label("Message is too large (64 KiB limit).");
                return false;
            }
            Err(_) => return false,
        };
        if clear_composer {
            self.rendering = true;
            widgets.composer.buffer().set_text("");
            self.rendering = false;
            self.store.set_active_draft(String::new());
        }

        let mut request_history = start.history;
        let recent = if payload.context.is_none() && widgets.include_recent.is_active() {
            self.recent_context()
        } else {
            None
        };
        let (new_system, api_user) = if let Some(context) = start.effective_context.as_ref() {
            ai::build_block_chat_prompt(&payload.user_text, context)
        } else {
            ai::build_session_prompt(&payload.user_text, recent.as_deref())
        };
        if let Some(last) = request_history
            .iter_mut()
            .rev()
            .find(|turn| turn.role == ai::Role::User)
        {
            last.text = api_user;
        }
        let system = self
            .conversation_systems
            .entry(start.token.chat_id)
            .or_insert(new_system)
            .clone();
        let token = start.token;
        self.retry_payloads.insert(token.chat_id, payload);
        let handle = if self.stream {
            let delta_sender = sender.clone();
            let done_sender = sender.clone();
            ai::ask_turns_streaming(
                client,
                system,
                request_history,
                move |text| delta_sender.input(AiPanelMsg::Delta { token, text }),
                move |result| done_sender.input(AiPanelMsg::Result { token, result }),
            )
        } else {
            let done_sender = sender.clone();
            ai::ask_turns(client, system, request_history, move |result| {
                done_sender.input(AiPanelMsg::Result { token, result });
            })
        };
        self.requests.insert(token, handle);
        self.render_all(widgets, sender);
        self.publish_snapshot(sender);
        true
    }

    fn cancel_all(&mut self) {
        let requests = std::mem::take(&mut self.requests);
        for (token, handle) in requests {
            handle.cancel();
            let _ = self
                .store
                .cancel_request(token, "Request cancelled during restore.".into());
        }
    }

    fn publish_snapshot(&self, sender: &ComponentSender<Self>) {
        let mut durable = self.store.clone();
        for (chat_id, payload) in &self.retry_payloads {
            durable.recover_retry_payload(*chat_id, &payload.user_text, payload.context.clone());
        }
        let Ok(mut snapshot) = durable.snapshot(self.redact_secrets) else {
            return;
        };
        if snapshot
            .compact_to_measured_limit(SESSION_SNAPSHOT_AI_BUDGET, |candidate| {
                candidate.to_json().ok().map(|encoded| encoded.len())
            })
            .is_none()
        {
            return;
        }
        if let Ok(encoded) = snapshot.to_json() {
            let _ = sender.output(AiPanelOutput::SnapshotChanged(encoded));
        }
    }

    fn render_all(&mut self, widgets: &mut AiPanelModelWidgets, sender: &ComponentSender<Self>) {
        self.rendering = true;
        widgets.title_entry.set_text(self.store.active_title());
        widgets
            .archive_button
            .set_label(if self.store.active_archived() {
                "Unarchive"
            } else {
                "Archive"
            });
        widgets.include_recent.set_active(
            *self
                .include_recent
                .entry(self.store.active_id())
                .or_insert(true),
        );
        widgets
            .composer
            .buffer()
            .set_text(self.store.active_draft());
        self.rendering = false;
        self.render_transcript(widgets);
        self.render_context(widgets);
        self.render_status(widgets);
        self.refresh_library(&widgets.chat_list, sender);
    }

    fn render_transcript(&self, widgets: &AiPanelModelWidgets) {
        let buffer = widgets.transcript.buffer();
        buffer.set_text("");
        for turn in self.store.active_history() {
            append_transcript(
                &widgets.transcript,
                if turn.role == ai::Role::User {
                    "You"
                } else {
                    "Assistant"
                },
                &turn.text,
            );
        }
        if !self.store.active_partial().is_empty() {
            append_transcript(
                &widgets.transcript,
                "Assistant",
                self.store.active_partial(),
            );
        }
        scroll_to_end(&widgets.transcript);
    }

    fn render_context(&self, widgets: &AiPanelModelWidgets) {
        let context = self
            .retry_payloads
            .get(&self.store.active_id())
            .and_then(|payload| payload.context.as_ref())
            .or_else(|| self.store.active_context());
        if let Some(context) = context {
            let command = crate::review_input::safe_inline_display(&context.cmd, 4 * 1024);
            widgets
                .context_label
                .set_label(&format!("Block: {command} (exit {})", context.exit_code));
            widgets.context_row.set_visible(true);
        } else {
            widgets.context_row.set_visible(false);
        }
    }

    fn render_status(&self, widgets: &AiPanelModelWidgets) {
        let busy = self.store.active_request_token().is_some();
        let status = match self.store.active_status() {
            ChatStatus::Idle => "",
            ChatStatus::Thinking(text) | ChatStatus::Info(text) | ChatStatus::Error(text) => text,
        };
        widgets.status.set_label(status);
        widgets.spinner.set_visible(busy);
        if busy {
            widgets.spinner.start();
        } else {
            widgets.spinner.stop();
        }
        widgets.stop_button.set_visible(busy);
        widgets.send_button.set_visible(!busy);
        widgets
            .send_button
            .set_sensitive(!self.store.active_archived());
        widgets
            .retry_button
            .set_visible(!busy && self.retry_payloads.contains_key(&self.store.active_id()));
    }

    fn refresh_library(&self, list: &gtk::ListBox, sender: &ComponentSender<Self>) {
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        for summary in self.store.summaries(&self.search) {
            let row = gtk::Button::new();
            row.add_css_class("flat");
            let body = gtk::Box::new(gtk::Orientation::Vertical, 2);
            let title = gtk::Label::new(Some(&summary.title));
            title.set_halign(gtk::Align::Start);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            if summary.active {
                title.add_css_class("heading");
            }
            let mut meta = summary.preview;
            if summary.archived {
                meta.push_str(" · Archived");
            }
            if summary.busy {
                meta.push_str(" · Thinking…");
            } else if summary.error {
                meta.push_str(" · Error");
            } else if summary.unread {
                meta.push_str(" · New reply");
            }
            let preview = gtk::Label::new(Some(&meta));
            preview.set_halign(gtk::Align::Start);
            preview.set_ellipsize(gtk::pango::EllipsizeMode::End);
            preview.add_css_class("dim-label");
            body.append(&title);
            body.append(&preview);
            row.set_child(Some(&body));
            let id = summary.id;
            let sender = sender.clone();
            row.connect_clicked(move |_| sender.input(AiPanelMsg::SelectChat(id)));
            list.append(&row);
        }
    }

    fn recent_context(&self) -> Option<String> {
        let path = self.history_path.as_deref()?;
        let items = palette::read_history(std::path::Path::new(path), 5);
        if items.is_empty() {
            return None;
        }
        Some(
            items
                .iter()
                .rev()
                .map(|item| format!("$ {} (exit {})", item.command, item.exit_code))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

fn text_view_text(view: &gtk::TextView) -> String {
    let buffer = view.buffer();
    let (start, end) = buffer.bounds();
    buffer.text(&start, &end, true).to_string()
}

fn draft_without_retry_message(retry: &str, draft: &str) -> String {
    if draft == retry {
        return String::new();
    }
    draft
        .strip_prefix(retry)
        .and_then(|rest| rest.strip_prefix("\n\n"))
        .map_or_else(|| draft.to_string(), str::to_string)
}

fn append_transcript(view: &gtk::TextView, label: &str, body: &str) {
    let buffer = view.buffer();
    let mut end = buffer.end_iter();
    if buffer.char_count() > 0 {
        buffer.insert(&mut end, "\n\n");
    }
    buffer.insert(&mut end, label);
    buffer.insert(&mut end, "\n");
    buffer.insert(&mut end, body);
}

fn scroll_to_end(view: &gtk::TextView) {
    let view = view.clone();
    gtk::glib::idle_add_local_once(move || {
        let mut end = view.buffer().end_iter();
        view.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_panel_icon_buttons_have_distinct_accessible_labels() {
        let labels = [
            NEW_CHAT_LABEL,
            CLOSE_AI_PANEL_LABEL,
            CLEAR_BLOCK_CONTEXT_LABEL,
            BACK_TO_CONVERSATION_LABEL,
        ];
        assert!(labels.iter().all(|label| !label.is_empty()));
        for (index, label) in labels.iter().enumerate() {
            assert!(!labels[..index].contains(label), "duplicate label: {label}");
        }
    }

    #[test]
    fn session_embedding_budget_is_below_the_outer_snapshot_limit() {
        const { assert!(SESSION_SNAPSHOT_AI_BUDGET < 4 * 1024 * 1024) }
    }

    #[test]
    fn request_token_can_key_parallel_request_maps() {
        let one = RequestToken {
            chat_id: 1,
            epoch: 2,
        };
        let two = RequestToken {
            chat_id: 2,
            epoch: 2,
        };
        let mut payloads = HashMap::new();
        payloads.insert(one, "one");
        payloads.insert(two, "two");
        assert_eq!(payloads[&one], "one");
        assert_eq!(payloads[&two], "two");
    }

    #[test]
    fn retry_removes_only_the_recovered_prefix_from_the_draft() {
        assert_eq!(draft_without_retry_message("failed", "failed"), "");
        assert_eq!(
            draft_without_retry_message("failed", "failed\n\nfollow-up"),
            "follow-up"
        );
        assert_eq!(
            draft_without_retry_message("failed", "edited failed"),
            "edited failed"
        );
    }

    #[test]
    fn composer_enter_semantics_match_chat_conventions() {
        use gtk::gdk::{Key, ModifierType};

        let cases = [
            (Key::Return, ModifierType::empty(), ComposerKeyAction::Send),
            (
                Key::Return,
                ModifierType::CONTROL_MASK,
                ComposerKeyAction::Send,
            ),
            (
                Key::KP_Enter,
                ModifierType::CONTROL_MASK | ModifierType::LOCK_MASK,
                ComposerKeyAction::Send,
            ),
            (
                Key::Return,
                ModifierType::SHIFT_MASK,
                ComposerKeyAction::Newline,
            ),
            (
                Key::KP_Enter,
                ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK,
                ComposerKeyAction::Newline,
            ),
            (
                Key::Return,
                ModifierType::ALT_MASK,
                ComposerKeyAction::Proceed,
            ),
            (Key::a, ModifierType::empty(), ComposerKeyAction::Proceed),
        ];

        for (key, state, expected) in cases {
            assert_eq!(classify_composer_key(key, state), expected);
        }
    }
}
