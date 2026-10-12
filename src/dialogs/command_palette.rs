//! Relm4 command-palette component backed by a factory list.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender, FactoryVecDeque};
use relm4::gtk;
use relm4::prelude::*;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use crate::command_history::CommandHistoryRecord;
use crate::keybindings::{Action, KeybindingMap};
use crate::palette::{self as palette_data, Accept, Entry, PaletteMode, Query};
use crate::workflows::Workflow;

#[derive(Debug)]
struct PaletteRowInit {
    entry: Entry,
    generation: u64,
}

#[derive(Debug)]
struct PaletteRow {
    generation: u64,
    label: String,
    sublabel: Option<String>,
    right: Option<String>,
    accept: Accept,
}

#[derive(Debug)]
enum PaletteRowOutput {
    Activate(u64, Accept),
}

#[relm4::factory]
impl FactoryComponent for PaletteRow {
    type Init = PaletteRowInit;
    type Input = ();
    type Output = PaletteRowOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::ListBox;

    view! {
        root = adw::ActionRow {
            set_title: &escape_markup(&self.label),
            set_subtitle: &escape_markup(self.sublabel.as_deref().unwrap_or("")),
            set_activatable: true,
            connect_activated[sender, accept = self.accept.clone(), generation = self.generation] => move |_| {
                let _ = sender.output(PaletteRowOutput::Activate(generation, accept.clone()));
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
            generation: entry.generation,
            label: entry.entry.label,
            sublabel: entry.entry.sublabel,
            right: entry.entry.right,
            accept: entry.entry.accept,
        }
    }
}

pub(crate) struct PaletteInit {
    pub(crate) parent: gtk::Window,
    pub(crate) keybindings: Rc<RefCell<KeybindingMap>>,
    pub(crate) workflows: Rc<RefCell<Vec<Workflow>>>,
}

#[derive(Debug)]
pub(crate) enum PaletteMsg {
    Toggle {
        workflow_target: Option<u64>,
        mode: PaletteMode,
        history_path: Option<PathBuf>,
        /// Current Block commands, already newest-first. The component merges
        /// these into its persisted snapshot only when the dialog opens.
        live_history: Vec<String>,
    },
    Search(u64, String),
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
    Activate(u64, Accept),
    Move(u64, i32),
    AcceptSelected(u64),
    Close(u64),
    Closed(u64),
}

#[derive(Debug)]
pub(crate) enum PaletteOutput {
    Action(Action),
    AsciiOrganismHello(Option<u64>),
    SetVteCompanionExpanded(Option<u64>, bool),
    TypeCommand(String),
    AskAi(String),
    RunWorkflow(PathBuf, Option<u64>),
}

#[derive(Debug, Default)]
struct PaletteOpeningGeneration {
    sequence: u64,
    active: Rc<Cell<Option<u64>>>,
}

impl PaletteOpeningGeneration {
    fn open(&mut self) -> u64 {
        // Reusing 1 after overflow is safe in practice: retaining a worker
        // across 2^64 distinct dialog openings is physically impossible.
        self.sequence = self.sequence.checked_add(1).unwrap_or(1);
        self.active.set(Some(self.sequence));
        self.sequence
    }

    fn close(&mut self) {
        self.active.set(None);
    }

    fn accepts(&self, generation: u64) -> bool {
        self.active.get() == Some(generation)
    }

    fn active(&self) -> Option<u64> {
        self.active.get()
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
    parent: gtk::Window,
    keybindings: Rc<RefCell<KeybindingMap>>,
    workflows: Rc<RefCell<Vec<Workflow>>>,
    mode: PaletteMode,
    history_snapshot: Vec<CommandHistoryRecord>,
    opening: PaletteOpeningGeneration,
    workflow_target: Option<u64>,
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
            connect_closed[sender, generation = model.opening.active.clone()] => move |_| {
                if let Some(generation) = generation.get() {
                    sender.input(PaletteMsg::Closed(generation));
                }
            },

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
                        connect_search_changed[sender, generation = model.opening.active.clone()] => move |entry| {
                            if let Some(generation) = generation.get() {
                                sender.input(PaletteMsg::Search(generation, entry.text().to_string()));
                            }
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
                set_name: Some("anvil-palette-keyboard"),
                set_propagation_phase: gtk::PropagationPhase::Capture,
                connect_key_pressed[sender, generation = model.opening.active.clone()] => move |_, key, _, state| {
                    let Some(generation) = generation.get() else {
                        return gtk::glib::Propagation::Proceed;
                    };
                    use gtk::gdk::{Key, ModifierType};
                    let close_shortcut = matches!(key, Key::P | Key::p)
                        && state.contains(
                            ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK
                        );
                    let message = match key {
                        Key::Escape => Some(PaletteMsg::Close(generation)),
                        Key::Return | Key::KP_Enter => Some(PaletteMsg::AcceptSelected(generation)),
                        Key::Down => Some(PaletteMsg::Move(generation, 1)),
                        Key::Up => Some(PaletteMsg::Move(generation, -1)),
                        _ if close_shortcut => Some(PaletteMsg::Close(generation)),
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
                    PaletteRowOutput::Activate(generation, accept) => {
                        PaletteMsg::Activate(generation, accept)
                    }
                });
        let model = Self {
            parent: init.parent,
            keybindings: init.keybindings,
            workflows: init.workflows,
            mode: PaletteMode::All,
            history_snapshot: Vec::new(),
            opening: PaletteOpeningGeneration::default(),
            workflow_target: None,
            history_load: HistoryLoadState::default(),
            query: String::new(),
            rows,
        };
        let list_box = model.rows.widget();
        crate::enter_ownership::install_widget(&root);
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
        let generation = match &msg {
            PaletteMsg::Search(generation, _)
            | PaletteMsg::Activate(generation, _)
            | PaletteMsg::Move(generation, _)
            | PaletteMsg::AcceptSelected(generation)
            | PaletteMsg::Close(generation)
            | PaletteMsg::Closed(generation) => Some(*generation),
            _ => None,
        };
        if generation.is_some_and(|generation| !self.opening.accepts(generation)) {
            return;
        }
        match msg {
            PaletteMsg::Toggle {
                workflow_target,
                mode,
                history_path,
                live_history,
            } => {
                if root.parent().is_some() {
                    self.close_opening(widgets);
                    root.force_close();
                    return;
                }
                let generation = self.opening.open();
                self.workflow_target = workflow_target;
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
            PaletteMsg::Search(_, query) => {
                self.query = query;
                self.rebuild_rows();
                self.select_first(widgets);
            }
            PaletteMsg::WorkflowsChanged => {
                if root.parent().is_some() && query_uses_workflows(&self.query, self.mode) {
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
            PaletteMsg::Activate(_, accept) => self.accept(accept, &sender, root, widgets),
            PaletteMsg::Move(_, delta) => {
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
            PaletteMsg::AcceptSelected(_) => {
                // SearchEntry's delayed Search message may still be pending.
                // Refresh model and rows synchronously before resolving Enter.
                let query = widgets.filter_entry.text().to_string();
                if self.query != query {
                    self.query = query;
                    self.rebuild_rows();
                    self.select_first(widgets);
                }
                let Some(row) = widgets.list_box.selected_row() else {
                    return;
                };
                let accept = self.rows.guard()[row.index() as usize].accept.clone();
                self.accept(accept, &sender, root, widgets);
            }
            PaletteMsg::Close(_) => {
                self.close_opening(widgets);
                root.force_close();
            }
            PaletteMsg::Closed(_) => {
                self.close_opening(widgets);
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
        let Some(generation) = self.opening.active() else {
            return;
        };
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
            rows.push_back(PaletteRowInit { entry, generation });
        }
    }

    fn select_first(&self, widgets: &PaletteModelWidgets) {
        widgets
            .list_box
            .select_row(widgets.list_box.row_at_index(0).as_ref());
    }

    fn close_opening(&mut self, widgets: &PaletteModelWidgets) {
        self.opening.close();
        self.history_load.close();
        self.history_snapshot.clear();
        self.workflow_target = None;
        widgets.history_status.set_visible(false);
    }

    fn accept(
        &mut self,
        accept: Accept,
        sender: &ComponentSender<Self>,
        root: &adw::Dialog,
        widgets: &PaletteModelWidgets,
    ) {
        let workflow_target = self.workflow_target;
        crate::enter_ownership::claim_held();
        self.close_opening(widgets);
        root.force_close();
        let output = match accept {
            Accept::Action(Action::AsciiOrganismHello) => {
                PaletteOutput::AsciiOrganismHello(workflow_target)
            }
            Accept::Action(Action::ExpandVteCompanion) => {
                PaletteOutput::SetVteCompanionExpanded(workflow_target, true)
            }
            Accept::Action(Action::CollapseVteCompanion) => {
                PaletteOutput::SetVteCompanionExpanded(workflow_target, false)
            }
            Accept::Action(action) => PaletteOutput::Action(action),
            Accept::TypeCommand(command) => PaletteOutput::TypeCommand(command),
            Accept::AskAi(query) => PaletteOutput::AskAi(query),
            Accept::RunWorkflow(path) => PaletteOutput::RunWorkflow(path, workflow_target),
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

fn query_uses_workflows(query: &str, default_mode: PaletteMode) -> bool {
    matches!(
        Query::parse(query, default_mode).mode,
        PaletteMode::All | PaletteMode::Workflows
    )
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
pub(crate) fn exercise_companion_presentation_palette(
    parent: &gtk::Window,
    terminal: &gtk::Widget,
    current_target: Rc<Cell<Option<u64>>>,
    apply: Rc<dyn Fn(Option<u64>, bool)>,
    expanded: Rc<dyn Fn() -> bool>,
    settle: &impl Fn(std::time::Duration),
) {
    use std::time::Duration;
    let outputs = Rc::new(RefCell::new(Vec::new()));
    let palette = PaletteModel::builder()
        .launch(PaletteInit {
            parent: parent.clone(),
            keybindings: Rc::new(RefCell::new(KeybindingMap::from_defaults())),
            workflows: Rc::new(RefCell::new(Vec::new())),
        })
        .connect_receiver({
            let outputs = outputs.clone();
            move |_, output| {
                if let PaletteOutput::SetVteCompanionExpanded(target, desired) = output {
                    outputs.borrow_mut().push((target, desired));
                    apply(target, desired);
                } else {
                    panic!("unexpected palette output");
                }
            }
        });
    let wait_for = |stage: &str, deadline: std::time::Instant, ready: &dyn Fn() -> bool| {
        while !ready() && std::time::Instant::now() < deadline {
            settle(Duration::from_millis(10));
        }
        assert!(ready(), "palette native stage did not settle: {stage}");
    };
    let open = || {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        assert!(terminal.grab_focus());
        palette.emit(PaletteMsg::Toggle {
            workflow_target: Some(42),
            mode: PaletteMode::All,
            history_path: None,
            live_history: Vec::new(),
        });
        // AdwDialog opens and grabs focus on a later frame, after mapping.
        // Wait for the actual filter descendant before sending a real key.
        let filter_ready = || {
            palette.widget().is_mapped()
                && palette.widget().parent().is_some()
                && gtk::prelude::RootExt::focus(parent).is_some_and(|focus| {
                    let widgets = palette.widgets();
                    focus == *widgets.filter_entry.upcast_ref::<gtk::Widget>()
                        || focus.is_ancestor(&widgets.filter_entry)
                })
        };
        wait_for("filter ready", deadline, &filter_ready);
        // Toggle explicitly focuses the filter before AdwDialog's second map
        // tick opens the sheet. Two actual after-paint frames occur after those
        // update-phase ticks, unlike mapped/focus alone or a fixed sleep.
        struct PaintObserver {
            clock: gtk::gdk::FrameClock,
            handler: Option<gtk::glib::SignalHandlerId>,
        }
        impl Drop for PaintObserver {
            fn drop(&mut self) {
                if let Some(handler) = self.handler.take() {
                    self.clock.disconnect(handler);
                }
            }
        }
        let clock = palette.widget().frame_clock().expect("mapped dialog clock");
        let paints = Rc::new(Cell::new(0u32));
        let handler = clock.connect_after_paint({
            let paints = paints.clone();
            move |_| paints.set(paints.get().saturating_add(1))
        });
        let observer = PaintObserver {
            clock,
            handler: Some(handler),
        };
        while paints.get() < 2 && std::time::Instant::now() < deadline {
            palette.widget().queue_draw();
            settle(Duration::from_millis(10));
        }
        drop(observer);
        assert!(
            paints.get() >= 2,
            "dialog did not complete two native paints"
        );
        assert!(filter_ready(), "dialog lost filter focus before the key");
    };
    let key = |keyval: u32| {
        let controllers = palette.widget().observe_controllers();
        let controller = (0..controllers.n_items())
            .find_map(|index| {
                controllers
                    .item(index)
                    .and_then(|c| c.downcast::<gtk::EventControllerKey>().ok())
                    .filter(|c| c.name().as_deref() == Some("anvil-palette-keyboard"))
            })
            .expect("production palette key controller");
        assert!(controller.emit_by_name::<bool>(
            "key-pressed",
            &[&keyval, &0u32, &gtk::gdk::ModifierType::empty()]
        ));
        wait_for(
            "closed and terminal focus restored",
            std::time::Instant::now() + Duration::from_secs(3),
            &|| {
                !palette.widget().is_mapped()
                    && palette.widget().parent().is_none()
                    && gtk::prelude::RootExt::focus(parent)
                        .is_some_and(|focus| focus == *terminal || focus.is_ancestor(terminal))
            },
        );
    };
    open();
    palette
        .widgets()
        .filter_entry
        .set_text("Expand VTE neutral companion");
    key(0xff0d); // Actual production Return handler, including delayed search.
    assert_eq!(&*outputs.borrow(), &[(Some(42), true)]);
    assert!(
        expanded(),
        "force_close must return native focus before the accepted action"
    );
    assert!(gtk::prelude::RootExt::focus(parent)
        .is_some_and(|focus| focus == *terminal || focus.is_ancestor(terminal)));
    open();
    palette
        .widgets()
        .filter_entry
        .set_text("Collapse VTE neutral companion");
    key(0xff1b); // Escape cancels without changing presentation.
    assert!(expanded());
    assert_eq!(outputs.borrow().len(), 1);
    open();
    palette
        .widgets()
        .filter_entry
        .set_text("Collapse VTE neutral companion");
    current_target.set(Some(99));
    key(0xff0d);
    assert_eq!(outputs.borrow().last(), Some(&(Some(42), false)));
    assert!(expanded(), "old opening target cannot change the new pane");
    current_target.set(Some(42));
    open();
    palette
        .widgets()
        .filter_entry
        .set_text("Collapse VTE neutral companion");
    key(0xff0d);
    assert!(!expanded());
    drop(palette);
}

#[cfg(test)]
mod tests {
    use super::{HistoryLoadRequest, HistoryLoadState, PaletteOpeningGeneration};

    #[test]
    fn palette_factory_messages_remain_send_safe() {
        fn assert_send<T: Send>() {}
        assert_send::<super::PaletteRowOutput>();
        assert_send::<super::PaletteMsg>();
        assert_send::<super::PaletteOutput>();
    }

    #[test]
    fn queued_palette_actions_keep_the_signal_time_opening() {
        let mut opening = PaletteOpeningGeneration::default();
        let signal_generation = opening.active.clone();
        let first = opening.open();
        let queued = signal_generation.get().unwrap();
        assert_eq!(queued, first);
        opening.close();
        assert_eq!(signal_generation.get(), None);
        let second = opening.open();
        assert!(!opening.accepts(queued));
        assert!(opening.accepts(second));
        assert_eq!(signal_generation.get(), Some(second));
    }

    #[test]
    fn palette_rows_and_native_actions_gate_before_selection_or_close() {
        let source = include_str!("command_palette.rs")
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .unwrap();
        assert!(source.contains("generation = self.generation"));
        assert!(source.contains("PaletteRowOutput::Activate(generation, accept.clone())"));
        assert!(source.contains("PaletteMsg::Activate(generation, accept)"));
        assert!(source.contains("rows.push_back(PaletteRowInit { entry, generation })"));
        assert_eq!(
            source
                .matches("generation = model.opening.active.clone()")
                .count(),
            3
        );
        for message in [
            "Search(generation,",
            "AcceptSelected(generation)",
            "Move(generation,",
            "Close(generation)",
            "Closed(generation)",
        ] {
            assert!(source.contains(message), "{message}");
        }
        let update = source.split("    fn update_with_view(").nth(1).unwrap();
        assert!(
            update.find("!self.opening.accepts(generation)").unwrap()
                < update.find("match msg {").unwrap()
        );
    }

    #[test]
    fn every_palette_close_path_clears_history_and_target_synchronously() {
        let source = include_str!("command_palette.rs")
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .unwrap();
        assert_eq!(source.matches("self.close_opening(widgets)").count(), 4);
        let cleanup = source
            .split("    fn close_opening(")
            .nth(1)
            .unwrap()
            .split("    fn accept(")
            .next()
            .unwrap();
        for clear in [
            "self.opening.close()",
            "self.history_load.close()",
            "self.history_snapshot.clear()",
            "self.workflow_target = None",
            "widgets.history_status.set_visible(false)",
        ] {
            assert!(cleanup.contains(clear), "{clear}");
        }
        let accept = source.split("    fn accept(").nth(1).unwrap();
        assert!(
            accept
                .find("let workflow_target = self.workflow_target")
                .unwrap()
                < accept.find("self.close_opening(widgets)").unwrap()
        );
        assert!(accept.contains("PaletteOutput::RunWorkflow(path, workflow_target)"));
    }

    #[test]
    fn workflow_reload_follows_the_effective_query_mode() {
        use super::{query_uses_workflows, PaletteMode};
        assert!(query_uses_workflows(": deploy", PaletteMode::History));
        assert!(query_uses_workflows("  : build", PaletteMode::Commands));
        assert!(query_uses_workflows("deploy", PaletteMode::Workflows));
        assert!(query_uses_workflows("", PaletteMode::All));
        assert!(!query_uses_workflows("@ history", PaletteMode::All));
        assert!(!query_uses_workflows("> commands", PaletteMode::Workflows));
        assert!(!query_uses_workflows("? request", PaletteMode::All));
    }

    #[test]
    fn enter_refreshes_model_and_rows_before_resolving_selection() {
        // This checks the Relm handler wiring without constructing GTK widgets.
        // It does not claim native event-loop or factory-widget integration.
        let source = include_str!("command_palette.rs");
        let enter = source
            .split_once("PaletteMsg::AcceptSelected(_) => {")
            .unwrap()
            .1
            .split_once("PaletteMsg::Close(_) => {")
            .unwrap()
            .0;
        let input = enter.find("widgets.filter_entry.text()").unwrap();
        let model = enter.find("self.query = query;").unwrap();
        let rows = enter.find("self.rebuild_rows();").unwrap();
        let selection = enter.find("widgets.list_box.selected_row()").unwrap();
        let activation = enter.find("self.accept(accept,").unwrap();
        assert!(input < model && model < rows && rows < selection && selection < activation);
    }

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
