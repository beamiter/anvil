//! On-demand inspection of one real native companion. The dialog owns values,
//! and the window voice, not a runtime or pane. It cannot dispatch a command,
//! grant attention, age the organism, write memory, or create a frame source.

use adw::prelude::*;
use jterm_core::organism::{LifeState, RenderContext, RepoVigil};
use jterm_core::organism_companion::{CompanionCare, CompanionIntent, CompanionVoice};
use jterm_core::organism_daily::{behavior_label, context_explanation, life_description};
use jterm_core::organism_memory::{GrowthProgress, GrowthStage};
use relm4::{adw, gtk};
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub context: RenderContext,
    pub life: LifeState,
    pub growth: GrowthProgress,
    pub activity: String,
    pub latest: String,
    pub memory: String,
    pub motion: &'static str,
    pub vigil: RepoVigil,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Dimension {
    name: &'static str,
    value: Option<f64>,
    explanation: &'static str,
}

fn dimension(name: &'static str, value: f32, explanation: &'static str) -> Dimension {
    Dimension {
        name,
        value: value.is_finite().then(|| f64::from(value.clamp(0.0, 1.0))),
        explanation,
    }
}

fn dimensions(life: LifeState) -> [Dimension; 8] {
    [
        dimension(
            "Energy",
            life.energy,
            "Available energy; quiet time lets it recover.",
        ),
        dimension(
            "Mood",
            life.mood,
            "Its simulated mood after recent work and rest.",
        ),
        dimension(
            "Curiosity",
            life.curiosity,
            "How inclined it is to look around.",
        ),
        dimension(
            "Restlessness",
            life.boredom,
            "How inclined it is to stretch or explore.",
        ),
        dimension(
            "Tension",
            life.stress,
            "Residual tension that settles during quieter work.",
        ),
        dimension(
            "Company",
            life.social_need,
            "Its inclination to come closer during idle time.",
        ),
        dimension(
            "Familiarity",
            life.attachment,
            "How familiar your shared work feels.",
        ),
        dimension(
            "Confidence",
            life.confidence,
            "Confidence shaped by observed outcomes.",
        ),
    ]
}

fn growth_description(growth: GrowthProgress) -> String {
    let stage = match growth.stage() {
        GrowthStage::Juvenile => "Juvenile",
        GrowthStage::Adult => "Adult",
        GrowthStage::Seasoned => "Seasoned",
    };
    format!(
        "{stage} · {} observed day{} · {} recover{}",
        growth.days_seen,
        if growth.days_seen == 1 { "" } else { "s" },
        growth.lifetime_recoveries,
        if growth.lifetime_recoveries == 1 {
            "y"
        } else {
            "ies"
        },
    )
}

fn work_description(vigil: RepoVigil) -> &'static str {
    match vigil {
        RepoVigil::None => "No unresolved build or recovery is currently being watched.",
        RepoVigil::Failure => "An observed build or test failure is still unresolved.",
        RepoVigil::Stuck => "Several observed build or test failures remain unresolved.",
        RepoVigil::Recovery => "The build recovered; keeping watch until a successful push.",
        RepoVigil::CautiousRecovery => {
            "The build recovered after repeated changes between success and failure."
        }
    }
}

fn text_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_hexpand(true);
    label
}

fn detail(body: &gtk::Box, heading: &str, text: &str) {
    let label = text_label(heading);
    label.add_css_class("heading");
    body.append(&label);
    let content = text_label(text);
    content.set_selectable(true);
    body.append(&content);
}

pub(crate) fn present(
    window: &adw::ApplicationWindow,
    snapshot: Snapshot,
    voice: Rc<RefCell<CompanionVoice>>,
    care: Rc<RefCell<CompanionCare>>,
    epoch: std::time::Instant,
    care_changed: Option<super::CareSettingsHandler>,
) -> adw::Dialog {
    let dialog = adw::Dialog::builder()
        .title("ASCII companion")
        .content_width(480)
        .content_height(650)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_margin_start(20);
    body.set_margin_end(20);
    body.set_margin_top(12);
    body.set_margin_bottom(20);

    let portrait = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    // Details describe the same companion. No portrait is rendered here: a
    // second ASCII sprite would make inspection look like another organism.
    let caption = gtk::Box::new(gtk::Orientation::Vertical, 4);
    caption.set_hexpand(true);
    let title = text_label(behavior_label(snapshot.context.behavior));
    title.add_css_class("title-3");
    caption.append(&title);
    caption.append(&text_label(context_explanation(snapshot.context)));
    portrait.append(&caption);
    body.append(&portrait);
    let disposition = text_label(life_description(snapshot.life));
    disposition.add_css_class("dim-label");
    body.append(&disposition);
    body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let temperament = voice.borrow().temperament();
    detail(&body, temperament.label(), temperament.description());
    let reply = text_label(
        voice
            .borrow()
            .last_reply()
            .unwrap_or("I'm here. We can share a moment, a story, or a little break."),
    );
    reply.set_widget_name("companion-reply");
    reply.set_accessible_role(gtk::AccessibleRole::Status);
    reply.set_selectable(true);
    body.append(&reply);
    let context_hint = text_label("Replies use the activity observed when this panel opened.");
    context_hint.add_css_class("dim-label");
    body.append(&context_hint);
    let actions = gtk::FlowBox::new();
    actions.set_selection_mode(gtk::SelectionMode::None);
    actions.set_row_spacing(6);
    actions.set_column_spacing(6);
    actions.set_max_children_per_line(2);
    for (label, intent) in [
        ("Say hello", CompanionIntent::Hello),
        ("Keep me company", CompanionIntent::Company),
        ("Tell a story", CompanionIntent::Story),
        ("Take a break", CompanionIntent::Rest),
    ] {
        let button = gtk::Button::with_label(label);
        let voice = voice.clone();
        let care = care.clone();
        let reply = reply.downgrade();
        let context = snapshot.context;
        button.connect_clicked(move |_| {
            if let Some(reply) = reply.upgrade() {
                if intent == CompanionIntent::Rest {
                    care.borrow_mut().rested(epoch.elapsed());
                }
                reply.set_text(voice.borrow_mut().respond(intent, context));
            }
        });
        actions.insert(&button, -1);
    }
    body.append(&actions);
    let reminders = gtk::CheckButton::with_label("Gentle break reminders");
    reminders.set_active(care.borrow().enabled());
    reminders.set_tooltip_text(Some("Occasional water, stretching or screen-break prompts after sustained foreground work. Silent during typing and running commands."));
    reminders.connect_toggled({
        let care = care.clone();
        move |button| {
            let enabled = button.is_active();
            care.borrow_mut().set_enabled(enabled);
            if let Some(changed) = &care_changed {
                changed(enabled);
            }
        }
    });
    body.append(&reminders);
    let later = gtk::Button::with_label("Remind me in 20 minutes");
    later.set_halign(gtk::Align::Start);
    later.connect_clicked(move |_| care.borrow_mut().snooze(epoch.elapsed()));
    body.append(&later);

    detail(&body, "This pane", &snapshot.activity);
    detail(&body, "Recent activity", &snapshot.latest);
    detail(
        &body,
        "Work being watched",
        work_description(snapshot.vigil),
    );
    detail(
        &body,
        "Growing together",
        &growth_description(snapshot.growth),
    );

    let levels_heading = text_label("Companion state");
    levels_heading.add_css_class("heading");
    levels_heading.set_margin_top(8);
    body.append(&levels_heading);
    let grid = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(8)
        .build();
    for (row, dimension) in dimensions(snapshot.life).into_iter().enumerate() {
        let label = text_label(dimension.name);
        label.set_tooltip_text(Some(dimension.explanation));
        grid.attach(&label, 0, row as i32, 1, 1);
        let level = gtk::ProgressBar::new();
        level.set_hexpand(true);
        level.set_valign(gtk::Align::Center);
        level.set_fraction(dimension.value.unwrap_or(0.0));
        level.set_tooltip_text(Some(dimension.explanation));
        level.update_property(&[
            gtk::accessible::Property::Label(dimension.name),
            gtk::accessible::Property::Description(dimension.explanation),
        ]);
        grid.attach(&level, 1, row as i32, 1, 1);
        let value = dimension.value.map_or_else(
            || "Unknown".to_owned(),
            |value| format!("{:.0}", value * 100.0),
        );
        let value = gtk::Label::new(Some(&value));
        value.set_width_chars(3);
        value.set_xalign(1.0);
        grid.attach(&value, 2, row as i32, 1, 1);
    }
    body.append(&grid);
    let levels_hint = text_label("0–100 describes the companion's simulated state.");
    levels_hint.add_css_class("dim-label");
    body.append(&levels_hint);
    detail(&body, "Memory", &snapshot.memory);
    detail(&body, "Motion", snapshot.motion);
    let footnote = text_label("Captured when opened. Shared state belongs to this window; recent activity belongs to this pane. Reopen to refresh.");
    footnote.add_css_class("dim-label");
    footnote.set_margin_top(12);
    body.append(&footnote);
    scrolled.set_child(Some(&body));
    toolbar.set_content(Some(&scrolled));
    dialog.set_child(Some(&toolbar));
    // Start on the scrollable report, not on a selectable label that GTK
    // would automatically highlight. Arrow/Page keys can inspect the rest.
    scrolled.set_focusable(true);
    dialog.set_focus(Some(&scrolled));
    dialog.present(Some(window));
    dialog
}

pub(crate) fn present_capability(window: &adw::ApplicationWindow, message: &str) {
    let dialog = adw::AlertDialog::new(Some("ASCII companion"), Some(message));
    dialog.add_response("close", "Close");
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");
    dialog.present(Some(window));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires DISPLAY"]
    fn snapshot_dialog_is_scrollable_and_closes_without_an_animation_source() {
        adw::init().expect("adwaita display");
        // Test the snapshot lifecycle without an unrelated desktop sheet animation.
        gtk::Settings::default()
            .expect("desktop settings")
            .set_gtk_enable_animations(false);
        let window = adw::ApplicationWindow::builder()
            .default_width(360)
            .default_height(420)
            .build();
        window.set_content(Some(&gtk::Box::new(gtk::Orientation::Vertical, 0)));
        window.present();
        let snapshot = Snapshot {
            context: RenderContext::new(
                jterm_core::organism::Behavior::WatchAgent,
                jterm_core::organism::BodyLanguage::default(),
                false,
            )
            .with_watch_rhythm(jterm_core::organism::WatchRhythm::Waiting),
            life: LifeState::default(),
            growth: GrowthProgress::default(),
            activity: "Shell Agent is working · 42s elapsed".to_owned(),
            latest: "A recorded command is still running".to_owned(),
            memory: "Local memory".to_owned(),
            motion: "Calm",
            vigil: RepoVigil::Failure,
        };
        let voice = Rc::new(RefCell::new(CompanionVoice::new(7)));
        let care = Rc::new(RefCell::new(CompanionCare::default()));
        let preference = Rc::new(std::cell::Cell::new(None));
        let recorded = preference.clone();
        let dialog = present(
            &window,
            snapshot,
            voice.clone(),
            care.clone(),
            std::time::Instant::now(),
            Some(Rc::new(move |enabled| recorded.set(Some(enabled)))),
        );
        let main = gtk::glib::MainContext::default();
        let start = std::time::Instant::now();
        while (!dialog.is_mapped() || dialog.width() <= 0)
            && start.elapsed() < std::time::Duration::from_secs(3)
        {
            while main.iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(dialog.is_mapped());
        assert!(
            dialog.width() > 0,
            "wait for the presented sheet to finish its first allocation"
        );
        let toolbar = dialog
            .child()
            .unwrap()
            .downcast::<adw::ToolbarView>()
            .unwrap();
        let scrolled = toolbar
            .content()
            .unwrap()
            .downcast::<gtk::ScrolledWindow>()
            .unwrap();
        assert_eq!(scrolled.hscrollbar_policy(), gtk::PolicyType::Never);
        assert_eq!(scrolled.vscrollbar_policy(), gtk::PolicyType::Automatic);
        assert_eq!(
            dialog.focus().as_ref(),
            Some(scrolled.upcast_ref::<gtk::Widget>())
        );
        assert!(dialog.width() <= window.width());
        fn descendants(widget: &gtk::Widget, out: &mut Vec<gtk::Widget>) {
            out.push(widget.clone());
            let mut child = widget.first_child();
            while let Some(widget) = child {
                child = widget.next_sibling();
                descendants(&widget, out);
            }
        }
        let mut widgets = Vec::new();
        descendants(dialog.upcast_ref(), &mut widgets);
        let story = widgets
            .iter()
            .find_map(|widget| {
                let button = widget.clone().downcast::<gtk::Button>().ok()?;
                (button.label().as_deref() == Some("Tell a story")).then_some(button)
            })
            .expect("story action is reachable in the real inspector");
        story.emit_clicked();
        let first = voice.borrow().last_reply().unwrap();
        story.emit_clicked();
        assert_ne!(voice.borrow().last_reply(), Some(first));
        let reminders = widgets
            .iter()
            .find_map(|widget| {
                let button = widget.clone().downcast::<gtk::CheckButton>().ok()?;
                (button.label().as_deref() == Some("Gentle break reminders")).then_some(button)
            })
            .expect("care preference is reachable");
        assert!(!care.borrow().enabled());
        reminders.set_active(true);
        assert!(care.borrow().enabled());
        assert_eq!(preference.get(), Some(true));
        reminders.set_active(false);
        assert!(!care.borrow().enabled());
        assert_eq!(preference.get(), Some(false));
        let closed = std::rc::Rc::new(std::cell::Cell::new(false));
        dialog.connect_closed({
            let closed = closed.clone();
            move |_| {
                closed.set(true);
            }
        });
        // Adwaita completes its initial presentation after the first paint.
        // A close in the map/allocation stack can precede that setup. Observe
        // two real frames so this tests the shown sheet's normal close path.
        let clock = dialog.frame_clock().unwrap();
        let frame = clock.frame_counter();
        let start = std::time::Instant::now();
        while clock.frame_counter() < frame + 2
            && start.elapsed() < std::time::Duration::from_secs(3)
        {
            window.queue_draw();
            while main.iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(clock.frame_counter() >= frame + 2);
        dialog.force_close();
        let start = std::time::Instant::now();
        while dialog.is_mapped() && start.elapsed() < std::time::Duration::from_secs(3) {
            while main.iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(closed.get());
        assert!(!dialog.is_mapped());
        assert!(dialog.parent().is_none());
        window.close();
    }

    #[test]
    fn every_simulated_dimension_is_named_and_unknown_stays_unknown() {
        let state = LifeState {
            energy: f32::NAN,
            mood: -1.0,
            curiosity: 2.0,
            boredom: 0.25,
            stress: f32::INFINITY,
            social_need: f32::NEG_INFINITY,
            attachment: 0.75,
            confidence: 0.5,
        };
        let levels = dimensions(state);
        assert_eq!(
            levels.map(|d| d.name),
            [
                "Energy",
                "Mood",
                "Curiosity",
                "Restlessness",
                "Tension",
                "Company",
                "Familiarity",
                "Confidence"
            ]
        );
        assert_eq!(
            levels.map(|d| d.value),
            [
                None,
                Some(0.0),
                Some(1.0),
                Some(0.25),
                None,
                None,
                Some(0.75),
                Some(0.5)
            ]
        );
        assert!(levels.into_iter().all(|d| !d.explanation.is_empty()));
    }

    #[test]
    fn reading_growth_and_work_does_not_invent_completed_work() {
        let growth = GrowthProgress::default();
        assert_eq!(
            growth_description(growth),
            "Juvenile · 0 observed days · 0 recoveries"
        );
        assert_eq!(
            growth_description(GrowthProgress {
                days_seen: 1,
                lifetime_recoveries: 1
            }),
            "Juvenile · 1 observed day · 1 recovery"
        );
        assert!(work_description(RepoVigil::Failure).contains("unresolved"));
        assert!(work_description(RepoVigil::Stuck).contains("unresolved"));
        assert!(work_description(RepoVigil::Recovery).contains("until a successful push"));
        assert!(work_description(RepoVigil::CautiousRecovery).contains("repeated changes"));
        assert_eq!(growth, GrowthProgress::default());
    }
}
