#![allow(dead_code)]
use std::{cell::RefCell, collections::HashMap, marker::PhantomData, rc::Rc};
// @pane_layout
extern crate self as log;
#[macro_export]
macro_rules! error { ($($arg:tt)*) => { let _ = format!($($arg)*); }; }
#[derive(Debug, PartialEq)]
enum VteInput {
    SearchClear,
    GrabFocus,
}
#[derive(Debug, PartialEq)]
enum Event {
    Terminal(u64, VteInput),
    Replay,
    Removed(u64),
    Quit,
}
type Events = Rc<RefCell<Vec<Event>>>;
struct Terminal {
    id: u64,
    events: Events,
    draft: String,
}
impl Terminal {
    fn emit(&self, input: VteInput) {
        self.events
            .borrow_mut()
            .push(Event::Terminal(self.id, input));
    }
}
mod search {
    pub enum SearchMsg {
        ActivePaneChanged,
    }
    // @changed
}
struct Search {
    events: Events,
}
impl Search {
    fn emit(&self, _: search::SearchMsg) {
        self.events.borrow_mut().push(Event::Replay);
    }
}
mod gtk {
    #[derive(Clone)]
    pub struct Widget(pub u64);
    impl Widget {
        pub fn is_mapped(&self) -> bool {
            true
        }
        pub fn upcast<T>(self) -> T
        where
            T: From<Self>,
        {
            T::from(self)
        }
        pub fn compute_bounds(&self, _: &Self) -> Option<Bounds> {
            Some(Bounds(self.0 as f32 * 100.0))
        }
    }
    pub struct Bounds(f32);
    impl Bounds {
        pub fn x(&self) -> f32 {
            self.0
        }
        pub fn y(&self) -> f32 {
            0.0
        }
        pub fn width(&self) -> f32 {
            80.0
        }
        pub fn height(&self) -> f32 {
            80.0
        }
    }
}
// The native editor owns its asynchronous close confirmation. Model that
// external boundary while executing the unchanged workspace methods below.
mod vim9_ide {
    use super::gtk;
    use std::cell::RefCell;

    thread_local! {
        static OPEN: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
        static REQUESTED: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    }

    pub fn configure(open: &[u64]) {
        OPEN.with_borrow_mut(|widgets| *widgets = open.to_vec());
        REQUESTED.with_borrow_mut(Vec::clear);
    }

    pub fn request_embedded_close(widget: &gtk::Widget) -> bool {
        REQUESTED.with_borrow_mut(|widgets| widgets.push(widget.0));
        OPEN.with_borrow(|widgets| widgets.contains(&widget.0))
    }

    pub fn requested() -> Vec<u64> {
        REQUESTED.with_borrow(Clone::clone)
    }
}
struct Pane {
    id: u64,
    terminal: Terminal,
}
impl Pane {
    fn widget(&self) -> gtk::Widget {
        gtk::Widget(self.id)
    }
}
struct Remote {
    pane_id: u64,
}
struct Tab {
    bell: bool,
    activity: bool,
    holder: gtk::Widget,
    id: u64,
    active_pane: usize,
    panes: Vec<Pane>,
    zoom: Option<()>,
    remote: Option<Remote>,
}
struct Agent {
    bound_tab: u64,
    bound_pane: u64,
}
struct ComponentSender<T>(PhantomData<T>);
struct Stack {
    events: Events,
}
impl Stack {
    fn set_visible_child_name(&self, _: &str) {}
    fn remove(&self, holder: &gtk::Widget) {
        self.events.borrow_mut().push(Event::Removed(holder.0));
    }
}
struct AppModel {
    stack: Stack,
    tabs: Vec<Tab>,
    active: usize,
    search: Search,
    pending_split_spawns: HashMap<u64, (u64, u64)>,
    active_agent: RefCell<Option<Agent>>,
    detach_fails: bool,
}
// @index
impl AppModel {
    // @methods
    fn active_pane_id(&self) -> Option<u64> {
        self.tabs
            .get(self.active)?
            .panes
            .get(self.tabs[self.active].active_pane)
            .map(|p| p.id)
    }
    fn active_terminal(&self) -> Option<&Terminal> {
        let t = self.tabs.get(self.active)?;
        Some(&t.panes.get(t.active_pane)?.terminal)
    }
    fn begin_organism_focus_transfer(&self, _: Option<u64>, _: bool) -> bool {
        true
    }
    fn finish_organism_focus_transfer(&self, _: bool) {}
    fn refresh_active_pane_headers(&self) {}
    fn refresh_pane_headers(&self, _: usize) {}
    fn close_command_suggestion_for_pane(&self, _: u64) {}
    fn close_command_correction_for_pane(&self, _: u64) {}
    fn agent_close(&self) {}
    fn toggle_pane_zoom_for(&self, _: usize) {}
    fn index_of(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }
    fn sync_organism_focus(&self) {}
    fn note_task_terminal_closed(&self, _: u64) {}
    fn force_quit(&self) {
        self.search.events.borrow_mut().push(Event::Quit);
    }
    fn file_tree_goto_current_cwd(&self, _: &ComponentSender<Self>) {}
    fn rebuild_tab_strip(&self, _: &ComponentSender<Self>) {}
    fn refresh_bottom_bar(&self) {}
    fn detach_pane_from_tab(&mut self, ti: usize, pi: usize) -> Option<Pane> {
        if self.detach_fails {
            return None;
        }
        let tab = &mut self.tabs[ti];
        let removed = tab.panes.remove(pi);
        tab.active_pane = active_index_after_remove(tab.active_pane, pi, tab.panes.len());
        Some(removed)
    }
}
fn model() -> (AppModel, Events) {
    vim9_ide::configure(&[]);
    let events = Events::default();
    let panes = (1..=3)
        .map(|id| Pane {
            id,
            terminal: Terminal {
                id,
                events: events.clone(),
                draft: format!("draft {id}"),
            },
        })
        .collect();
    (
        AppModel {
            stack: Stack {
                events: events.clone(),
            },
            tabs: vec![Tab {
                bell: false,
                activity: false,
                holder: gtk::Widget(0),
                id: 10,
                active_pane: 0,
                panes,
                zoom: None,
                remote: None,
            }],
            active: 0,
            search: Search {
                events: events.clone(),
            },
            pending_split_spawns: HashMap::new(),
            active_agent: RefCell::new(None),
            detach_fails: false,
        },
        events,
    )
}
fn expected(old: u64, new: u64) -> Vec<Event> {
    vec![
        Event::Terminal(old, VteInput::SearchClear),
        Event::Replay,
        Event::Terminal(new, VteInput::GrabFocus),
    ]
}
#[test]
fn cycle_clears_original_before_replaying_on_selected_pane() {
    let (mut m, e) = model();
    m.cycle_pane_focus(1);
    assert_eq!(*e.borrow(), expected(1, 2));
    assert_eq!(m.active_pane_id(), Some(2));
    // The asynchronous PaneFocused notification observes the already committed
    // destination: it cannot repair a missing transition in this method.
    assert!(!search::active_pane_changed(m.active_pane_id(), Some(2)));
    assert_eq!(m.tabs[0].panes[0].terminal.draft, "draft 1");
    assert_eq!(m.tabs[0].panes[1].terminal.draft, "draft 2");
    e.borrow_mut().clear();
    m.cycle_pane_focus(-1);
    assert_eq!(*e.borrow(), expected(2, 1));
}
#[test]
fn identity_stable_cycle_does_not_clear_or_replay() {
    let (mut m, e) = model();
    m.cycle_pane_focus(3);
    assert_eq!(*e.borrow(), vec![Event::Terminal(1, VteInput::GrabFocus)]);
}
#[test]
fn closing_active_split_replays_after_removal() {
    let (mut m, e) = model();
    m.close_pane(1, &ComponentSender(PhantomData));
    assert_eq!(*e.borrow(), expected(1, 2));
    assert_eq!(m.active_pane_id(), Some(2));
    assert_eq!(m.tabs[0].panes[0].terminal.draft, "draft 2");
}
#[test]
fn closing_inactive_split_does_not_clear_search() {
    let (mut m, e) = model();
    m.close_pane(2, &ComponentSender(PhantomData));
    assert_eq!(*e.borrow(), vec![Event::Terminal(1, VteInput::GrabFocus)]);
}
#[test]
fn embedded_editor_close_preserves_pane_and_search_until_confirmation() {
    let (mut m, e) = model();
    m.pending_split_spawns.insert(1, (10, 1));
    vim9_ide::configure(&[1]);
    m.close_pane(1, &ComponentSender(PhantomData));
    assert_eq!(vim9_ide::requested(), vec![1]);
    assert_eq!(m.active_pane_id(), Some(1));
    assert_eq!(m.tabs[0].panes.len(), 3);
    assert_eq!(m.pending_split_spawns.get(&1), Some(&(10, 1)));
    assert_eq!(m.tabs[0].panes[0].terminal.draft, "draft 1");
    assert!(e.borrow().is_empty());
}
#[test]
fn failed_detachment_restores_original_search() {
    let (mut m, e) = model();
    m.detach_fails = true;
    m.close_pane(1, &ComponentSender(PhantomData));
    assert_eq!(
        *e.borrow(),
        vec![Event::Terminal(1, VteInput::SearchClear), Event::Replay]
    );
    assert_eq!(m.active_pane_id(), Some(1));
}
#[test]
fn failed_split_rollback_rebinds_surviving_source() {
    let (mut m, e) = model();
    m.tabs[0].active_pane = 1;
    assert!(m.rollback_failed_split(2, 10, 1));
    assert_eq!(*e.borrow(), expected(2, 1));
    assert_eq!(m.active_pane_id(), Some(1));
}
#[test]
fn inactive_failed_split_preserves_search_owner() {
    let (mut m, e) = model();
    assert!(m.rollback_failed_split(2, 10, 1));
    assert!(e.borrow().is_empty());
}

#[test]
fn directional_focus_replays_on_new_pane() {
    let (mut m, e) = model();
    m.focus_pane_directional(Direction::Right);
    assert_eq!(*e.borrow(), expected(1, 2));
    assert_eq!(m.active_pane_id(), Some(2));
    e.borrow_mut().clear();
    m.focus_pane_directional(Direction::Up);
    assert!(e.borrow().is_empty());
}

fn tabs_model() -> (AppModel, Events) {
    let (mut m, e) = model();
    m.tabs = (1..=3)
        .map(|n| Tab {
            id: n * 10,
            holder: gtk::Widget(n * 10),
            active_pane: 1,
            bell: false,
            activity: false,
            zoom: None,
            remote: None,
            panes: (0..2)
                .map(|i| {
                    let id = n * 10 + i;
                    Pane {
                        id,
                        terminal: Terminal {
                            id,
                            events: e.clone(),
                            draft: format!("draft {id}"),
                        },
                    }
                })
                .collect(),
        })
        .collect();
    (m, e)
}
#[test]
fn closing_selected_first_tab_clears_destroyed_owner_and_replays_once() {
    let (mut m, e) = tabs_model();
    m.close_tab(10, &ComponentSender(PhantomData));
    assert_eq!(m.active_pane_id(), Some(21));
    assert_eq!(
        *e.borrow(),
        vec![
            Event::Terminal(11, VteInput::SearchClear),
            Event::Removed(10),
            Event::Terminal(21, VteInput::GrabFocus),
            Event::Replay
        ]
    );
}
#[test]
fn embedded_editor_close_preserves_tab_and_search_until_confirmation() {
    let (mut m, e) = tabs_model();
    vim9_ide::configure(&[10]);
    m.close_tab(10, &ComponentSender(PhantomData));
    assert_eq!(vim9_ide::requested(), vec![10]);
    assert_eq!(m.active_pane_id(), Some(11));
    assert_eq!(m.tabs.len(), 3);
    assert!(e.borrow().is_empty());
}
#[test]
fn closing_selected_last_tab_uses_previous_pane_identity() {
    let (mut m, e) = tabs_model();
    m.active = 2;
    m.close_tab(30, &ComponentSender(PhantomData));
    assert_eq!(m.active_pane_id(), Some(21));
    assert_eq!(
        *e.borrow(),
        vec![
            Event::Terminal(31, VteInput::SearchClear),
            Event::Removed(30),
            Event::Terminal(21, VteInput::GrabFocus),
            Event::Replay
        ]
    );
}
#[test]
fn background_tab_close_preserves_focus_and_search() {
    let (mut m, e) = tabs_model();
    m.active = 1;
    m.close_tab(10, &ComponentSender(PhantomData));
    assert_eq!(m.active_pane_id(), Some(21));
    assert_eq!(*e.borrow(), vec![Event::Removed(10)]);
}
#[test]
fn batch_selected_close_replays_only_once_after_all_removals() {
    let (mut m, e) = tabs_model();
    m.close_tabs(vec![10, 20], &ComponentSender(PhantomData));
    assert_eq!(m.active_pane_id(), Some(31));
    assert_eq!(
        *e.borrow(),
        vec![
            Event::Terminal(11, VteInput::SearchClear),
            Event::Removed(10),
            Event::Removed(20),
            Event::Terminal(31, VteInput::GrabFocus),
            Event::Replay
        ]
    );
}
#[test]
fn batch_close_requests_each_selected_editor_before_any_workspace_mutation() {
    let (mut m, e) = tabs_model();
    vim9_ide::configure(&[10, 20]);
    m.close_tabs(vec![10, 20, 99], &ComponentSender(PhantomData));
    assert_eq!(vim9_ide::requested(), vec![10, 20]);
    assert_eq!(m.active_pane_id(), Some(11));
    assert_eq!(m.tabs.len(), 3);
    assert!(e.borrow().is_empty());
}
#[test]
fn batch_background_close_does_not_replay_or_grab_focus() {
    let (mut m, e) = tabs_model();
    m.active = 1;
    m.close_tabs(vec![10, 30], &ComponentSender(PhantomData));
    assert_eq!(m.active_pane_id(), Some(21));
    assert_eq!(*e.borrow(), vec![Event::Removed(10), Event::Removed(30)]);
}
#[test]
fn closing_last_tab_does_not_replay_to_missing_destination() {
    let (mut m, e) = tabs_model();
    m.tabs.truncate(1);
    m.close_tab(10, &ComponentSender(PhantomData));
    assert_eq!(
        *e.borrow(),
        vec![
            Event::Terminal(11, VteInput::SearchClear),
            Event::Removed(10),
            Event::Quit
        ]
    );
}
#[test]
fn normal_tab_selection_clears_exact_original_controller() {
    let (mut m, e) = tabs_model();
    m.select_tab(30, &ComponentSender(PhantomData));
    assert_eq!(
        *e.borrow(),
        vec![
            Event::Terminal(11, VteInput::SearchClear),
            Event::Terminal(31, VteInput::GrabFocus),
            Event::Replay
        ]
    );
    e.borrow_mut().clear();
    m.select_tab(30, &ComponentSender(PhantomData));
    assert_eq!(*e.borrow(), vec![Event::Terminal(31, VteInput::GrabFocus)]);
}
