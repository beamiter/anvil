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
        StepState::Attention => "!",
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

// Inspector roles mirror the shared native reference while remaining separate
// from the central conversation renderer and its typography.
fn inspector_text(value: &str, class: &str) -> gtk::Label {
    let label = text(value, class);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_tooltip_text(Some(value));
    label
}

fn inspector_vector(kind: jterm_core::reference_icons::Icon, size: i32) -> gtk::DrawingArea {
    let icon = crate::workspace_shell::icon(kind);
    icon.set_content_width(size);
    icon.set_content_height(size);
    icon
}

fn inspector_step_icon(state: StepState) -> (&'static str, &'static str) {
    match state {
        StepState::Complete => ("✓", "reference-step-complete"),
        StepState::Running => ("◉", "reference-step-running"),
        StepState::Queued => ("○", "reference-step-queued"),
        StepState::Attention => ("!", "reference-step-queued"),
    }
}

fn inspector_step_row(step: &Step, details: bool) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("reference-rich-step");
    if details {
        row.add_css_class("reference-current-step");
    }
    row.set_height_request(jterm_core::developer_workspace::TASK_ROW_HEIGHT as i32);
    let (symbol, class) = inspector_step_icon(step.state);
    row.add_css_class(class);
    let symbol = inspector_text(symbol, "reference-step-symbol");
    if details {
        symbol.set_valign(gtk::Align::Center);
    }
    row.append(&symbol);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 3);
    content.set_hexpand(true);
    let title = inspector_text(&step.title, "reference-step-title");
    if details {
        content.set_valign(gtk::Align::Center);
        title.set_wrap(false);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.set_width_chars(1);
    }
    content.append(&title);
    if details {
        let detail = inspector_text(&step.detail, "reference-step-detail");
        detail.set_lines(1);
        detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
        detail.set_width_chars(1);
        content.append(&detail);
    }
    row.append(&content);
    if let Some(elapsed) = &step.elapsed {
        if elapsed != "Fixture" {
            let elapsed = inspector_text(elapsed, "reference-step-elapsed");
            elapsed.set_wrap(false);
            row.append(&elapsed);
        }
    }
    row
}

pub(crate) fn inspector(model: &Inspector, is_fixture: bool) -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 2);
    root.add_css_class("shell-agent-card");
    root.add_css_class("reference-agent-card");
    root.add_css_class("reference-rich-inspector");
    root.set_vexpand(true);
    let heading = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    heading.add_css_class("reference-inspector-agent-heading");
    heading.set_height_request(14);
    heading.append(&inspector_vector(
        jterm_core::reference_icons::Icon::Agent,
        14,
    ));
    let title = inspector_text(&model.title, "reference-card-title");
    title.set_wrap(false);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_width_chars(1);
    title.set_hexpand(true);
    heading.append(&title);
    let badge = inspector_text(&model.badge, "reference-neutral-badge");
    badge.set_wrap(false);
    badge.set_ellipsize(gtk::pango::EllipsizeMode::End);
    badge.add_css_class(if model.total > 0 || model.current_step.is_some() {
        "reference-agent-badge-active"
    } else {
        "reference-agent-badge-neutral"
    });
    heading.append(&badge);
    root.append(&heading);
    root.append(&inspector_text(&model.subtitle, "reference-muted"));
    // The idle slot has an empty track and an honest neutral label. Its height
    // matches observed-task progress without fabricating a count or activity.
    let progress_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    progress_row.set_height_request(12);
    // Compensate the shared badge's height against the original 14px heading,
    // preserving the metadata text baseline and 19px row pitch.
    progress_row.set_margin_top(
        (jterm_core::developer_workspace::INSPECTOR_PROGRESS_EXTRA_TOP + 9.0
            - (jterm_core::developer_workspace::INSPECTOR_BADGE_HEIGHT - 14.0))
            .max(0.0) as i32,
    );
    let progress = gtk::ProgressBar::new();
    progress.add_css_class("reference-inspector-progress");
    progress.set_hexpand(true);
    progress.set_valign(gtk::Align::Center);
    progress.set_fraction(if model.total == 0 {
        0.0
    } else {
        model.completed as f64 / model.total as f64
    });
    progress_row.append(&progress);
    progress_row.append(&inspector_text(
        &if model.total == 0 {
            jterm_core::developer_workspace::NO_PROGRESS_LABEL.into()
        } else {
            format!(
                "{} / {}{}",
                model.completed,
                model.total,
                if is_fixture { "" } else { " tasks" }
            )
        },
        "reference-muted",
    ));
    root.append(&progress_row);
    let metadata = gtk::Grid::new();
    metadata.set_column_spacing(14);
    metadata.set_row_spacing(0);
    metadata.set_margin_top(10);
    metadata.add_css_class("reference-inspector-metadata");
    for (row, (name, value)) in model.metadata.iter().enumerate() {
        let key = inspector_text(name, "reference-muted");
        key.add_css_class("reference-inspector-metadata-label");
        key.set_height_request(
            jterm_core::developer_workspace::INSPECTOR_METADATA_ROW_HEIGHT as i32,
        );
        metadata.attach(&key, 0, row as i32, 1, 1);
        let value = inspector_text(value, "reference-metadata");
        value.add_css_class("reference-inspector-metadata-value");
        value.set_ellipsize(gtk::pango::EllipsizeMode::End);
        value.set_wrap(false);
        value.set_width_chars(1);
        value.set_hexpand(true);
        value.set_xalign(1.0);
        metadata.attach(&value, 1, row as i32, 1, 1);
    }
    root.append(&metadata);
    let current_heading = inspector_text(
        if is_fixture {
            "CURRENT STEP"
        } else {
            "NEXT ACTION"
        },
        "reference-section-label",
    );
    current_heading.add_css_class("reference-inspector-current-heading");
    current_heading.set_margin_top(6);
    current_heading.set_margin_bottom(7);
    root.append(&current_heading);
    let current = if let Some(step) = &model.current_step {
        inspector_step_row(step, true)
    } else {
        let card = inspector_empty_card(
            "Select a task to see its next action",
            jterm_core::reference_icons::Icon::History,
        );
        card.add_css_class("reference-current-step");
        card
    };
    current.set_height_request(jterm_core::developer_workspace::INSPECTOR_CURRENT_HEIGHT as i32);
    current.add_css_class("reference-inspector-current");
    root.append(&current);
    let queue_heading = inspector_text(
        if is_fixture {
            "TASK QUEUE"
        } else {
            "OTHER TASKS"
        },
        "reference-section-label",
    );
    queue_heading.add_css_class("reference-inspector-queue-heading");
    root.append(&queue_heading);
    let queue = gtk::Box::new(
        gtk::Orientation::Vertical,
        jterm_core::developer_workspace::INSPECTOR_QUEUE_GAP as i32,
    );
    queue.add_css_class("reference-inspector-queue");
    if model.queue.is_empty() {
        let empty = inspector_text("No other tasks", "reference-muted");
        empty
            .set_height_request(jterm_core::developer_workspace::INSPECTOR_QUEUE_ROW_HEIGHT as i32);
        queue.append(&empty);
    }
    for step in &model.queue {
        let row = inspector_step_row(step, false);
        row.add_css_class("reference-inspector-queue-row");
        row.set_height_request(jterm_core::developer_workspace::INSPECTOR_QUEUE_ROW_HEIGHT as i32);
        queue.append(&row);
    }
    root.append(&queue);
    let fill = gtk::Box::new(gtk::Orientation::Vertical, 0);
    fill.set_vexpand(true);
    root.append(&fill);
    let terminal_section = gtk::Box::new(gtk::Orientation::Vertical, 2);
    terminal_section.add_css_class("reference-inspector-terminal-section");
    // Only the inter-section fill grows. The centered empty output's child
    // must not make this whole section share the spare vertical space.
    terminal_section.set_vexpand(false);
    let output_heading = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    output_heading.add_css_class("reference-inspector-output-heading");
    output_heading.set_height_request(12);
    output_heading.append(&inspector_vector(
        jterm_core::reference_icons::Icon::Terminal,
        12,
    ));
    let title = inspector_text(&model.output_title, "reference-section-label");
    title.set_hexpand(true);
    output_heading.append(&title);
    output_heading.append(&inspector_text(
        if is_fixture { "Fixture" } else { &model.badge },
        "reference-muted",
    ));
    terminal_section.append(&output_heading);
    let output = gtk::Box::new(gtk::Orientation::Vertical, 0);
    output.add_css_class("reference-rich-terminal");
    output.add_css_class("reference-inspector-output");
    let body = inspector_text(&model.output, "reference-terminal-body");
    body.set_selectable(true);
    if model.total == 0 && !is_fixture {
        output.add_css_class("reference-inspector-output-empty");
        output.set_valign(gtk::Align::Fill);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_valign(gtk::Align::Center);
        content.set_vexpand(true);
        let icon = inspector_vector(jterm_core::reference_icons::Icon::Terminal, 22);
        icon.set_halign(gtk::Align::Center);
        content.append(&icon);
        body.set_xalign(0.5);
        content.append(&body);
        output.append(&content);
    } else {
        output.append(&body);
    }
    output.set_height_request(jterm_core::developer_workspace::INSPECTOR_OUTPUT_HEIGHT as i32);
    terminal_section.append(&output);
    root.append(&terminal_section);
    root
}

fn inspector_icon(icon: jterm_core::reference_icons::Icon, size: i32) -> gtk::CenterBox {
    let tile = gtk::CenterBox::new();
    tile.add_css_class("reference-inspector-icon-tile");
    tile.set_size_request(size, size);
    tile.set_valign(gtk::Align::Center);
    tile.set_center_widget(Some(&inspector_vector(icon, 14)));
    tile
}

fn inspector_empty_card(message: &str, icon: jterm_core::reference_icons::Icon) -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    card.add_css_class("reference-inspector-empty-card");
    card.append(&inspector_icon(icon, 28));
    let label = inspector_text(message, "reference-muted");
    label.set_hexpand(true);
    card.append(&label);
    card
}

#[cfg(test)]
#[path = "workspace_conversation_tests.rs"]
mod tests;
