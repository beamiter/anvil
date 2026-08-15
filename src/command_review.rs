//! Shared command-review UI used by every Block-mode assistant surface.
//!
//! Source-specific state machines stay in their owning modules. This module
//! owns the review contract: one editable line, live risk feedback, copy,
//! explicit secondary actions, and one clearly labelled primary action.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use relm4::gtk;
use relm4::gtk::prelude::*;

const MAX_REVIEW_LABEL_BYTES: usize = 1024;
const MAX_REVIEW_DESCRIPTION_BYTES: usize = 16 * 1024;
const MAX_REVIEW_FEEDBACK_BYTES: usize = 16 * 1024;

fn safe_inline_display(text: &str, max_bytes: usize) -> String {
    crate::review_input::safe_inline_display(text, max_bytes)
}

fn safe_multiline_display(text: &str, max_bytes: usize) -> String {
    crate::review_input::safe_multiline_display(text, max_bytes)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReviewPresentation {
    Standalone,
    Embedded,
}

pub(crate) struct CommandReviewSpec {
    pub(crate) presentation: ReviewPresentation,
    pub(crate) compact: bool,
    pub(crate) icon_name: &'static str,
    pub(crate) title: String,
    pub(crate) badge: String,
    pub(crate) description: String,
    pub(crate) command: String,
    pub(crate) primary_label: String,
    pub(crate) primary_executes: bool,
    pub(crate) auxiliary_label: Option<String>,
    pub(crate) secondary_label: Option<String>,
    pub(crate) close_button: bool,
}

#[derive(Clone)]
pub(crate) struct CommandReviewCard {
    pub(crate) root: gtk::Box,
    pub(crate) entry: gtk::Entry,
    pub(crate) primary: gtk::Button,
    pub(crate) auxiliary: Option<gtk::Button>,
    pub(crate) secondary: Option<gtk::Button>,
    pub(crate) close: Option<gtk::Button>,
    pub(crate) feedback: gtk::Label,
    risk: gtk::Label,
    primary_executes: Rc<Cell<bool>>,
}

#[derive(Clone)]
pub(crate) struct CommandReviewPrimary {
    button: gtk::Button,
    risk: gtk::Label,
    executes: Rc<Cell<bool>>,
}

impl CommandReviewPrimary {
    pub(crate) fn set(&self, label: &str, executes: bool, command: &str) {
        self.button
            .set_label(&safe_inline_display(label, MAX_REVIEW_LABEL_BYTES));
        self.executes.set(executes);
        sync_risk(&self.risk, &self.button, command, executes);
    }

    pub(crate) fn executes(&self) -> bool {
        self.executes.get()
    }
}

impl CommandReviewCard {
    pub(crate) fn new(spec: CommandReviewSpec) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("command-review");
        match spec.presentation {
            ReviewPresentation::Standalone => {
                root.add_css_class("block-finished");
                root.add_css_class("block-assistant");
                root.add_css_class("command-review-standalone");
                if spec.compact {
                    root.add_css_class("block-compact");
                    root.set_margin_top(1);
                    root.set_margin_bottom(1);
                    root.set_margin_start(4);
                    root.set_margin_end(4);
                } else {
                    root.set_margin_top(4);
                    root.set_margin_bottom(4);
                    root.set_margin_start(8);
                    root.set_margin_end(8);
                }
            }
            ReviewPresentation::Embedded => root.add_css_class("command-review-embedded"),
        }
        root.set_hexpand(true);
        root.set_vexpand(false);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("command-review-header");
        if spec.presentation == ReviewPresentation::Standalone {
            header.add_css_class("block-header");
        }
        let (side_margin, top_margin, bottom_margin) =
            if spec.compact { (8, 3, 1) } else { (12, 6, 2) };
        header.set_margin_start(side_margin);
        header.set_margin_end(if spec.compact { 6 } else { 8 });
        header.set_margin_top(top_margin);
        header.set_margin_bottom(bottom_margin);

        let icon = gtk::Image::from_icon_name(spec.icon_name);
        icon.add_css_class("assistant-card-icon");
        header.append(&icon);

        let title_text = safe_inline_display(&spec.title, MAX_REVIEW_LABEL_BYTES);
        let title = gtk::Label::new(Some(&title_text));
        title.add_css_class("assistant-card-title");
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        header.append(&title);

        let badge_text = safe_inline_display(&spec.badge, MAX_REVIEW_LABEL_BYTES);
        let badge = gtk::Label::new(Some(&badge_text));
        badge.add_css_class("assistant-card-badge");
        badge.set_hexpand(true);
        badge.set_halign(gtk::Align::End);
        badge.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        badge.set_tooltip_text(Some(&badge_text));
        header.append(&badge);

        let close = spec.close_button.then(|| {
            let button = gtk::Button::from_icon_name("window-close-symbolic");
            button.add_css_class("flat");
            button.set_focusable(false);
            button.set_tooltip_text(Some("Dismiss (Esc)"));
            header.append(&button);
            button
        });
        root.append(&header);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 7);
        body.add_css_class("command-review-body");
        body.set_margin_start(side_margin);
        body.set_margin_end(side_margin);
        body.set_margin_top(2);
        body.set_margin_bottom(if spec.compact { 7 } else { 11 });

        let description_text =
            safe_multiline_display(&spec.description, MAX_REVIEW_DESCRIPTION_BYTES);
        let description = gtk::Label::new(Some(&description_text));
        description.add_css_class("command-review-description");
        description.set_xalign(0.0);
        description.set_hexpand(true);
        description.set_wrap(true);
        description.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        description.set_selectable(true);
        body.append(&description);

        let risk = gtk::Label::new(None);
        risk.add_css_class("command-review-risk");
        risk.set_xalign(0.0);
        risk.set_hexpand(true);
        risk.set_wrap(true);
        risk.set_accessible_role(gtk::AccessibleRole::Status);
        body.append(&risk);

        let (initial_command, initial_command_error) = initial_review_entry_text(&spec.command);
        let entry = gtk::Entry::new();
        entry.add_css_class("command-review-entry");
        entry.set_text(&initial_command);
        entry.set_hexpand(true);
        entry.set_width_chars(8);
        entry.update_property(&[gtk::accessible::Property::Label("Proposed shell command")]);
        body.append(&entry);

        let feedback = gtk::Label::new(None);
        feedback.add_css_class("command-review-feedback");
        feedback.set_xalign(0.0);
        feedback.set_hexpand(true);
        feedback.set_wrap(true);
        feedback.set_visible(false);
        feedback.set_accessible_role(gtk::AccessibleRole::Status);
        body.append(&feedback);
        if let Some(error) = initial_command_error.as_deref() {
            set_review_feedback(&feedback, error, true);
        }

        // FlowBox preserves every action at narrow pane widths instead of
        // clipping the right-most approval button off-screen.
        let actions = gtk::FlowBox::new();
        actions.add_css_class("command-review-actions");
        actions.set_selection_mode(gtk::SelectionMode::None);
        actions.set_homogeneous(false);
        actions.set_row_spacing(6);
        actions.set_column_spacing(6);
        actions.set_min_children_per_line(1);
        actions.set_max_children_per_line(4);
        // FlowBox must receive the body's full width before it decides where
        // to wrap. End-aligning the box itself makes GTK measure it at one
        // button wide, then reflow children outside the card's reported
        // height. Individual action ordering remains Copy → secondary →
        // primary, with every control reachable at narrow widths.
        actions.set_hexpand(true);
        actions.set_halign(gtk::Align::Fill);
        actions.set_valign(gtk::Align::Start);

        let copy = gtk::Button::with_label("Copy");
        copy.add_css_class("command-review-secondary");
        copy.set_tooltip_text(Some("Copy the command without inserting or running it"));
        actions.append(&copy);
        let auxiliary = spec.auxiliary_label.map(|label| {
            let label = safe_inline_display(&label, MAX_REVIEW_LABEL_BYTES);
            let button = gtk::Button::with_label(&label);
            button.add_css_class("command-review-secondary");
            actions.append(&button);
            button
        });
        let secondary = spec.secondary_label.map(|label| {
            let label = safe_inline_display(&label, MAX_REVIEW_LABEL_BYTES);
            let button = gtk::Button::with_label(&label);
            button.add_css_class("command-review-secondary");
            actions.append(&button);
            button
        });
        let primary_label = safe_inline_display(&spec.primary_label, MAX_REVIEW_LABEL_BYTES);
        let primary = gtk::Button::with_label(&primary_label);
        actions.append(&primary);
        body.append(&actions);
        root.append(&body);

        {
            let entry = entry.clone();
            let feedback = feedback.clone();
            copy.connect_clicked(move |button| {
                button.clipboard().set_text(&entry.text());
                set_review_feedback(&feedback, "Copied. Nothing was inserted or run.", false);
            });
        }

        let primary_executes = Rc::new(Cell::new(spec.primary_executes));
        sync_risk(&risk, &primary, &entry.text(), spec.primary_executes);
        {
            let risk = risk.clone();
            let primary = primary.clone();
            let executes = primary_executes.clone();
            let feedback = feedback.clone();
            let last_accepted = Rc::new(RefCell::new(initial_command));
            entry.connect_changed(move |entry| {
                let current = entry.text();
                let (accepted, rejected) =
                    accepted_review_entry_text(&last_accepted.borrow(), &current);
                if rejected {
                    entry.set_text(&accepted);
                    entry.set_position(accepted.chars().count() as i32);
                    set_review_feedback(
                        &feedback,
                        "Oversized edit rejected; the prior command was restored (256 KiB limit).",
                        true,
                    );
                    return;
                }
                *last_accepted.borrow_mut() = accepted;
                feedback.set_visible(false);
                sync_risk(&risk, &primary, &current, executes.get());
            });
        }

        Self {
            root,
            entry,
            primary,
            auxiliary,
            secondary,
            close,
            feedback,
            risk,
            primary_executes,
        }
    }

    pub(crate) fn validated_command(&self) -> Result<String, String> {
        crate::review_input::validate(&self.entry.text())
            .map(str::to_string)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn show_error(&self, message: &str) {
        set_review_feedback(&self.feedback, message, true);
    }

    pub(crate) fn show_info(&self, message: &str) {
        set_review_feedback(&self.feedback, message, false);
    }

    pub(crate) fn focus(&self) {
        self.entry.grab_focus();
    }

    pub(crate) fn set_primary_action(&self, label: &str, executes: bool) {
        self.primary_controller()
            .set(label, executes, &self.entry.text());
    }

    pub(crate) fn primary_executes(&self) -> bool {
        self.primary_executes.get()
    }

    pub(crate) fn primary_controller(&self) -> CommandReviewPrimary {
        CommandReviewPrimary {
            button: self.primary.clone(),
            risk: self.risk.clone(),
            executes: self.primary_executes.clone(),
        }
    }
}

fn accepted_review_entry_text(previous: &str, candidate: &str) -> (String, bool) {
    if candidate.len() <= crate::review_input::MAX_REVIEW_INPUT_BYTES {
        (candidate.to_string(), false)
    } else {
        (previous.to_string(), true)
    }
}

fn initial_review_entry_text(text: &str) -> (String, Option<String>) {
    match crate::review_input::validate(text) {
        Ok(command) => (command.to_string(), None),
        Err(error) => (
            String::new(),
            Some(format!(
                "Unsafe proposal withheld from the review field: {error}."
            )),
        ),
    }
}

fn safe_review_feedback_text(message: &str) -> String {
    safe_multiline_display(message, MAX_REVIEW_FEEDBACK_BYTES)
}

pub(crate) fn set_review_feedback(feedback: &gtk::Label, message: &str, error: bool) {
    feedback.set_text(&safe_review_feedback_text(message));
    if error {
        feedback.add_css_class("error");
    } else {
        feedback.remove_css_class("error");
    }
    feedback.set_visible(true);
}

fn sync_risk(risk: &gtk::Label, primary: &gtk::Button, command: &str, executes: bool) {
    if let Some(reason) = crate::agent::is_dangerous(command) {
        risk.set_text(&format!("Potentially destructive: {reason}"));
        risk.add_css_class("error");
        primary.remove_css_class("suggested-action");
        if executes {
            primary.add_css_class("destructive-action");
            primary.set_tooltip_text(Some(
                "Running this command requires a second exact-command confirmation",
            ));
        } else {
            primary.remove_css_class("destructive-action");
            primary.add_css_class("suggested-action");
            primary.set_tooltip_text(Some("Insert this command at the prompt without running it"));
        }
    } else {
        risk.set_text(if executes {
            "Review the exact command. It runs only after explicit approval."
        } else {
            "Review first. The primary action inserts this command but does not run it."
        });
        risk.remove_css_class("error");
        primary.remove_css_class("destructive-action");
        primary.add_css_class("suggested-action");
        primary.set_tooltip_text(Some(if executes {
            "Run this exact command after approval"
        } else {
            "Insert this command at the prompt without running it"
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::{accepted_review_entry_text, initial_review_entry_text, safe_review_feedback_text};

    #[test]
    fn oversized_edit_restores_prior_command_instead_of_running_a_prefix() {
        let prior = "printf safe";
        let input = format!(
            "{}界",
            "a".repeat(crate::review_input::MAX_REVIEW_INPUT_BYTES - 1)
        );
        let (accepted, rejected) = accepted_review_entry_text(prior, &input);
        assert!(rejected);
        assert_eq!(accepted, prior);

        let (accepted, rejected) = accepted_review_entry_text(prior, "echo changed");
        assert!(!rejected);
        assert_eq!(accepted, "echo changed");
    }

    #[test]
    fn feedback_is_bounded_and_neutralises_visual_controls() {
        let feedback = safe_review_feedback_text(&format!(
            "bad\u{202e}\u{fff0}\u{e0080}{}",
            "x".repeat(32 * 1024)
        ));
        assert!(feedback.len() <= super::MAX_REVIEW_FEEDBACK_BYTES);
        assert!(!feedback.contains('\u{202e}'));
        assert!(!feedback.contains('\u{fff0}'));
        assert!(!feedback.contains('\u{e0080}'));
    }

    #[test]
    fn invalid_initial_proposal_is_withheld_instead_of_normalized() {
        for command in [
            "echo one\necho two".to_string(),
            "echo safe\u{202e}txt".to_string(),
            "x".repeat(crate::review_input::MAX_REVIEW_INPUT_BYTES + 1),
        ] {
            let (entry, error) = initial_review_entry_text(&command);
            assert!(entry.is_empty());
            assert!(error.is_some());
        }

        let (entry, error) = initial_review_entry_text("printf '%s' safe");
        assert_eq!(entry, "printf '%s' safe");
        assert!(error.is_none());
    }
}
