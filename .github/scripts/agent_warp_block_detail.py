from __future__ import annotations

import re
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


def regex_once(text: str, pattern: str, replacement: str, label: str) -> str:
    updated, count = re.subn(pattern, replacement, text, count=1, flags=re.S)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return updated


alt_path = Path("src/block_view/alt_screen.rs")
alt = alt_path.read_text()
alt = replace_once(
    alt,
    "fn expand_finished_terminal_to_buffer(terminal: &Terminal, finalize: bool) {",
    "pub(crate) fn expand_finished_terminal_to_buffer(terminal: &Terminal, finalize: bool) {",
    "export finished-terminal expansion",
)
alt = replace_once(
    alt,
    """    if let Some(adj) = terminal.vadjustment() {
        adj.set_value(adj.lower());
    }
}

/// The command snapshot is the only finished VTE placed directly inside the
""",
    """    if let Some(adj) = terminal.vadjustment() {
        adj.set_value(adj.lower());
    }
}

/// Settle a finished snapshot after bytes have been fed. VTE updates its grid
/// and adjustment asynchronously, so use two idle passes: the first folds any
/// overflow/soft-wrapped rows into the widget, and the second removes the
/// temporary private scrollback once those rows are part of the card itself.
pub(crate) fn settle_finished_terminal_after_feed(terminal: &Terminal) {
    let terminal = terminal.clone();
    glib::idle_add_local_once(move || {
        expand_finished_terminal_to_buffer(&terminal, false);
        let terminal = terminal.clone();
        glib::idle_add_local_once(move || {
            expand_finished_terminal_to_buffer(&terminal, true);
            let terminal = terminal.clone();
            glib::idle_add_local_once(move || {
                if let Some(adj) = terminal.vadjustment() {
                    adj.set_value(adj.lower());
                }
            });
        });
    });
}

/// The command snapshot is the only finished VTE placed directly inside the
""",
    "insert post-feed settling helper",
)
alt = regex_once(
    alt,
    r'''    // `blocks\.rs` feeds snapshots from its own map handler\..*?    \{
        let expanded = std::cell::Cell::new\(false\);
        terminal\.connect_map\(move \|terminal\| \{
            if expanded\.replace\(true\) \{
                return;
            \}
.*?        \}\);
    \}

    // VTE consumes wheel events''',
    '''    // `blocks.rs` performs the actual feed from its map/filter paths. Keep
    // one constructor-level hook for command snapshots and other one-shot users;
    // every explicit re-feed also calls the same settling helper.
    {
        let expanded = std::cell::Cell::new(false);
        terminal.connect_map(move |terminal| {
            if expanded.replace(true) {
                return;
            }
            settle_finished_terminal_after_feed(terminal);
        });
    }

    // VTE consumes wheel events''',
    "deduplicate finished-terminal map settling",
)
alt_path.write_text(alt)

blocks_path = Path("src/block_view/blocks.rs")
blocks = blocks_path.read_text()
blocks = replace_once(
    blocks,
    """    pub(crate) action_box: gtk::Box,
    pub(crate) bookmark_star: gtk::Label,
""",
    """    pub(crate) action_box: gtk::Box,
    /// Toggle the per-block output filter without discarding its query. Exposed
    /// so the Warp-compatible keyboard action can target the selected/latest block.
    pub(crate) toggle_filter: Rc<dyn Fn()>,
    pub(crate) bookmark_star: gtk::Label,
""",
    "add filter toggle handle",
)
blocks = replace_once(
    blocks,
    """            action_box: self.action_box.clone(),
            bookmark_star: self.bookmark_star.clone(),
""",
    """            action_box: self.action_box.clone(),
            toggle_filter: self.toggle_filter.clone(),
            bookmark_star: self.bookmark_star.clone(),
""",
    "clone filter toggle handle",
)
blocks = regex_once(
    blocks,
    r'''/// Render `bytes` into a read-only finished VTE:.*?pub\(crate\) fn render_bytes_into_finished_vte\(
    vte: &vte4::Terminal,
    text: &str,
    cols: i64,
    output_rows: i64,
    viewport_cap: i64,
\) \{.*?\n\}

/// Convert logical line breaks''',
    '''/// Render `bytes` into a read-only finished VTE. Keep a generous temporary
/// scrollback while feeding: the logical/visual row estimate can still be smaller
/// than VTE's real result for cursor movement, CR redraws, combining glyphs and
/// other terminal semantics. The post-feed settling pass then expands the card to
/// the real buffer and removes that private scrollback.
pub(crate) fn render_bytes_into_finished_vte(
    vte: &vte4::Terminal,
    text: &str,
    cols: i64,
    output_rows: i64,
    viewport_cap: i64,
    capture_rows: i64,
) {
    let display_text = output_display_text(text);
    let visible_rows = output_rows.min(viewport_cap).max(1);
    let overflow_rows = output_rows
        .saturating_sub(visible_rows)
        .saturating_add(64);
    let scrollback = capture_rows.max(overflow_rows).max(64);
    vte.set_scroll_on_output(false);
    // Size and arm scrollback BEFORE reset/feed. Reset may clamp the grid on some
    // VTE builds, so both are reasserted before processing the snapshot bytes.
    vte.set_size(cols.max(1), visible_rows);
    vte.set_scrollback_lines(scrollback);
    vte.reset(true, true);
    vte.set_size(cols.max(1), visible_rows);
    vte.set_scrollback_lines(scrollback);
    vte.feed(display_text.as_bytes());
    settle_finished_terminal_after_feed(vte);
    if let Some(adj) = vte.vadjustment() {
        adj.set_value(adj.lower());
    }
}

/// Convert logical line breaks''',
    "preserve capture scrollback through output feed",
)
blocks = replace_once(
    blocks,
    """        let output_rows = output_visual_row_count(output, cols);
        let viewport_cap = output_rows.max(1);
        let max_expanded_cap = viewport_cap;
""",
    """        let output_rows = output_visual_row_count(output, cols);
        let viewport_cap = output_rows.max(1);
        let max_expanded_cap = viewport_cap;
        // Mirrors create_finished_terminal's temporary capture budget. It is a
        // limit, not an eagerly allocated grid, and is removed after each feed.
        let capture_rows = (config.truncation_threshold_lines as i64).max(4096);
""",
    "add per-block capture budget",
)
blocks = replace_once(
    blocks,
    """        // Command typically fits one line; allow a few in case of multiline pastes.
        let cmd_rows = cmd_bytes.iter().filter(|&&b| b == b'\\n').count() as i64 + 1;
        let command_vte = create_finished_terminal(config, cols, cmd_rows.max(1), 5);
""",
    """        // Allocate every logical command row up front; VTE's post-feed pass
        // adds any further rows caused by soft wrapping or control sequences.
        let cmd_rows = cmd_bytes.iter().filter(|&&b| b == b'\\n').count() as i64 + 1;
        let command_vte =
            create_finished_terminal(config, cols, cmd_rows.max(1), cmd_rows.max(1));
""",
    "remove command row cap",
)
blocks = replace_once(
    blocks,
    "let cmd_rows_for_map = cmd_rows.max(1).min(5);",
    "let cmd_rows_for_map = cmd_rows.max(1);",
    "remove command map row cap",
)
# Both initial output and filter re-render calls share this exact tail.
call_old = """                        shown_visual_rows,
                        active_cap,
                    );"""
call_new = """                        shown_visual_rows,
                        active_cap,
                        capture_rows,
                    );"""
blocks = replace_once(blocks, call_old, call_new, "add filter capture budget")
blocks = replace_once(
    blocks,
    """                render_bytes_into_finished_vte(w, &text, cols_for_map, rows, cap);""",
    """                render_bytes_into_finished_vte(
                    w,
                    &text,
                    cols_for_map,
                    rows,
                    cap,
                    capture_rows,
                );""",
    "add initial output capture budget",
)
filter_pattern = r'''        // Per-block output filter \(Warp's BlockFilterQuery\):.*?        \{
            let filter_row = gtk::Box::new\(Orientation::Horizontal, 4\);.*?            filter_btn\.connect_clicked\(move \|_\| \{.*?            \}\);
        \}

        FinishedBlock \{'''
filter_replacement = '''        // Per-block output filter (Warp's BlockFilterQuery). Closing the
        // editor disables filtering but deliberately preserves the query/options;
        // reopening it reapplies the same filter, matching Warp's toggle behavior.
        let filter_enabled = Rc::new(Cell::new(false));
        let toggle_filter: Rc<dyn Fn()> = {
            let filter_row = gtk::Box::new(Orientation::Horizontal, 4);
            filter_row.add_css_class("block-filter-row");
            filter_row.set_visible(false);
            filter_row.set_margin_start(12);
            filter_row.set_margin_end(8);
            filter_row.set_margin_top(2);
            filter_row.set_margin_bottom(2);

            let filter_entry = gtk::SearchEntry::new();
            filter_entry.set_placeholder_text(Some("Filter output…"));
            filter_entry.set_hexpand(true);
            let regex_tg = gtk::ToggleButton::with_label(".*");
            regex_tg.set_tooltip_text(Some("Regular expression"));
            let case_tg = gtk::ToggleButton::with_label("Aa");
            case_tg.set_tooltip_text(Some("Case sensitive"));
            let invert_tg = gtk::ToggleButton::with_label("!");
            invert_tg.set_tooltip_text(Some("Invert match (hide matching lines)"));
            let ctx_spin = gtk::SpinButton::with_range(0.0, 9.0, 1.0);
            ctx_spin.set_tooltip_text(Some("Lines of context around each match"));
            ctx_spin.set_value(0.0);
            let filter_status = gtk::Label::new(None);
            filter_status.add_css_class("block-filter-status");
            filter_status.set_halign(gtk::Align::Start);
            for w in [&regex_tg, &case_tg, &invert_tg] {
                w.add_css_class("flat");
                w.add_css_class("block-filter-toggle");
            }
            filter_row.append(&filter_entry);
            filter_row.append(&regex_tg);
            filter_row.append(&case_tg);
            filter_row.append(&invert_tg);
            filter_row.append(&ctx_spin);
            filter_row.append(&filter_status);

            outer.append(&filter_row);
            outer.reorder_child_after(&filter_row, Some(&cmd_row));

            let apply = {
                let output_vte = output_vte.clone();
                let full_output = full_output.clone();
                let displayed_output = displayed_output.clone();
                let filter_entry = filter_entry.clone();
                let regex_tg = regex_tg.clone();
                let case_tg = case_tg.clone();
                let invert_tg = invert_tg.clone();
                let ctx_spin = ctx_spin.clone();
                let filter_status = filter_status.clone();
                let expand_btn = expand_btn.clone();
                let expanded = expanded.clone();
                let collapsed_summary = collapsed_summary.clone();
                let filter_enabled = filter_enabled.clone();
                move || {
                    let q = filter_entry.text().to_string();
                    let full = full_output.borrow();
                    let full_rows = output_row_count(&full);
                    let shown = if !filter_enabled.get() || q.is_empty() {
                        full.to_string()
                    } else {
                        filter_output_lines(
                            full.as_str(),
                            &q,
                            regex_tg.is_active(),
                            case_tg.is_active(),
                            invert_tg.is_active(),
                            ctx_spin.value() as usize,
                        )
                    };
                    let shown_rows = output_row_count(&shown);
                    let shown_visual_rows = output_visual_row_count(&shown, cols);
                    let active_cap = if expanded.get() {
                        max_expanded_cap
                    } else {
                        viewport_cap
                    };
                    render_bytes_into_finished_vte(
                        &output_vte,
                        &shown,
                        cols,
                        shown_visual_rows,
                        active_cap,
                        capture_rows,
                    );
                    let ch = output_vte.char_height() as i32;
                    if ch > 0 {
                        output_vte.set_height_request(
                            (shown_visual_rows.min(active_cap).max(1) as i32) * ch,
                        );
                    }
                    let has_query = filter_enabled.get() && !q.trim().is_empty();
                    if has_query {
                        filter_status.set_visible(true);
                        let hidden = full_rows.saturating_sub(shown_rows);
                        if shown.trim().is_empty() {
                            filter_status.set_text("No matches");
                            filter_status.add_css_class("block-filter-empty");
                        } else {
                            filter_status.remove_css_class("block-filter-empty");
                            filter_status.set_text(&format!(
                                "{} shown, {} hidden",
                                line_count_text(shown_rows),
                                hidden
                            ));
                        }
                    } else {
                        filter_status.remove_css_class("block-filter-empty");
                        filter_status.set_visible(false);
                    }
                    expand_btn.set_visible(shown_visual_rows > viewport_cap);
                    collapsed_summary.set_label(&collapsed_output_summary(shown_rows));
                    *displayed_output.borrow_mut() = shown;
                }
            };
            let apply = Rc::new(apply);
            {
                let a = apply.clone();
                filter_entry.connect_search_changed(move |_| a());
            }
            for tg in [&regex_tg, &case_tg, &invert_tg] {
                let a = apply.clone();
                tg.connect_toggled(move |_| a());
            }
            {
                let a = apply.clone();
                ctx_spin.connect_value_changed(move |_| a());
            }

            let filter_row_for_toggle = filter_row.clone();
            let entry_for_toggle = filter_entry.clone();
            let apply_for_toggle = apply.clone();
            let filter_btn_for_toggle = filter_btn.clone();
            let filter_enabled_for_toggle = filter_enabled.clone();
            let toggle: Rc<dyn Fn()> = Rc::new(move || {
                let show = !filter_row_for_toggle.is_visible();
                filter_row_for_toggle.set_visible(show);
                filter_enabled_for_toggle.set(show);
                if show {
                    filter_btn_for_toggle.add_css_class("block-action-active");
                    entry_for_toggle.grab_focus();
                } else {
                    filter_btn_for_toggle.remove_css_class("block-action-active");
                }
                apply_for_toggle();
            });
            let toggle_for_button = toggle.clone();
            filter_btn.connect_clicked(move |_| toggle_for_button());
            toggle
        };

        FinishedBlock {'''
blocks = regex_once(blocks, filter_pattern, filter_replacement, "persist block filter state")
blocks = replace_once(
    blocks,
    """            action_box,
            bookmark_star,
""",
    """            action_box,
            toggle_filter,
            bookmark_star,
""",
    "initialize filter toggle handle",
)
blocks_path.write_text(blocks)

print("Warp block rendering/filter detail patch applied")
