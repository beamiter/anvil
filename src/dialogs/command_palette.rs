//! Relm4 command-palette component backed by a factory list.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender, FactoryVecDeque};
use relm4::gtk;
use relm4::prelude::*;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use crate::command_history::CommandHistoryRecord;
use crate::keybindings::{Action, KeybindingMap};
use crate::palette::{self as palette_data, Accept, Entry, PaletteMode, Query};
use crate::workflows::Workflow;

#[derive(Debug)]
struct PaletteRow {
    label: String,
    sublabel: Option<String>,
    right: Option<String>,
    accept: Accept,
}

#[derive(Debug)]
enum PaletteRowOutput {
    Activate(Accept),
}

#[relm4::factory]
impl FactoryComponent for PaletteRow {
    type Init = Entry;
    type Input = ();
    type Output = PaletteRowOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::ListBox;

    view! {
        root = adw::ActionRow {
            set_title: &escape_markup(&self.label),
            set_subtitle: &escape_markup(self.sublabel.as_deref().unwrap_or("")),
            set_activatable: true,
            connect_activated[sender, accept = self.accept.clone()] => move |_| {
                let _ = sender.output(PaletteRowOutput::Activate(accept.clone()));
            },

            add_suffix = &gtk::Label {
                set_label: self.right.as_deref().unwrap_or(""),
                set_visible: self.right.is_some(),
                add_css_class: "dim-label",
            },
        }
    }

    fn init_model(entry: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self {
            label: entry.label,
            sublabel: entry.sublabel,
            right: entry.right,
            accept: entry.accept,
        }
    }
}

pub(crate) struct PaletteInit {
    pub(crate) parent: adw::ApplicationWindow,
    pub(crate) keybindings: Rc<RefCell<KeybindingMap>>,
    pub(crate) workflows: Rc<RefCell<Vec<Workflow>>>,
}

#[derive(Debug)]
pub(crate) enum PaletteMsg {
    Toggle {
        mode: PaletteMode,
        history_path: Option<PathBuf>,
        /// Current Block commands, already newest-first. The component merges
        /// these into its persisted snapshot only when the dialog opens.
        live_history: Vec<String>,
    },
    Search(String),
    /// The shared workflow cache was replaced in the app model. Rebuild only
    /// when this dialog is still presented; a closed palette will consume the
    /// same cache the next time it opens.
    WorkflowsChanged,
    /// A bounded command-history snapshot produced for one particular dialog
    /// opening. The generation prevents a slow read from populating a later
    /// opening after Close/reopen.
    HistorySnapshotReady {
        generation: u64,
        result: Result<Vec<CommandHistoryRecord>, String>,
    },
    Activate(Accept),
    Move(i32),
    AcceptSelected,
    Close,
    Closed,
}

#[derive(Debug)]
pub(crate) enum PaletteOutput {
    Action(Action),
    TypeCommand(String),
    AskAi(String),
    RunWorkflow(PathBuf),
}

#[derive(Debug, Default)]
struct PaletteOpeningGeneration {
    sequence: u64,
    active: Option<u64>,
}

impl PaletteOpeningGeneration {
    fn open(&mut self) -> u64 {
        // Reusing 1 after overflow is safe in practice: retaining a worker
        // across 2^64 distinct dialog openings is physically impossible.
        self.sequence = self.sequence.checked_add(1).unwrap_or(1);
        self.active = Some(self.sequence);
        self.sequence
    }

    fn close(&mut self) {
        self.active = None;
    }

    fn accepts(&self, generation: u64) -> bool {
        self.active == Some(generation)
    }

    fn active(&self) -> Option<u64> {
        self.active
    }
}

#[derive(Debug)]
struct HistoryLoadRequest {
    generation: u64,
    history_path: Option<PathBuf>,
    live_history: Vec<String>,
}

#[derive(Debug, Default)]
struct HistoryLoadState {
    in_flight: Option<u64>,
    pending: Option<HistoryLoadRequest>,
}

struct HistoryLoadCompletion {
    recognized: bool,
    next: Option<HistoryLoadRequest>,
}

impl HistoryLoadState {
    /// Admit a request immediately when idle, otherwise retain only the most
    /// recent opening. The caller must spawn every request returned here.
    fn submit(&mut self, request: HistoryLoadRequest) -> Option<HistoryLoadRequest> {
        if self.in_flight.is_some() {
            self.pending = Some(request);
            None
        } else {
            self.in_flight = Some(request.generation);
            Some(request)
        }
    }

    /// Release the completed worker and admit only the latest request when it
    /// still belongs to the active dialog opening.
    fn complete(
        &mut self,
        generation: u64,
        active_generation: Option<u64>,
    ) -> HistoryLoadCompletion {
        if self.in_flight != Some(generation) {
            return HistoryLoadCompletion {
                recognized: false,
                next: None,
            };
        }

        self.in_flight = None;
        let next = self
            .pending
            .take()
            .filter(|request| Some(request.generation) == active_generation);
        if let Some(request) = next.as_ref() {
            self.in_flight = Some(request.generation);
        }
        HistoryLoadCompletion {
            recognized: true,
            next,
        }
    }

    fn spawn_failed(&mut self, generation: u64) {
        if self.in_flight == Some(generation) {
            self.in_flight = None;
        }
    }

    fn close(&mut self) {
        self.pending = None;
    }
}

pub(crate) struct PaletteModel {
    parent: adw::ApplicationWindow,
    keybindings: Rc<RefCell<KeybindingMap>>,
    workflows: Rc<RefCell<Vec<Workflow>>>,
    mode: PaletteMode,
    history_snapshot: Vec<CommandHistoryRecord>,
    opening: PaletteOpeningGeneration,
    history_load: HistoryLoadState,
    query: String,
    rows: FactoryVecDeque<PaletteRow>,
}

#[relm4::component(pub(crate))]
impl Component for PaletteModel {
    type Init = PaletteInit;
    type Input = PaletteMsg;
    type Output = PaletteOutput;
    type CommandOutput = ();

    view! {
        root = adw::Dialog {
            set_title: "Palette",
            set_content_width: 560,
            set_content_height: 520,
            connect_closed => PaletteMsg::Closed,

            #[wrap(Some)]
            set_child = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},

                #[wrap(Some)]
                set_content = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,

                    #[name(filter_entry)]
                    gtk::SearchEntry {
                        set_hexpand: true,
                        set_margin_all: 12,
                        connect_search_changed[sender] => move |entry| {
                            sender.input(PaletteMsg::Search(entry.text().to_string()));
                        },
                    },

                    #[name(history_status)]
                    gtk::Label {
                        set_visible: false,
                        set_xalign: 0.0,
                        set_wrap: true,
                        set_margin_start: 12,
                        set_margin_end: 12,
                        set_margin_bottom: 6,
                        add_css_class: "dim-label",
                    },

                    gtk::ScrolledWindow {
                        set_hexpand: true,
                        set_vexpand: true,

                        #[local_ref]
                        list_box -> gtk::ListBox {
                            set_selection_mode: gtk::SelectionMode::Single,
                            add_css_class: "boxed-list",
                            set_margin_start: 12,
                            set_margin_end: 12,
                            set_margin_bottom: 12,
                        },
                    },
                },
            },

            add_controller = gtk::EventControllerKey {
                set_propagation_phase: gtk::PropagationPhase::Capture,
                connect_key_pressed[sender] => move |_, key, _, state| {
                    use gtk::gdk::{Key, ModifierType};
                    let close_shortcut = matches!(key, Key::P | Key::p)
                        && state.contains(
                            ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK
                        );
                    let message = match key {
                        Key::Escape => Some(PaletteMsg::Close),
                        Key::Return | Key::KP_Enter => Some(PaletteMsg::AcceptSelected),
                        Key::Down => Some(PaletteMsg::Move(1)),
                        Key::Up => Some(PaletteMsg::Move(-1)),
                        _ if close_shortcut => Some(PaletteMsg::Close),
                        _ => None,
                    };
                    if let Some(message) = message {
                        sender.input(message);
                        gtk::glib::Propagation::Stop
                    } else {
                        gtk::glib::Propagation::Proceed
                    }
                },
            },
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let rows =
            FactoryVecDeque::builder()
                .launch_default()
                .forward(sender.input_sender(), |output| match output {
                    PaletteRowOutput::Activate(accept) => PaletteMsg::Activate(accept),
                });
        let model = Self {
            parent: init.parent,
            keybindings: init.keybindings,
            workflows: init.workflows,
            mode: PaletteMode::All,
            history_snapshot: Vec::new(),
            opening: PaletteOpeningGeneration::default(),
            history_load: HistoryLoadState::default(),
            query: String::new(),
            rows,
        };
        let list_box = model.rows.widget();
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
            PaletteMsg::Toggle {
                mode,
                history_path,
                live_history,
            } => {
                if root.parent().is_some() {
                    self.opening.close();
                    self.history_load.close();
                    root.force_close();
                    return;
                }
                let generation = self.opening.open();
                self.mode = mode;
                // History is pane/path specific. Do not briefly expose a stale
                // prior opening while its replacement loads; actions and the
                // workflow cache are enough to present the palette immediately.
                self.history_snapshot.clear();
                self.query.clear();
                root.set_title(title(mode));
                widgets
                    .filter_entry
                    .set_placeholder_text(Some(placeholder(mode)));
                widgets.filter_entry.set_text("");
                self.rebuild_rows();
                self.select_first(widgets);
                root.present(Some(&self.parent));
                widgets.filter_entry.grab_focus();

                if history_path.is_none() && live_history.is_empty() {
                    widgets.history_status.set_visible(false);
                    return;
                }
                widgets.history_status.set_label("Loading command history…");
                widgets.history_status.set_visible(true);
                let request = HistoryLoadRequest {
                    generation,
                    history_path,
                    live_history,
                };
                if let Some(request) = self.history_load.submit(request) {
                    self.spawn_admitted_history_load(request, &sender, widgets);
                }
            }
            PaletteMsg::Search(query) => {
                self.query = query;
                self.rebuild_rows();
                self.select_first(widgets);
            }
            PaletteMsg::WorkflowsChanged => {
                if root.parent().is_some()
                    && matches!(self.mode, PaletteMode::All | PaletteMode::Workflows)
                {
                    self.rebuild_rows();
                    self.select_first(widgets);
                }
            }
            PaletteMsg::HistorySnapshotReady { generation, result } => {
                let completion = self
                    .history_load
                    .complete(generation, self.opening.active());
                if !completion.recognized {
                    return;
                }

                if self.opening.accepts(generation) && root.parent().is_some() {
                    match result {
                        Ok(snapshot) => {
                            self.history_snapshot = snapshot;
                            widgets.history_status.set_visible(false);
                            self.rebuild_rows();
                            self.select_first(widgets);
                        }
                        Err(error) => {
                            log::error!("palette history worker failed: {error}");
                            widgets.history_status.set_label(
                                "Command history could not be loaded; actions and workflows remain available.",
                            );
                            widgets.history_status.set_visible(true);
                        }
                    }
                }

                if let Some(request) = completion.next {
                    self.spawn_admitted_history_load(request, &sender, widgets);
                }
            }
            PaletteMsg::Activate(accept) => self.accept(accept, &sender, root),
            PaletteMsg::Move(delta) => {
                let len = self.rows.len() as i32;
                if len == 0 {
                    return;
                }
                let current = widgets
                    .list_box
                    .selected_row()
                    .map(|row| row.index())
                    .unwrap_or(0);
                let next = (current + delta).clamp(0, len - 1);
                if let Some(row) = widgets.list_box.row_at_index(next) {
                    widgets.list_box.select_row(Some(&row));
                }
            }
            PaletteMsg::AcceptSelected => {
                let Some(row) = widgets.list_box.selected_row() else {
                    return;
                };
                let accept = self.rows.guard()[row.index() as usize].accept.clone();
                self.accept(accept, &sender, root);
            }
            PaletteMsg::Close => {
                self.opening.close();
                self.history_load.close();
                root.force_close();
            }
            PaletteMsg::Closed => {
                self.opening.close();
                self.history_load.close();
                self.history_snapshot.clear();
                widgets.history_status.set_visible(false);
            }
        }
    }
}

impl PaletteModel {
    fn spawn_admitted_history_load(
        &mut self,
        request: HistoryLoadRequest,
        sender: &ComponentSender<Self>,
        widgets: &PaletteModelWidgets,
    ) {
        let generation = request.generation;
        if let Err(error) = spawn_history_snapshot_load(sender.clone(), request) {
            self.history_load.spawn_failed(generation);
            let error = crate::review_input::safe_inline_display(&error.to_string(), 1024);
            log::warn!("could not start palette history worker: {error}");
            if self.opening.accepts(generation) {
                widgets.history_status.set_label(&format!(
                    "Command history could not load in the background; actions and workflows remain available. {error}"
                ));
                widgets.history_status.set_visible(true);
            }
        }
    }

    fn rebuild_rows(&mut self) {
        let query = Query::parse(&self.query, self.mode);
        let entries = palette_data::gather(
            &query,
            &self.keybindings.borrow(),
            &self.history_snapshot,
            &self.workflows.borrow(),
            200,
        );
        let mut rows = self.rows.guard();
        rows.clear();
        for entry in entries {
            rows.push_back(entry);
        }
    }

    fn select_first(&self, widgets: &PaletteModelWidgets) {
        widgets
            .list_box
            .select_row(widgets.list_box.row_at_index(0).as_ref());
    }

    fn accept(&mut self, accept: Accept, sender: &ComponentSender<Self>, root: &adw::Dialog) {
        self.opening.close();
        self.history_load.close();
        root.force_close();
        let output = match accept {
            Accept::Action(action) => PaletteOutput::Action(action),
            Accept::TypeCommand(command) => PaletteOutput::TypeCommand(command),
            Accept::AskAi(query) => PaletteOutput::AskAi(query),
            Accept::RunWorkflow(path) => PaletteOutput::RunWorkflow(path),
        };
        let _ = sender.output(output);
    }
}

fn spawn_history_snapshot_load(
    sender: ComponentSender<PaletteModel>,
    request: HistoryLoadRequest,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("anvil-palette-history".to_string())
        .spawn(move || {
            let HistoryLoadRequest {
                generation,
                history_path,
                live_history,
            } = request;
            let result = std::panic::catch_unwind(|| {
                palette_data::load_history_snapshot(history_path.as_deref(), &live_history)
            })
            .map_err(|_| "command-history loader panicked".to_string());
            sender.input(PaletteMsg::HistorySnapshotReady { generation, result });
        })
        .map(|_| ())
}

fn title(mode: PaletteMode) -> &'static str {
    match mode {
        PaletteMode::All => "Palette",
        PaletteMode::Commands => "Command Palette",
        PaletteMode::History => "History",
        PaletteMode::Ai => "Ask AI",
        PaletteMode::Workflows => "Workflows",
    }
}

fn placeholder(mode: PaletteMode) -> &'static str {
    match mode {
        PaletteMode::All => "Search everything…  (> commands, @ history, : workflows, ? AI)",
        PaletteMode::Commands => "Search commands…  (@ history, : workflows, ? AI)",
        PaletteMode::History => "Search history…  (> commands, : workflows, ? AI)",
        PaletteMode::Ai => "Describe what you want…",
        PaletteMode::Workflows => "Search workflows…  (> commands, @ history)",
    }
}

fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::{HistoryLoadRequest, HistoryLoadState, PaletteOpeningGeneration};

    fn history_request(generation: u64) -> HistoryLoadRequest {
        HistoryLoadRequest {
            generation,
            history_path: None,
            live_history: vec![format!("command-{generation}")],
        }
    }

    #[test]
    fn history_result_is_admitted_only_for_the_current_opening() {
        let mut opening = PaletteOpeningGeneration::default();
        let first = opening.open();
        assert!(opening.accepts(first));

        let second = opening.open();
        assert_ne!(first, second);
        assert!(!opening.accepts(first));
        assert!(opening.accepts(second));
    }

    #[test]
    fn close_and_reopen_discard_a_late_history_result() {
        let mut opening = PaletteOpeningGeneration::default();
        let closed = opening.open();
        opening.close();
        assert!(!opening.accepts(closed));

        let reopened = opening.open();
        assert!(!opening.accepts(closed));
        assert!(opening.accepts(reopened));
    }

    #[test]
    fn history_loader_is_single_flight_and_close_clears_pending() {
        let mut loads = HistoryLoadState::default();
        let first = loads.submit(history_request(1)).unwrap();
        assert_eq!(first.generation, 1);
        assert_eq!(loads.in_flight, Some(1));

        assert!(loads.submit(history_request(2)).is_none());
        assert_eq!(loads.in_flight, Some(1));
        assert_eq!(loads.pending.as_ref().unwrap().generation, 2);

        loads.close();
        assert!(loads.pending.is_none());
        let completion = loads.complete(1, None);
        assert!(completion.recognized);
        assert!(completion.next.is_none());
        assert!(loads.in_flight.is_none());
    }

    #[test]
    fn history_loader_pending_request_is_latest_wins() {
        let mut loads = HistoryLoadState::default();
        assert!(loads.submit(history_request(10)).is_some());
        assert!(loads.submit(history_request(11)).is_none());
        assert!(loads.submit(history_request(12)).is_none());
        assert_eq!(loads.in_flight, Some(10));
        assert_eq!(loads.pending.as_ref().unwrap().generation, 12);

        let completion = loads.complete(10, Some(12));
        assert!(completion.recognized);
        assert_eq!(completion.next.unwrap().generation, 12);
        assert_eq!(loads.in_flight, Some(12));
        assert!(loads.pending.is_none());
    }
}
