//! Native rich-message widgets. Fixture data and observed runtime data share
//! the same typed renderer; this module owns no shell or agent execution.
use gtk::prelude::*;
use jterm_core::reference_conversation::{
    CodeDiff, Conversation, DiffKind, Inspector, Message, Part, Role, Step, StepState,
};
use relm4::gtk;

fn text(value: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(value));
    label.set_xalign(0.0);
    label.add_css_class(class);
    label
}

fn paragraph(value: &str, class: &str) -> gtk::Label {
    let label = text(value, class);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_selectable(true);
    label
}

pub(crate) fn role_header(role: Role, author: &str, timing: Option<&str>) -> gtk::Box {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 7);
    header.add_css_class("conversation-role-header");
    header.set_height_request(16);
    if role == Role::Shell {
        header.append(&crate::workspace_shell::icon(
            jterm_core::reference_icons::Icon::Terminal,
        ));
    }
    let author_label = text(author, "conversation-author");
    author_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    author_label.set_width_chars(1);
    author_label.set_tooltip_text(Some(author));
    header.append(&author_label);
    if let Some(timing) = timing {
        header.append(&text(&format!("·  {timing}"), "conversation-timing"));
    }
    header
}

fn state_icon(state: StepState) -> &'static str {
    match state {
        StepState::Complete => "✓",
        StepState::Running => "◉",
        StepState::Queued => "○",
    }
}

fn step_row(step: &Step, inspector: bool) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 7);
    row.set_height_request(if inspector {
        jterm_core::reference_shell::INSPECTOR_QUEUE_ROW_HEIGHT
    } else {
        jterm_core::reference_shell::TASK_ROW_HEIGHT
    } as i32);
    row.add_css_class(if inspector {
        "conversation-queue-row"
    } else {
        "conversation-step"
    });
    if step.state == StepState::Running {
        row.add_css_class("conversation-step-running");
    }
    let icon = text(state_icon(step.state), "conversation-step-icon");
    icon.set_width_chars(2);
    row.append(&icon);
    let title = text(&step.title, "conversation-step-title");
    title.set_hexpand(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_tooltip_text(Some(&format!("{}\n{}", step.title, step.detail)));
    row.append(&title);
    if let Some(elapsed) = &step.elapsed {
        row.append(&text(elapsed, "conversation-step-time"));
    }
    row
}

fn diff_text(diff: &CodeDiff) -> String {
    let mut output = format!("--- a/{}\n+++ b/{}\n", diff.path, diff.path);
    for line in &diff.lines {
        let prefix = match line.kind {
            DiffKind::Added => '+',
            DiffKind::Removed => '-',
            DiffKind::Context => ' ',
        };
        output.push(prefix);
        output.push_str(&line.text);
        output.push('\n');
    }
    output
}

fn diff_view(diff: &CodeDiff) -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("conversation-diff");
    card.set_overflow(gtk::Overflow::Hidden);
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    header.set_height_request(jterm_core::reference_shell::DIFF_HEADER_HEIGHT as i32);
    header.add_css_class("conversation-code-header");
    header.append(&crate::workspace_shell::icon(
        jterm_core::reference_icons::Icon::Code,
    ));
    let path = text(&diff.path, "conversation-code-path");
    path.set_hexpand(true);
    path.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    path.set_tooltip_text(Some(&diff.path));
    header.append(&path);
    header.append(&text(&diff.language, "conversation-code-language"));
    header.append(&text(
        &format!("+{} −{}", diff.added, diff.removed),
        "conversation-diff-count",
    ));
    let copy = gtk::Button::with_label("Copy");
    copy.set_focus_on_click(false);
    copy.add_css_class("conversation-code-action");
    let content = diff_text(diff);
    copy.connect_clicked(move |button| {
        log::debug!(target: "anvil::workspace_shell", "diff_copy_activated");
        button.clipboard().set_text(&content);
    });
    header.append(&copy);
    let apply = gtk::Button::with_label("Apply");
    apply.add_css_class("conversation-code-action");
    apply.set_sensitive(false);
    apply.set_tooltip_text(Some(if diff.can_apply {
        "An edit target must be connected before applying changes"
    } else {
        "No authorized edit target is connected"
    }));
    header.append(&apply);
    card.append(&header);
    let lines = gtk::Box::new(gtk::Orientation::Vertical, 0);
    lines.set_margin_top(jterm_core::reference_shell::DIFF_BODY_VERTICAL_PADDING as i32);
    lines.set_margin_bottom(jterm_core::reference_shell::DIFF_BODY_VERTICAL_PADDING as i32);
    for line in &diff.lines {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        row.set_height_request(jterm_core::reference_shell::DIFF_LINE_HEIGHT as i32);
        row.add_css_class("conversation-code-line");
        row.add_css_class(match line.kind {
            DiffKind::Added => "conversation-line-added",
            DiffKind::Removed => "conversation-line-removed",
            DiffKind::Context => "conversation-line-context",
        });
        let old = text(
            &line
                .old_line
                .map(|line| line.to_string())
                .unwrap_or_default(),
            "conversation-line-number",
        );
        old.set_width_request(jterm_core::reference_shell::DIFF_GUTTER_COLUMN_WIDTH as i32);
        old.set_xalign(1.0);
        row.append(&old);
        let new = text(
            &line
                .new_line
                .map(|line| line.to_string())
                .unwrap_or_default(),
            "conversation-line-number",
        );
        new.set_width_request(jterm_core::reference_shell::DIFF_GUTTER_COLUMN_WIDTH as i32);
        new.set_xalign(1.0);
        row.append(&new);
        let marker = match line.kind {
            DiffKind::Added => "+",
            DiffKind::Removed => "−",
            DiffKind::Context => " ",
        };
        let sign = text(marker, "conversation-code-text");
        sign.set_width_request(jterm_core::reference_shell::DIFF_SIGN_WIDTH as i32);
        row.append(&sign);
        let source = text(&line.text, "conversation-code-text");
        source.set_selectable(true);
        source.set_ellipsize(gtk::pango::EllipsizeMode::End);
        source.set_hexpand(true);
        source.set_tooltip_text(Some(&line.text));
        row.append(&source);
        lines.append(&row);
    }
    card.append(&lines);
    card
}

fn message_view(message: &Message) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("conversation-message");
    let user = message.role == Role::User;
    row.set_halign(if user {
        gtk::Align::End
    } else {
        gtk::Align::Fill
    });
    let avatar = text(&message.avatar, "conversation-avatar");
    avatar.set_xalign(0.5);
    avatar.set_valign(gtk::Align::Start);
    avatar.set_size_request(20, 20);
    let body = gtk::Box::new(
        gtk::Orientation::Vertical,
        jterm_core::reference_shell::MESSAGE_PART_GAP as i32,
    );
    body.set_hexpand(!user);
    if user {
        body.add_css_class("conversation-user-bubble");
        body.set_width_request(jterm_core::reference_shell::USER_BUBBLE_WIDTH as i32);
        body.set_height_request(jterm_core::reference_shell::USER_BUBBLE_HEIGHT as i32);
    } else {
        row.set_margin_top(10);
        avatar.set_margin_start(8);
        row.set_margin_end(15);
        row.append(&avatar);
        body.append(&role_header(
            message.role,
            &message.author,
            message.timing.as_deref(),
        ));
    }
    for part in &message.parts {
        match part {
            Part::Heading(value) => body.append(&paragraph(value, "conversation-heading")),
            Part::Paragraph(value) => {
                let paragraph = paragraph(value, "conversation-paragraph");
                if user {
                    paragraph.set_max_width_chars(44);
                }
                body.append(&paragraph);
            }
            Part::Metadata(values) => {
                let chips = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                chips.set_margin_top(0);
                for (index, value) in values.iter().enumerate() {
                    let chip = gtk::Box::new(gtk::Orientation::Horizontal, 4);
                    chip.add_css_class("conversation-chip");
                    chip.set_height_request(
                        jterm_core::reference_shell::METADATA_PILL_HEIGHT as i32,
                    );
                    let key = jterm_core::reference_conversation::METADATA_ICON_KEYS
                        .get(index)
                        .copied()
                        .unwrap_or("code");
                    chip.append(&crate::workspace_shell::icon(
                        jterm_core::reference_icons::Icon::from_key(key),
                    ));
                    chip.append(&text(value, "conversation-chip-text"));
                    if index + 1 == values.len() {
                        chip.add_css_class("conversation-chip-progress");
                    }
                    chips.append(&chip);
                }
                body.append(&chips);
            }
            Part::Steps(steps) => {
                let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
                list.add_css_class("conversation-steps");
                for step in steps {
                    list.append(&step_row(step, false));
                }
                body.append(&list);
            }
            Part::Diff(diff) => body.append(&diff_view(diff)),
            Part::Terminal {
                title,
                text: content,
            } => {
                let terminal = gtk::Expander::new(Some(title));
                terminal.add_css_class("conversation-inline-terminal");
                terminal
                    .set_expanded(jterm_core::reference_conversation::FIXTURE_TERMINAL_EXPANDED);
                terminal.set_child(Some(&paragraph(content, "conversation-terminal-text")));
                body.append(&terminal);
            }
        }
    }
    row.append(&body);
    if user {
        row.append(&avatar);
    }
    row
}

pub(crate) fn render(conversation: &Conversation) -> gtk::Box {
    let document = gtk::Box::new(gtk::Orientation::Vertical, 11);
    document.add_css_class("native-conversation");
    if conversation.is_fixture {
        document.set_tooltip_text(Some(jterm_core::reference_conversation::FIXTURE_LABEL));
        let date = text(
            jterm_core::reference_conversation::FIXTURE_DATE_SEPARATOR,
            "conversation-date",
        );
        date.set_xalign(0.5);
        date.set_height_request(19);
        document.append(&date);
    }
    for message in &conversation.messages {
        document.append(&message_view(message));
    }
    document
}

pub(crate) fn inspector(model: &Inspector, is_fixture: bool) -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("shell-agent-card");
    card.set_vexpand(true);
    let heading = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    heading.append(&crate::workspace_shell::icon(
        jterm_core::reference_icons::Icon::Agent,
    ));
    let title = text(&model.title, "shell-agent-title");
    title.set_hexpand(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    heading.append(&title);
    heading.append(&text(&model.badge, "shell-neutral-badge"));
    card.append(&heading);
    let subtitle = text(&model.subtitle, "shell-muted");
    subtitle.set_margin_top(6);
    subtitle
        .set_tooltip_text(is_fixture.then_some(jterm_core::reference_conversation::FIXTURE_LABEL));
    card.append(&subtitle);
    let progress = gtk::ProgressBar::new();
    progress.set_valign(gtk::Align::Center);
    progress.set_hexpand(true);
    progress.set_fraction(if model.total == 0 {
        0.0
    } else {
        model.completed as f64 / model.total as f64
    });
    let progress_line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    progress_line
        .set_margin_top(8 + jterm_core::reference_shell::INSPECTOR_PROGRESS_EXTRA_TOP as i32);
    progress_line.append(&progress);
    let count = text(
        &format!("{} / {} steps", model.completed, model.total),
        "shell-muted",
    );
    progress_line.append(&count);
    card.append(&progress_line);
    let metadata = gtk::Box::new(gtk::Orientation::Vertical, 0);
    metadata.set_margin_top(9);
    for (key, value) in &model.metadata {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_height_request(jterm_core::reference_shell::INSPECTOR_METADATA_ROW_HEIGHT as i32);
        let key = text(key, "shell-muted");
        key.set_hexpand(true);
        row.append(&key);
        let value = text(value, "conversation-inspector-value");
        value.set_xalign(1.0);
        value.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        value.set_max_width_chars(24);
        row.append(&value);
        metadata.append(&row);
    }
    card.append(&metadata);
    let current_title = text("CURRENT STEP", "shell-section");
    current_title.set_margin_top(12);
    card.append(&current_title);
    let current = gtk::Box::new(gtk::Orientation::Vertical, 2);
    current.add_css_class("conversation-current-step");
    current.set_height_request(jterm_core::reference_shell::INSPECTOR_CURRENT_HEIGHT as i32);
    current.set_margin_top(6);
    if let Some(step) = &model.current_step {
        current.append(&step_row(step, true));
        let detail = text(&step.detail, "shell-muted");
        detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
        detail.set_tooltip_text(Some(&step.detail));
        current.append(&detail);
    } else {
        current.append(&text("No task selected", "shell-muted"));
    }
    card.append(&current);
    let queue_title = text("TASK QUEUE", "shell-section");
    queue_title.set_margin_top(12);
    card.append(&queue_title);
    let queue = gtk::Box::new(
        gtk::Orientation::Vertical,
        jterm_core::reference_shell::INSPECTOR_QUEUE_GAP as i32,
    );
    queue.set_margin_top(4);
    if model.queue.is_empty() {
        queue.append(&text("No selected task", "shell-muted"));
    }
    for step in &model.queue {
        queue.append(&step_row(step, true));
    }
    card.append(&queue);
    let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    spacer.set_height_request(8);
    card.append(&spacer);
    let output_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let output_title = text(
        &model.output_title.replace(" · fixture", "").to_uppercase(),
        "shell-section",
    );
    output_title.set_hexpand(true);
    output_header.append(&output_title);
    output_header.append(&text(
        if is_fixture { "Fixture" } else { "No task" },
        "shell-neutral-badge",
    ));
    output_header.set_margin_bottom(6);
    card.append(&output_header);
    let output = paragraph(&model.output, "shell-inspector-terminal");
    output.set_height_request(jterm_core::reference_shell::INSPECTOR_OUTPUT_HEIGHT as i32);
    output.set_yalign(0.0);
    card.append(&output);
    card
}
