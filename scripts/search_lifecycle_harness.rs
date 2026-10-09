//! Actual search component message handling with deterministic widget/sender
//! boundaries. This is not a native GTK rendering or regex-engine fixture.
#![allow(dead_code, unused_imports)]
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
mod gtk {
    pub mod accessible {
        pub enum Property<'a> {
            Description(&'a str),
        }
    }
}
#[derive(Default)]
struct Entry(RefCell<String>);
impl Entry {
    fn text(&self) -> String {
        self.0.borrow().clone()
    }
    fn grab_focus(&self) {}
    fn set_text(&self, s: &str) {
        *self.0.borrow_mut() = s.to_owned()
    }
}
#[derive(Default)]
struct Label {
    text: RefCell<String>,
    visible: Cell<bool>,
}
impl Label {
    fn set_visible(&self, v: bool) {
        self.visible.set(v)
    }
    fn set_label(&self, s: &str) {
        *self.text.borrow_mut() = s.to_owned()
    }
    fn set_tooltip_text(&self, _: Option<&str>) {}
    fn update_property(&self, _: &[gtk::accessible::Property<'_>]) {}
    fn add_css_class(&self, _: &str) {}
    fn remove_css_class(&self, _: &str) {}
}
#[derive(Default)]
struct Button(Cell<bool>);
impl Button {
    fn set_sensitive(&self, v: bool) {
        self.0.set(v)
    }
}
#[derive(Default)]
struct Root(Cell<bool>);
impl Root {
    fn is_search_mode(&self) -> bool {
        self.0.get()
    }
    fn set_search_mode(&self, v: bool) {
        self.0.set(v)
    }
}
#[derive(Default)]
struct SearchModelWidgets {
    entry: Entry,
    regex_mode: Label,
    status_label: Label,
    previous_button: Button,
    next_button: Button,
}
struct ComponentSender<T> {
    outputs: Rc<RefCell<Vec<SearchOutput>>>,
    marker: std::marker::PhantomData<T>,
}
impl<T> Clone for ComponentSender<T> {
    fn clone(&self) -> Self {
        Self {
            outputs: self.outputs.clone(),
            marker: std::marker::PhantomData,
        }
    }
}
impl<T> ComponentSender<T> {
    fn output(&self, o: SearchOutput) -> Result<(), ()> {
        self.outputs.borrow_mut().push(o);
        Ok(())
    }
}
trait Component: Sized {
    type Widgets;
    type Root;
    type Input;
    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    );
}
// @search-lifecycle:types
impl Component for SearchModel {
    type Widgets = SearchModelWidgets;
    type Root = Root;
    type Input = SearchMsg;
    // @search-lifecycle:update
}
// @search-lifecycle:helpers
fn setup() -> (
    SearchModel,
    SearchModelWidgets,
    ComponentSender<SearchModel>,
    Root,
) {
    (
        SearchModel {
            case_sensitive: false,
        },
        SearchModelWidgets::default(),
        ComponentSender {
            outputs: Rc::new(RefCell::new(Vec::new())),
            marker: std::marker::PhantomData,
        },
        Root(Cell::new(true)),
    )
}
fn apply(
    model: &mut SearchModel,
    widgets: &mut SearchModelWidgets,
    sender: &ComponentSender<SearchModel>,
    root: &Root,
    msg: SearchMsg,
) {
    model.update_with_view(widgets, msg, sender.clone(), root)
}
#[test]
fn delayed_change_after_close_does_not_restart_hidden_search() {
    let (mut m, mut w, s, r) = setup();
    w.entry.set_text("needle");
    apply(&mut m, &mut w, &s, &r, SearchMsg::Close);
    assert!(matches!(
        s.outputs.borrow().last(),
        Some(SearchOutput::Closed)
    ));
    s.outputs.borrow_mut().clear();
    apply(&mut m, &mut w, &s, &r, SearchMsg::Changed("needle".into()));
    assert!(
        s.outputs.borrow().is_empty(),
        "a delayed GTK search-changed restarted a closed search"
    );
    assert!(w.status_label.text.borrow().is_empty());
}
#[test]
fn delayed_change_after_toggle_close_does_not_restart_hidden_search() {
    let (mut m, mut w, s, r) = setup();
    w.entry.set_text("needle");
    apply(&mut m, &mut w, &s, &r, SearchMsg::Toggle);
    s.outputs.borrow_mut().clear();
    apply(&mut m, &mut w, &s, &r, SearchMsg::Changed("needle".into()));
    assert!(s.outputs.borrow().is_empty());
}
#[test]
fn open_query_and_empty_query_dispatch_normally() {
    let (mut m, mut w, s, r) = setup();
    for text in ["needle", ""] {
        apply(&mut m, &mut w, &s, &r, SearchMsg::Changed(text.into()));
        assert!(
            matches!(s.outputs.borrow().last(),Some(SearchOutput::Changed{query,..}) if query==text)
        );
    }
    assert!(w.status_label.text.borrow().is_empty());
}
#[test]
fn reopening_replays_retained_query() {
    let (mut m, mut w, s, r) = setup();
    w.entry.set_text("needle");
    apply(&mut m, &mut w, &s, &r, SearchMsg::Close);
    s.outputs.borrow_mut().clear();
    apply(&mut m, &mut w, &s, &r, SearchMsg::Toggle);
    assert!(
        matches!(s.outputs.borrow().as_slice(),[SearchOutput::Changed{query,case_sensitive:false}] if query=="needle")
    );
    assert_eq!(*w.status_label.text.borrow(), "Searching…");
}
#[test]
fn closed_pane_transition_stays_idle() {
    let (mut m, mut w, s, r) = setup();
    w.entry.set_text("needle");
    apply(&mut m, &mut w, &s, &r, SearchMsg::Close);
    s.outputs.borrow_mut().clear();
    apply(&mut m, &mut w, &s, &r, SearchMsg::ActivePaneChanged);
    assert!(s.outputs.borrow().is_empty());
    assert!(w.status_label.text.borrow().is_empty());
}
#[test]
fn open_pane_transition_replays_current_query() {
    let (mut m, mut w, s, r) = setup();
    w.entry.set_text("needle");
    apply(&mut m, &mut w, &s, &r, SearchMsg::ActivePaneChanged);
    assert!(
        matches!(s.outputs.borrow().as_slice(),[SearchOutput::Changed{query,..}] if query=="needle")
    );
}
