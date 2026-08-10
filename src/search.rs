//! Relm4 component for find-in-terminal controls.

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;

/// Backend-neutral state for the window search bar. `Searching` is a short
/// UI-only transition while a pane computes its answer; terminal backends
/// return `Idle`, `Results`, or `Error` through `VteOutput::SearchStatus`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum SearchStatus {
    #[default]
    Idle,
    Searching,
    Results {
        current: usize,
        total: usize,
        /// The backend scanned a bounded suffix or used a different regex
        /// engine to estimate the native result set. `total` is marked as
        /// inexact in the presentation.
        truncated: bool,
    },
    Error(String),
}

impl SearchStatus {
    pub(crate) fn results(current: usize, total: usize) -> Self {
        Self::Results {
            current: current.min(total),
            total,
            truncated: false,
        }
    }

    pub(crate) fn partial_results(current: usize, reported_total: usize) -> Self {
        Self::Results {
            current,
            total: reported_total.max(usize::from(current > 0)),
            truncated: true,
        }
    }

    /// Advance a conventional VTE search cursor after the native backend says
    /// it found a match. VTE exposes success/failure but not its match index,
    /// so the adapter keeps this tiny wrap-around counter beside the regex.
    pub(crate) fn stepped(&self, delta: isize, found: bool) -> Self {
        let Self::Results {
            current,
            total,
            truncated,
        } = self
        else {
            return self.clone();
        };
        if !found || *total == 0 {
            return self.clone();
        }
        let current = if *truncated {
            if *current == 0 {
                0
            } else if delta < 0 {
                current.saturating_sub(1)
            } else if *current < *total {
                *current + 1
            } else {
                // Once navigation passes the reported range, VTE may or may
                // not have wrapped inside uncounted or cross-engine matches.
                0
            }
        } else if *current == 0 {
            if delta < 0 {
                *total
            } else {
                1
            }
        } else {
            ((*current as isize - 1 + delta).rem_euclid(*total as isize) + 1) as usize
        };
        Self::Results {
            current,
            total: *total,
            truncated: *truncated,
        }
    }
}

/// Keep compiler diagnostics useful in a one-line status area. Rust's regex
/// error includes the complete pattern and a caret on separate lines; the last
/// line is the concise reason users need.
pub(crate) fn invalid_regex_message(error: impl std::fmt::Display) -> String {
    let rendered = error.to_string();
    let last_line = rendered
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("invalid pattern");
    let detail = last_line.strip_prefix("error: ").unwrap_or(last_line);
    format!("Invalid regex: {detail}")
}

#[derive(Debug)]
pub(crate) enum SearchMsg {
    Toggle,
    Changed(String),
    /// The visible terminal changed while the find bar may still be open.
    /// Rebind the current query to the new pane instead of displaying the
    /// previous pane's result count.
    ActivePaneChanged,
    Status(SearchStatus),
    Next,
    Previous,
    Close,
}

#[derive(Debug)]
pub(crate) enum SearchOutput {
    Changed(String),
    Next,
    Previous,
    Closed,
}

pub(crate) struct SearchModel;

#[relm4::component(pub(crate))]
impl Component for SearchModel {
    type Init = ();
    type Input = SearchMsg;
    type Output = SearchOutput;
    type CommandOutput = ();

    view! {
        root = gtk::SearchBar {
            set_search_mode: false,

            #[wrap(Some)]
            set_child = &gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 6,
                set_margin_start: 8,
                set_margin_end: 8,
                set_margin_top: 6,
                set_margin_bottom: 6,

                #[name(entry)]
                gtk::SearchEntry {
                    set_placeholder_text: Some("Find… (/regex/ for regex)"),
                    set_hexpand: true,
                    update_property: &[
                        gtk::accessible::Property::Label("Find in terminal output"),
                    ],
                    connect_search_changed[sender] => move |entry| {
                        sender.input(SearchMsg::Changed(entry.text().to_string()));
                    },
                    connect_activate => SearchMsg::Next,

                    add_controller = gtk::EventControllerKey {
                        set_propagation_phase: gtk::PropagationPhase::Capture,
                        connect_key_pressed[sender] => move |_, key, _, state| {
                            use gtk::gdk::{Key, ModifierType};
                            let message = if key == Key::Escape {
                                Some(SearchMsg::Close)
                            } else if matches!(key, Key::Return | Key::KP_Enter)
                                && state.contains(ModifierType::SHIFT_MASK)
                            {
                                Some(SearchMsg::Previous)
                            } else {
                                None
                            };
                            if let Some(message) = message {
                                sender.input(message);
                                gtk::glib::Propagation::Stop
                            } else {
                                gtk::glib::Propagation::Proceed
                            }
                        },
                    },
                },

                #[name(status_label)]
                gtk::Label {
                    set_width_chars: 14,
                    set_max_width_chars: 36,
                    set_ellipsize: gtk::pango::EllipsizeMode::End,
                    set_xalign: 1.0,
                    add_css_class: "dim-label",
                },

                #[name(previous_button)]
                gtk::Button {
                    set_icon_name: "go-up-symbolic",
                    set_tooltip_text: Some("Previous match (Shift+Enter)"),
                    set_sensitive: false,
                    add_css_class: "flat",
                    update_property: &[
                        gtk::accessible::Property::Label("Previous match"),
                        gtk::accessible::Property::KeyShortcuts("Shift+Enter"),
                    ],
                    connect_clicked => SearchMsg::Previous,
                },

                #[name(next_button)]
                gtk::Button {
                    set_icon_name: "go-down-symbolic",
                    set_tooltip_text: Some("Next match (Enter)"),
                    set_sensitive: false,
                    add_css_class: "flat",
                    update_property: &[
                        gtk::accessible::Property::Label("Next match"),
                        gtk::accessible::Property::KeyShortcuts("Enter"),
                    ],
                    connect_clicked => SearchMsg::Next,
                },

                gtk::Button {
                    set_icon_name: "window-close-symbolic",
                    set_tooltip_text: Some("Close search (Escape)"),
                    add_css_class: "flat",
                    update_property: &[
                        gtk::accessible::Property::Label("Close search"),
                        gtk::accessible::Property::KeyShortcuts("Escape"),
                    ],
                    connect_clicked => SearchMsg::Close,
                },
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = Self;
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
            SearchMsg::Toggle => {
                let open = !root.is_search_mode();
                root.set_search_mode(open);
                if open {
                    widgets.entry.grab_focus();
                    let query = widgets.entry.text().to_string();
                    if !query.is_empty() {
                        apply_status(widgets, &SearchStatus::Searching);
                        let _ = sender.output(SearchOutput::Changed(query));
                    }
                } else {
                    apply_status(widgets, &SearchStatus::Idle);
                    let _ = sender.output(SearchOutput::Closed);
                }
            }
            SearchMsg::Changed(query) => {
                apply_status(
                    widgets,
                    if query.is_empty() {
                        &SearchStatus::Idle
                    } else {
                        &SearchStatus::Searching
                    },
                );
                let _ = sender.output(SearchOutput::Changed(query));
            }
            SearchMsg::ActivePaneChanged => {
                let query = widgets.entry.text().to_string();
                let (status, replay) = pane_transition(root.is_search_mode(), &query);
                apply_status(widgets, &status);
                if let Some(query) = replay {
                    let _ = sender.output(SearchOutput::Changed(query));
                }
            }
            SearchMsg::Status(status) => apply_status(widgets, &status),
            SearchMsg::Next => {
                let _ = sender.output(SearchOutput::Next);
            }
            SearchMsg::Previous => {
                let _ = sender.output(SearchOutput::Previous);
            }
            SearchMsg::Close => {
                apply_status(widgets, &SearchStatus::Idle);
                root.set_search_mode(false);
                let _ = sender.output(SearchOutput::Closed);
            }
        }
    }
}

/// Decide how the search component follows a newly active pane. Keeping this
/// transition independent of GTK makes the lifecycle contract testable: an
/// open bar always replays its query (including an empty query, which clears a
/// pane), while a closed bar only forgets stale presentation state.
fn pane_transition(search_open: bool, query: &str) -> (SearchStatus, Option<String>) {
    if !search_open {
        return (SearchStatus::Idle, None);
    }
    let status = if query.is_empty() {
        SearchStatus::Idle
    } else {
        SearchStatus::Searching
    };
    (status, Some(query.to_string()))
}

/// Stable pane identities are the ownership boundary for backend search state.
pub(crate) fn active_pane_changed(previous: Option<u64>, next: Option<u64>) -> bool {
    previous != next
}

#[derive(Debug, PartialEq, Eq)]
struct SearchPresentation {
    text: String,
    error: bool,
    navigable: bool,
}

fn presentation(status: &SearchStatus) -> SearchPresentation {
    match status {
        SearchStatus::Idle => SearchPresentation {
            text: String::new(),
            error: false,
            navigable: false,
        },
        SearchStatus::Searching => SearchPresentation {
            text: "Searching…".to_string(),
            error: false,
            navigable: false,
        },
        SearchStatus::Results {
            total: 0,
            truncated: false,
            ..
        } => SearchPresentation {
            text: "No results".to_string(),
            error: false,
            navigable: false,
        },
        SearchStatus::Results {
            current,
            total,
            truncated: true,
        } => SearchPresentation {
            text: if *current == 0 {
                format!("? of {total}+")
            } else {
                format!("{current} of {total}+")
            },
            error: false,
            navigable: *total > 0,
        },
        SearchStatus::Results {
            current,
            total,
            truncated: false,
        } => SearchPresentation {
            text: format!("{current} of {total}"),
            error: false,
            navigable: true,
        },
        SearchStatus::Error(error) => SearchPresentation {
            text: error.clone(),
            error: true,
            navigable: false,
        },
    }
}

fn apply_status(widgets: &SearchModelWidgets, status: &SearchStatus) {
    let presentation = presentation(status);
    widgets.status_label.set_label(&presentation.text);
    widgets
        .status_label
        .set_tooltip_text((!presentation.text.is_empty()).then_some(presentation.text.as_str()));
    if presentation.error {
        widgets.status_label.add_css_class("error");
        widgets.status_label.remove_css_class("dim-label");
    } else {
        widgets.status_label.remove_css_class("error");
        widgets.status_label.add_css_class("dim-label");
    }
    widgets
        .previous_button
        .set_sensitive(presentation.navigable);
    widgets.next_button.set_sensitive(presentation.navigable);
}

#[cfg(test)]
mod tests {
    use super::{
        active_pane_changed, invalid_regex_message, pane_transition, presentation,
        SearchPresentation, SearchStatus,
    };

    #[test]
    fn status_presentation_distinguishes_progress_empty_matches_and_errors() {
        assert_eq!(
            presentation(&SearchStatus::Idle),
            SearchPresentation {
                text: String::new(),
                error: false,
                navigable: false,
            }
        );
        assert_eq!(presentation(&SearchStatus::Searching).text, "Searching…");
        assert_eq!(
            presentation(&SearchStatus::results(0, 0)).text,
            "No results"
        );
        let matches = presentation(&SearchStatus::results(3, 18));
        assert_eq!(matches.text, "3 of 18");
        assert!(matches.navigable);
        let partial = presentation(&SearchStatus::partial_results(3, 200));
        assert_eq!(partial.text, "3 of 200+");
        assert!(partial.navigable);
        let unknown_zero = presentation(&SearchStatus::partial_results(0, 0));
        assert_eq!(unknown_zero.text, "? of 0+");
        assert!(!unknown_zero.navigable);

        let error = presentation(&SearchStatus::Error("Invalid regex: unclosed group".into()));
        assert!(error.error);
        assert!(!error.navigable);
    }

    #[test]
    fn native_cursor_steps_wrap_and_ignore_failed_navigation() {
        let first = SearchStatus::results(1, 3);
        assert_eq!(first.stepped(1, true), SearchStatus::results(2, 3));
        assert_eq!(
            SearchStatus::results(3, 3).stepped(1, true),
            SearchStatus::results(1, 3)
        );
        assert_eq!(
            SearchStatus::results(1, 3).stepped(-1, true),
            SearchStatus::results(3, 3)
        );
        assert_eq!(first.stepped(1, false), first);
        assert_eq!(
            SearchStatus::results(0, 3).stepped(-1, true),
            SearchStatus::results(3, 3)
        );

        let partial = SearchStatus::partial_results(1, 200);
        assert_eq!(
            partial.stepped(1, true),
            SearchStatus::partial_results(2, 200)
        );
        // Moving previous from the first known match wraps somewhere beyond
        // the bounded count, so its exact ordinal becomes explicitly unknown.
        assert_eq!(
            partial.stepped(-1, true),
            SearchStatus::partial_results(0, 200)
        );
        assert_eq!(
            SearchStatus::partial_results(200, 200).stepped(1, true),
            SearchStatus::partial_results(0, 200)
        );
    }

    #[test]
    fn regex_diagnostic_is_collapsed_to_one_actionable_line() {
        let message = invalid_regex_message("regex parse error:\n  (\n  ^\nerror: unclosed group");
        assert_eq!(message, "Invalid regex: unclosed group");
        assert!(!message.contains('\n'));
    }

    #[test]
    fn pane_transition_replays_only_an_open_search() {
        assert_eq!(
            pane_transition(true, "needle"),
            (SearchStatus::Searching, Some("needle".to_string()))
        );
        assert_eq!(
            pane_transition(true, ""),
            (SearchStatus::Idle, Some(String::new()))
        );
        assert_eq!(pane_transition(false, "needle"), (SearchStatus::Idle, None));
        assert!(!active_pane_changed(Some(7), Some(7)));
        assert!(active_pane_changed(Some(7), Some(8)));
        assert!(active_pane_changed(None, Some(8)));
    }
}
