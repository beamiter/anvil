//! Native presentation of retained command records. No terminal input lives here.
use super::{blocks::BlockStatus, command_row::CommandRow};
use gtk::prelude::*;
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::{Terminal, TerminalExt};

pub(super) const HEADER_HEIGHT: i32 = 40;
pub(super) const GUTTER_WIDTH: i32 = 24;
pub(super) const OUTPUT_GAP: i32 = 16;

pub(super) fn trace_label_font(label: &gtk::Label, role: &'static str) {
    if std::env::var_os("ANVIL_BLOCK_REFERENCE_METRICS").is_none() {
        return;
    }
    label.connect_map(move |label| {
        let font=label.layout().font_description().or_else(||label.pango_context().font_description());
        if let Some(font)=font {
            log::info!(target:"anvil::block_reference","font role={role} description={font} pango_units={} absolute={} dpr={}",font.size(),font.is_size_absolute(),label.scale_factor());
        }
    });
}

fn status_badge(text: &str, class: &str, role: &'static str) -> gtk::Box {
    use jterm_core::reference_icons::Icon;
    let badge = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    badge.set_valign(gtk::Align::Center);
    badge.set_height_request(20);
    badge.add_css_class("block-reference-status");
    badge.add_css_class(class);
    let symbol = match class {
        "completed" => Some(Icon::Check),
        "failed" => Some(Icon::Close),
        "running" => Some(Icon::History),
        _ => None,
    };
    if let Some(symbol) = symbol {
        let icon = crate::workspace_shell::icon(symbol);
        icon.set_content_width(12);
        icon.set_content_height(12);
        badge.append(&icon);
    }
    let label = gtk::Label::new(Some(text));
    label.set_wrap(false);
    trace_label_font(&label, role);
    badge.append(&label);
    badge.set_tooltip_text(Some(text));
    badge
}

#[derive(Clone)]
pub(super) struct LiveChrome {
    pub root: gtk::Box,
    command: gtk::Label,
    elapsed: gtk::Label,
}

impl LiveChrome {
    pub(super) fn new() -> Self {
        // Forge's developer card: 16px leading inset, 12px trailing inset,
        // 8px between the icon tile and command, 8px between every control.
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        root.add_css_class("block-reference-header");
        root.add_css_class("anvil-ui-font");
        root.set_margin_start(16);
        root.set_margin_end(12);
        root.set_height_request(HEADER_HEIGHT);
        let icon = crate::workspace_shell::icon(jterm_core::reference_icons::Icon::Terminal);
        icon.set_size_request(24, 24);
        icon.set_margin_end(8);
        icon.add_css_class("block-reference-command-icon");
        root.append(&icon);
        let command = gtk::Label::new(None);
        command.set_xalign(0.0);
        command.set_hexpand(true);
        command.set_ellipsize(gtk::pango::EllipsizeMode::End);
        command.set_width_chars(1);
        command.add_css_class("block-reference-live-command");
        trace_label_font(&command, "running-command");
        root.append(&command);
        let status = status_badge("Running", "running", "running-status");
        root.append(&status);
        let elapsed = gtk::Label::new(None);
        elapsed.add_css_class("block-reference-elapsed");
        root.append(&elapsed);
        root.set_visible(false);
        Self {
            root,
            command,
            elapsed,
        }
    }

    pub(super) fn update(&self, command: &str, seconds: u64, visible: bool) {
        let text = jterm_core::review_input::safe_inline_display(command, 4096);
        if self.command.text().as_str() != text {
            self.command.set_text(&text);
            self.command.set_tooltip_text(Some(&text));
        }
        let elapsed = super::unified_chrome::format_elapsed_clock(seconds);
        if self.elapsed.text().as_str() != elapsed {
            self.elapsed.set_text(&elapsed);
        }
        self.root.set_visible(visible);
    }
}

/// Labels mark logical source lines. Soft-wrapped continuation rows have no
/// repeated number. ANSI cursor/erase replay uses the existing plain renderer.
/// The cache is invalidated by actual terminal content/column changes, not a
/// per-frame text scan. Numbers never enter either VTE or any copy buffer.
fn line_starts(text: &str, columns: i64) -> Vec<(i64, usize)> {
    // Match render_bytes_into_finished_vte: remove one native display boundary
    // from the ANSI stream before replaying controls. Additional blank rows,
    // or LF after a leading SGR sequence, are real terminal rows.
    let plain = super::ansi::strip_ansi(super::blocks::output_display_text(text));
    let mut row = 0;
    plain
        .split('\n')
        .enumerate()
        .map(|(index, line)| {
            let entry = (row, index + 1);
            row += super::blocks::output_visual_row_count(line, columns);
            entry
        })
        .collect()
}

/// Shared with the Block document's initial estimate and viewport refit.
/// The gap belongs inside the indexed outer holder, outside its painted child.
pub(super) fn estimated_height(cell: i32, command_rows: i64, output_rows: i64) -> i32 {
    let header =
        super::alt_screen::finished_vte_height_px(command_rows.max(1), cell).max(HEADER_HEIGHT);
    header
        .saturating_add(if output_rows > 0 {
            super::alt_screen::finished_vte_height_px(output_rows, cell).saturating_add(12)
        } else {
            0
        })
        .saturating_add(2 + 10)
}

pub(super) fn output_gutter(
    terminal: &Terminal,
    full_output: Rc<RefCell<String>>,
    displayed_output: Rc<RefCell<Option<String>>>,
    generation: Rc<Cell<u64>>,
) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.add_css_class("block-reference-gutter");
    area.set_content_width(GUTTER_WIDTH);
    area.set_hexpand(false);
    area.set_can_target(false);
    area.set_focusable(false);
    let cache = Rc::new(RefCell::new(None::<(i64, u64, Vec<(i64, usize)>)>));
    let weak_terminal = terminal.downgrade();
    let rows = cache.clone();
    let sampled = Cell::new(false);
    #[allow(deprecated)]
    area.set_draw_func(move |area, cr, width, height| {
        let Some(terminal) = weak_terminal.upgrade() else {
            return;
        };
        let columns = terminal.column_count().max(1);
        if !sampled.replace(true) && std::env::var_os("ANVIL_BLOCK_REFERENCE_METRICS").is_some() {
            log::info!(target:"anvil::block_reference","font role=output description={} scale={} cell={}x{} dpr={}",terminal.font().map_or_else(||"unavailable".to_string(),|font|font.to_string()),terminal.font_scale(),terminal.char_width(),terminal.char_height(),terminal.scale_factor());
        }
        let mut cache = rows.borrow_mut();
        if cache
            .as_ref()
            .is_none_or(|(old, stamp, _)| *old != columns || *stamp != generation.get())
        {
            let displayed = displayed_output.borrow();
            let source = full_output.borrow();
            let text = displayed.as_deref().unwrap_or(source.as_str());
            *cache = Some((columns, generation.get(), line_starts(text, columns)));
        }
        let value = terminal
            .vadjustment()
            .map_or(0.0, |adjustment| adjustment.value());
        let cell = terminal.char_height().max(1) as f64;
        let padding = terminal.style_context().padding();
        let first = value.floor() as i64;
        let count = (height as f64 / cell).ceil() as i64 + 1;
        let labels = &cache.as_ref().expect("gutter cache").2;
        let start = labels.partition_point(|(row, _)| *row < first);
        for &(row, number) in &labels[start..] {
            if row > first + count {
                break;
            }
            let layout = area.create_pango_layout(Some(&number.to_string()));
            if let Some(mut font) = terminal.font() {
                if font.is_size_absolute() {
                    font.set_absolute_size(font.size() as f64 * terminal.font_scale());
                } else {
                    font.set_size((font.size() as f64 * terminal.font_scale()).round() as i32);
                }
                layout.set_font_description(Some(&font));
            }
            let (w, h) = layout.pixel_size();
            let y =
                (row as f64 - value) * cell + f64::from(padding.top()) + (cell - h as f64) / 2.0;
            gtk::render_layout(&area.style_context(), cr, (width - w) as f64, y, &layout);
        }
    });
    let weak = area.downgrade();
    terminal.connect_contents_changed(move |_| {
        if let Some(area) = weak.upgrade() {
            area.queue_draw();
        }
    });
    let weak = area.downgrade();
    terminal.connect_char_size_changed(move |_, _, _| {
        if let Some(area) = weak.upgrade() {
            area.queue_draw();
        }
    });
    if let Some(adjustment) = terminal.vadjustment() {
        let weak = area.downgrade();
        adjustment.connect_value_changed(move |_| {
            if let Some(area) = weak.upgrade() {
                area.queue_draw();
            }
        });
    }
    area
}

fn status_text(status: BlockStatus) -> (&'static str, &'static str) {
    match status {
        BlockStatus::Succeeded => ("Completed", "completed"),
        BlockStatus::Failed(_) => ("Failed", "failed"),
        BlockStatus::Interrupted(_) => ("Interrupted", "neutral"),
        BlockStatus::Background => ("Background", "neutral"),
        BlockStatus::Unreported => ("Unreported", "neutral"),
    }
}

pub(super) struct FinishedChrome<'a> {
    pub outer: &'a gtk::Box,
    pub content: &'a gtk::Box,
    pub header: &'a gtk::Box,
    pub command: &'a CommandRow,
    pub command_vte: &'a Terminal,
    pub command_text: &'a str,
    pub metadata: &'a gtk::Widget,
    pub status: BlockStatus,
    pub duration_ms: Option<u64>,
    pub cwd: Option<&'a str>,
    pub more: &'a gtk::Button,
    pub actions: &'a gtk::Box,
    pub collapse: &'a gtk::Button,
}

pub(super) fn decorate_finished(chrome: FinishedChrome<'_>) {
    let FinishedChrome {
        outer,
        content,
        header,
        command,
        command_vte,
        command_text,
        metadata,
        status,
        duration_ms,
        cwd,
        more,
        actions,
        collapse,
    } = chrome;
    outer.add_css_class("block-reference-card");
    outer.set_margin_start(12);
    outer.set_margin_end(12);
    outer.set_margin_top(0);
    outer.set_margin_bottom(0);
    content.add_css_class("block-reference-card-surface");
    content.set_margin_bottom(10);
    header.add_css_class("block-reference-header");
    header.add_css_class("anvil-ui-font");
    header.set_height_request(HEADER_HEIGHT);
    header.set_margin_start(16);
    header.set_margin_end(12);
    header.set_margin_top(0);
    header.set_margin_bottom(0);
    header.set_spacing(8);
    // Preserve action objects and callbacks. The existing native More menu
    // exposes them without reserving an invisible quick-action strip's width.
    actions.remove(more);
    actions.set_visible(false);
    // Existing collapse actions intentionally hold only a weak button edge.
    // Keep the hidden native owner in the header so More/keyboard toggles work.
    collapse.set_visible(false);
    let mut child = header.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if widget != *actions.upcast_ref::<gtk::Widget>() {
            widget.set_visible(false);
        }
    }
    if metadata.parent().as_ref() == Some(content.upcast_ref()) {
        content.remove(metadata);
    }
    content.remove(command);
    super::command_row::fit(
        command,
        command_vte,
        command_vte.column_count().max(2),
        command_text,
    );
    command.set_halign(gtk::Align::Fill);
    command.set_hexpand(true);
    command.add_css_class("block-reference-command");
    command_vte.add_css_class("block-command-terminal");
    // A fully transparent VTE can clear its snapshot to the toolkit's light
    // default. Give the read-only command the header's actual dark surface.
    command_vte.set_color_background(&super::alt_screen::command_header_background());
    let icon = crate::workspace_shell::icon(jterm_core::reference_icons::Icon::Terminal);
    icon.set_size_request(24, 24);
    icon.set_margin_end(8);
    icon.add_css_class("block-reference-command-icon");
    let (text, class) = status_text(status);
    icon.add_css_class(class);
    header.append(&icon);
    header.append(command);
    if let Some(cwd) = cwd {
        command.set_tooltip_text(Some(cwd));
    }
    let badge = status_badge(text, class, "completed-status");
    header.append(&badge);
    if let Some(duration) = duration_ms {
        let elapsed = gtk::Label::new(Some(&super::format_block_duration(duration)));
        elapsed.add_css_class("block-reference-elapsed");
        elapsed.set_tooltip_text(Some(&format!("{duration} ms")));
        header.append(&elapsed);
    }
    more.add_css_class("block-reference-more");
    more.set_valign(gtk::Align::Center);
    more.set_child(Some(&crate::workspace_shell::icon(
        jterm_core::reference_icons::Icon::More,
    )));
    header.append(more);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gutter_labels_logical_lines_and_leaves_soft_wraps_blank() {
        assert_eq!(
            line_starts("\x1b[31m123456789\x1b[0m\nnext\n", 4),
            vec![(0, 1), (3, 2)]
        );
        assert_eq!(
            status_text(BlockStatus::Unreported),
            ("Unreported", "neutral")
        );
        assert_eq!(status_text(BlockStatus::Failed(1)), ("Failed", "failed"));
        assert_eq!(
            line_starts("\r\n12345\n\n\n", 4),
            vec![(0, 1), (2, 2), (3, 3)]
        );
        assert_eq!(
            line_starts("\x1b[31m\nfirst\x1b[0m", 80),
            vec![(0, 1), (1, 2)]
        );
    }
}
