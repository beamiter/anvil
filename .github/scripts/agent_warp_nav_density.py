from __future__ import annotations

from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


# ---------------------------------------------------------------------------
# src/block_view/blocks.rs
# ---------------------------------------------------------------------------
path = Path("src/block_view/blocks.rs")
text = path.read_text()

text = replace_once(
    text,
    r'''    pub(crate) action_box: gtk::Box,
    /// Toggle the per-block output filter without discarding its query. Exposed
    /// so the Warp-compatible keyboard action can target the selected/latest block.
    pub(crate) toggle_filter: Rc<dyn Fn()>,
    pub(crate) bookmark_star: gtk::Label,
    pub(crate) status_icon: gtk::Label,
    /// Column count the output VTE is sized to — needed for re-feed (filter).
    pub(crate) cols: i64,
    /// Number of rows allocated to this finished output. Kept with the widget
    /// so filter re-renders use the same full-height canvas allocation.
    pub(crate) viewport_cap: i64,
    /// True only when this block has more output rows than can be shown at once.
    pub(crate) output_scrollable: bool,
''',
    r'''    pub(crate) action_box: gtk::Box,
    /// Toggle the per-block output filter without discarding its query. Exposed
    /// so the Warp-compatible keyboard action can target the selected/latest block.
    pub(crate) toggle_filter: Rc<dyn Fn()>,
    /// Explicit Warp-style navigation affordance for oversized output.
    pub(crate) jump_bottom_btn: gtk::Button,
    pub(crate) bookmark_star: gtk::Label,
    pub(crate) status_icon: gtk::Label,
    /// Column count the output VTE is sized to — needed for re-feed (filter).
    pub(crate) cols: i64,
    /// Number of rows allocated to this finished output. Kept with the widget
    /// so filter re-renders use the same full-height canvas allocation.
    pub(crate) viewport_cap: i64,
    /// True only when this block has more output rows than can be shown at once.
    pub(crate) output_scrollable: bool,
    /// Whether this block is tall enough to expose long-block navigation.
    pub(crate) long_output: bool,
''',
    "add finished block navigation fields",
)

text = replace_once(
    text,
    r'''            header_row: self.header_row.clone(),
            action_box: self.action_box.clone(),
            toggle_filter: self.toggle_filter.clone(),
            bookmark_star: self.bookmark_star.clone(),
            status_icon: self.status_icon.clone(),
            cols: self.cols,
            viewport_cap: self.viewport_cap,
            output_scrollable: self.output_scrollable,
''',
    r'''            header_row: self.header_row.clone(),
            action_box: self.action_box.clone(),
            toggle_filter: self.toggle_filter.clone(),
            jump_bottom_btn: self.jump_bottom_btn.clone(),
            bookmark_star: self.bookmark_star.clone(),
            status_icon: self.status_icon.clone(),
            cols: self.cols,
            viewport_cap: self.viewport_cap,
            output_scrollable: self.output_scrollable,
            long_output: self.long_output,
''',
    "clone finished block navigation fields",
)

text = replace_once(
    text,
    r'''fn forward_outer_scroll(outer: &gtk::ScrolledWindow, dy: f64) {
    let outer_adj = outer.vadjustment();
    let step = outer_adj.step_increment().max(outer_adj.page_size() * 0.1);
    let max_value = (outer_adj.upper() - outer_adj.page_size()).max(outer_adj.lower());
    let target = (outer_adj.value() + dy * step).clamp(outer_adj.lower(), max_value);
    outer_adj.set_value(target);
}

#[cfg(test)]
''',
    r'''fn forward_outer_scroll(outer: &gtk::ScrolledWindow, dy: f64) {
    let outer_adj = outer.vadjustment();
    let step = outer_adj.step_increment().max(outer_adj.page_size() * 0.1);
    let max_value = (outer_adj.upper() - outer_adj.page_size()).max(outer_adj.lower());
    let target = (outer_adj.value() + dy * step).clamp(outer_adj.lower(), max_value);
    outer_adj.set_value(target);
}

/// Convert a block's viewport-relative geometry into an absolute outer-scroll
/// target. All entry points (button, shortcut, context menu, sticky header) use
/// this one calculation so long-block navigation lands identically.
fn block_edge_scroll_target(
    current: f64,
    relative_top: f64,
    block_height: f64,
    page_size: f64,
    lower: f64,
    upper: f64,
    bottom: bool,
) -> f64 {
    let max_value = (upper - page_size).max(lower);
    let absolute_top = current + relative_top;
    let target = if bottom {
        absolute_top + block_height - page_size
    } else {
        absolute_top
    };
    target.clamp(lower, max_value)
}

#[cfg(test)]
''',
    "add block edge target helper",
)

text = replace_once(
    text,
    r'''    #[test]
    fn filter_output_lines_matches_case_insensitively_without_regex() {
        assert_eq!(
            filter_output_lines("alpha\nBeta\ngamma", "BETA", false, false, false, 0),
            "Beta"
        );
    }
}
''',
    r'''    #[test]
    fn filter_output_lines_matches_case_insensitively_without_regex() {
        assert_eq!(
            filter_output_lines("alpha\nBeta\ngamma", "BETA", false, false, false, 0),
            "Beta"
        );
    }

    #[test]
    fn block_edge_target_uses_absolute_canvas_coordinates() {
        assert_eq!(
            block_edge_scroll_target(300.0, 50.0, 800.0, 200.0, 0.0, 2000.0, false),
            350.0
        );
        assert_eq!(
            block_edge_scroll_target(300.0, 50.0, 800.0, 200.0, 0.0, 2000.0, true),
            950.0
        );
        assert_eq!(
            block_edge_scroll_target(1500.0, 100.0, 800.0, 200.0, 0.0, 1800.0, true),
            1600.0
        );
    }
}
''',
    "test block edge target",
)

text = replace_once(
    text,
    r'''        let output_rows = output_visual_row_count(output, cols);
        let viewport_cap = output_rows.max(1);
        let max_expanded_cap = viewport_cap;
        // Mirrors create_finished_terminal's temporary capture budget. It is a
        // limit, not an eagerly allocated grid, and is removed after each feed.
        let capture_rows = (config.truncation_threshold_lines as i64).max(4096);

        let outer = if let Some(reused) = recycled {
            while let Some(child) = reused.first_child() {
                reused.remove(&child);
            }
            reused.remove_css_class("block-hovered");
            reused.remove_css_class("block-selected");
            reused.remove_css_class("block-success");
            reused.remove_css_class("block-failed");
            reused
        } else {
            let b = gtk::Box::new(Orientation::Vertical, 0);
            b.add_css_class("block-finished");
            b.set_margin_top(4);
            b.set_margin_bottom(4);
            b.set_margin_start(8);
            b.set_margin_end(8);
            b
        };
''',
    r'''        let output_rows = output_visual_row_count(output, cols);
        let viewport_cap = output_rows.max(1);
        let max_expanded_cap = viewport_cap;
        let long_output = output_rows > config.finished_block_viewport_rows.max(3) as i64;
        // Mirrors create_finished_terminal's temporary capture budget. It is a
        // limit, not an eagerly allocated grid, and is removed after each feed.
        let capture_rows = (config.truncation_threshold_lines as i64).max(4096);

        let outer = if let Some(reused) = recycled {
            while let Some(child) = reused.first_child() {
                reused.remove(&child);
            }
            reused.remove_css_class("block-hovered");
            reused.remove_css_class("block-selected");
            reused.remove_css_class("block-success");
            reused.remove_css_class("block-failed");
            reused
        } else {
            let b = gtk::Box::new(Orientation::Vertical, 0);
            b.add_css_class("block-finished");
            b
        };
        if config.block_compact {
            outer.add_css_class("block-compact");
            outer.set_margin_top(1);
            outer.set_margin_bottom(1);
            outer.set_margin_start(4);
            outer.set_margin_end(4);
        } else {
            outer.remove_css_class("block-compact");
            outer.set_margin_top(4);
            outer.set_margin_bottom(4);
            outer.set_margin_start(8);
            outer.set_margin_end(8);
        }
''',
    "apply compact finished block geometry",
)

text = replace_once(
    text,
    r'''        header_row.set_tooltip_text(Some(
            "Click to select · Enter recalls command · Ctrl+B toggles bookmark",
        ));
        header_row.set_margin_start(12);
        header_row.set_margin_end(8);
        header_row.set_margin_top(6);
        header_row.set_margin_bottom(2);
''',
    r'''        header_row.set_tooltip_text(Some(
            "Click to select · Enter recalls command · Ctrl+Shift+B toggles bookmark",
        ));
        if config.block_compact {
            header_row.set_margin_start(8);
            header_row.set_margin_end(6);
            header_row.set_margin_top(3);
            header_row.set_margin_bottom(1);
        } else {
            header_row.set_margin_start(12);
            header_row.set_margin_end(8);
            header_row.set_margin_top(6);
            header_row.set_margin_bottom(2);
        }
''',
    "compact finished header and update bookmark tooltip",
)

text = replace_once(
    text,
    r'''        let filter_btn = gtk::Button::with_label("\u{f0b0}"); // nf-fa-filter  filter output
        filter_btn.set_tooltip_text(Some("Filter output"));
        // Expand button: appears only when output_rows > viewport_cap; toggles
        // the output VTE between the capped height and a roomier expanded height
        // (`finished_block_max_expanded_rows`). Wired below once output_rows and
        // the output VTE exist.
        let expand_btn = gtk::Button::with_label("\u{f065}"); // nf-fa-expand
        expand_btn.set_tooltip_text(Some("Expand block"));
        for btn in [
            &copy_cmd_btn,
            &copy_output_btn,
            &rerun_btn,
            &filter_btn,
            &expand_btn,
        ] {
''',
    r'''        let filter_btn = gtk::Button::with_label("\u{f0b0}"); // nf-fa-filter  filter output
        filter_btn.set_tooltip_text(Some("Filter output"));
        let jump_bottom_btn = gtk::Button::with_label("\u{f103}"); // nf-fa-angle-double-down
        jump_bottom_btn.set_tooltip_text(Some("Jump to bottom of this block"));
        jump_bottom_btn.set_visible(long_output);
        // Expand button: appears only when output_rows > viewport_cap; toggles
        // the output VTE between the capped height and a roomier expanded height
        // (`finished_block_max_expanded_rows`). Wired below once output_rows and
        // the output VTE exist.
        let expand_btn = gtk::Button::with_label("\u{f065}"); // nf-fa-expand
        expand_btn.set_tooltip_text(Some("Expand block"));
        for btn in [
            &copy_cmd_btn,
            &copy_output_btn,
            &rerun_btn,
            &filter_btn,
            &jump_bottom_btn,
            &expand_btn,
        ] {
''',
    "add jump-bottom quick action",
)

text = replace_once(
    text,
    r'''            header_row,
            action_box,
            toggle_filter,
            bookmark_star,
            status_icon,
            cols,
            viewport_cap,
            output_scrollable,
''',
    r'''            header_row,
            action_box,
            toggle_filter,
            jump_bottom_btn,
            bookmark_star,
            status_icon,
            cols,
            viewport_cap,
            output_scrollable,
            long_output,
''',
    "return finished block navigation fields",
)

text = replace_once(
    text,
    r'''    pub(crate) fn widget(&self) -> &gtk::Box {
        &self.widget
    }

    /// Forward wheel events from full-height output VTEs to the one outer
    /// ScrolledWindow so all finished blocks behave as a continuous canvas.
    pub(crate) fn connect_scroll_forwarding(&self, outer: &gtk::ScrolledWindow) {
        let scroll_ctrl =
''',
    r'''    pub(crate) fn widget(&self) -> &gtk::Box {
        &self.widget
    }

    /// Scroll this block's command edge or output edge into the outer history
    /// canvas. The child VTEs never own this navigation.
    pub(crate) fn scroll_to_edge(&self, outer: &gtk::ScrolledWindow, bottom: bool) {
        let widget = self.widget.clone();
        let outer = outer.clone();
        glib::idle_add_local_once(move || {
            let Some(bounds) = widget.compute_bounds(&outer) else {
                return;
            };
            let adj = outer.vadjustment();
            let target = block_edge_scroll_target(
                adj.value(),
                bounds.y() as f64,
                bounds.height() as f64,
                adj.page_size(),
                adj.lower(),
                adj.upper(),
                bottom,
            );
            adj.set_value(target);
        });
    }

    /// Forward wheel events from full-height output VTEs to the one outer
    /// ScrolledWindow so all finished blocks behave as a continuous canvas.
    pub(crate) fn connect_scroll_forwarding(&self, outer: &gtk::ScrolledWindow) {
        let block_for_jump = self.clone();
        let outer_for_jump = outer.clone();
        self.jump_bottom_btn.connect_clicked(move |_| {
            block_for_jump.scroll_to_edge(&outer_for_jump, true);
        });

        let scroll_ctrl =
''',
    "add shared block edge scrolling and quick action handler",
)

text = replace_once(
    text,
    r'''        let widget = gtk::Box::new(Orientation::Vertical, 0);
        widget.add_css_class("block-active");
        // focusable(false) keeps the holder Box from being a focus target, but we
''',
    r'''        let widget = gtk::Box::new(Orientation::Vertical, 0);
        widget.add_css_class("block-active");
        if config.block_compact {
            widget.add_css_class("block-compact");
        }
        // focusable(false) keeps the holder Box from being a focus target, but we
''',
    "apply compact mode to active block",
)

path.write_text(text)


# ---------------------------------------------------------------------------
# src/block_view/mod.rs
# ---------------------------------------------------------------------------
path = Path("src/block_view/mod.rs")
text = path.read_text()

text = replace_once(
    text,
    r'''fn move_finished_block_selection(
    finished: &[FinishedBlock],
    selected_block_id: &Rc<Cell<Option<u64>>>,
    scroll: &ScrolledWindow,
    direction: i32,
) -> bool {
    if finished.is_empty() || direction == 0 {
        return false;
    }
    let current = selected_block_id
        .get()
        .and_then(|id| finished.iter().position(|block| block.id == id));
    let target = if direction < 0 {
        match current {
            None => Some(finished.len() - 1),
            Some(0) => Some(0),
            Some(index) => Some(index - 1),
        }
    } else {
        match current {
            None => return false,
            Some(index) if index + 1 >= finished.len() => None,
            Some(index) => Some(index + 1),
        }
    };
    let target_id = target.and_then(|index| finished.get(index).map(|block| block.id));
    select_finished_block(finished, selected_block_id, target_id);
    if let Some(index) = target {
        if let Some(block) = finished.get(index) {
            scroll_finished_block_into_view(block, scroll);
        }
    }
    true
}

/// Install the shared click-to-select behavior for a finished block. New blocks
''',
    r'''fn move_finished_block_selection(
    finished: &[FinishedBlock],
    selected_block_id: &Rc<Cell<Option<u64>>>,
    scroll: &ScrolledWindow,
    direction: i32,
) -> bool {
    if finished.is_empty() || direction == 0 {
        return false;
    }
    let current = selected_block_id
        .get()
        .and_then(|id| finished.iter().position(|block| block.id == id));
    let target = if direction < 0 {
        match current {
            None => Some(finished.len() - 1),
            Some(0) => Some(0),
            Some(index) => Some(index - 1),
        }
    } else {
        match current {
            None => return false,
            Some(index) if index + 1 >= finished.len() => None,
            Some(index) => Some(index + 1),
        }
    };
    let target_id = target.and_then(|index| finished.get(index).map(|block| block.id));
    select_finished_block(finished, selected_block_id, target_id);
    if let Some(index) = target {
        if let Some(block) = finished.get(index) {
            scroll_finished_block_into_view(block, scroll);
        }
    }
    true
}

fn scroll_selected_finished_block_edge(
    finished: &[FinishedBlock],
    selected_block_id: &Rc<Cell<Option<u64>>>,
    scroll: &ScrolledWindow,
    bottom: bool,
) -> bool {
    let Some(id) = selected_block_id.get() else {
        return false;
    };
    let Some(block) = finished.iter().find(|block| block.id == id) else {
        return false;
    };
    block.scroll_to_edge(scroll, bottom);
    true
}

/// Install the shared click-to-select behavior for a finished block. New blocks
''',
    "add selected block edge helper",
)

text = replace_once(
    text,
    r'''            // Keep the existing bracket aliases for window-manager/keybinding
            // conflicts, but route them through the same selection semantics.
            if ctrl && shift && !alt && matches!(keyval, Key::bracketleft | Key::bracketright) {
''',
    r'''            // Warp Linux: Ctrl+Shift+Up/Down jumps to the top/bottom edge
            // of the currently selected block.
            if ctrl && shift && !alt && matches!(keyval, Key::Up | Key::Down) {
                let finished = finished_blocks_for_key.borrow();
                if scroll_selected_finished_block_edge(
                    &finished,
                    &selected_block_id_for_key,
                    &block_scroll_for_key,
                    keyval == Key::Down,
                ) {
                    return glib::Propagation::Stop;
                }
            }

            // Keep the existing bracket aliases for window-manager/keybinding
            // conflicts, but route them through the same selection semantics.
            if ctrl && shift && !alt && matches!(keyval, Key::bracketleft | Key::bracketright) {
''',
    "add selected block top bottom shortcuts",
)

text = replace_once(
    text,
    r'''            // Ctrl+B: toggle a bookmark on the selected block (Warp's
            // ToggleBookmarkBlock). Shows the gutter star + accent stripe.
            // Only consume the key when bookmark logic actually fires — in
            // alt-screen (vim/less) or with no selection, let VTE deliver
            // Ctrl+B to the running app (e.g. vim's page-up).
            if ctrl
                && !shift
                && !alt
                && matches!(keyval, Key::b | Key::B)
''',
    r'''            // Ctrl+Shift+B: toggle a bookmark on the selected block (Warp's
            // Linux binding). Shows the gutter star + accent stripe.
            // Only consume the key when bookmark logic actually fires.
            if ctrl
                && shift
                && !alt
                && matches!(keyval, Key::b | Key::B)
''',
    "align bookmark shortcut",
)

text = replace_once(
    text,
    r'''        let sticky_label = gtk::Label::new(None);
        sticky_label.set_halign(gtk::Align::Start);
        sticky_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        sticky_label.set_hexpand(true);
        sticky_label.add_css_class("sticky-running-label");
        let sticky_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        sticky_bar.add_css_class("sticky-running-header");
        sticky_bar.append(&sticky_label);
        sticky_bar.set_halign(gtk::Align::Fill);
        sticky_bar.set_valign(gtk::Align::Start);
        sticky_bar.set_visible(false);
        sticky_bar.set_can_focus(false);
        // Some sticky headers represent a finished, oversized block. Store that
        // block id so clicking the header can jump back to its command start.
        let sticky_target_id: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));
''',
    r'''        let sticky_label = gtk::Label::new(None);
        sticky_label.set_halign(gtk::Align::Start);
        sticky_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        sticky_label.set_hexpand(true);
        sticky_label.add_css_class("sticky-running-label");

        let sticky_jump_bottom_btn = gtk::Button::with_label("\u{f103}");
        sticky_jump_bottom_btn.set_tooltip_text(Some("Jump to bottom of this block"));
        sticky_jump_bottom_btn.add_css_class("sticky-header-control");
        sticky_jump_bottom_btn.add_css_class("flat");
        sticky_jump_bottom_btn.set_focusable(false);
        sticky_jump_bottom_btn.set_visible(false);

        let sticky_minimize_btn = gtk::Button::with_label("\u{f077}");
        sticky_minimize_btn.set_tooltip_text(Some("Minimize sticky command header"));
        sticky_minimize_btn.add_css_class("sticky-header-control");
        sticky_minimize_btn.add_css_class("flat");
        sticky_minimize_btn.set_focusable(false);

        let sticky_bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        sticky_bar.add_css_class("sticky-running-header");
        sticky_bar.append(&sticky_label);
        sticky_bar.append(&sticky_jump_bottom_btn);
        sticky_bar.append(&sticky_minimize_btn);
        sticky_bar.set_halign(gtk::Align::Fill);
        sticky_bar.set_valign(gtk::Align::Start);
        sticky_bar.set_visible(false);
        sticky_bar.set_focusable(false);

        // Some sticky headers represent a finished, oversized block. Store that
        // block id so clicking the label can jump back to its command start.
        let sticky_target_id: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));
        let sticky_minimized: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        {
            let minimized = sticky_minimized.clone();
            let label = sticky_label.clone();
            let jump = sticky_jump_bottom_btn.clone();
            let bar = sticky_bar.clone();
            sticky_minimize_btn.connect_clicked(move |button| {
                let now_minimized = !minimized.get();
                minimized.set(now_minimized);
                label.set_visible(!now_minimized);
                jump.set_visible(false);
                if now_minimized {
                    bar.add_css_class("sticky-minimized");
                    button.set_label("\u{f078}");
                    button.set_tooltip_text(Some("Expand sticky command header"));
                } else {
                    bar.remove_css_class("sticky-minimized");
                    button.set_label("\u{f077}");
                    button.set_tooltip_text(Some("Minimize sticky command header"));
                }
            });
        }
''',
    "add sticky header controls",
)

text = replace_once(
    text,
    r'''        // A finished-block sticky header behaves like Warp's: click it to return
        // to the command at the top of the oversized block.
        {
            let target = sticky_target_id.clone();
            let finished = finished_blocks_rc.clone();
            let scroll = block_scroll.clone();
            let click = gtk::GestureClick::new();
            click.set_button(1);
            click.connect_released(move |_, n_press, _, _| {
                if n_press != 1 {
                    return;
                }
                let Some(id) = target.get() else {
                    return;
                };
                let finished = finished.borrow();
                let Some(block) = finished.iter().find(|block| block.id == id) else {
                    return;
                };
                let widget = block.widget().clone();
                let scroll = scroll.clone();
                glib::idle_add_local_once(move || {
                    if let Some(point) =
                        widget.compute_point(&scroll, &gtk::graphene::Point::new(0.0, 0.0))
                    {
                        let adj = scroll.vadjustment();
                        let max_value = (adj.upper() - adj.page_size()).max(adj.lower());
                        let target = adj.value() + point.y() as f64;
                        adj.set_value(target.clamp(adj.lower(), max_value));
                    }
                });
            });
            sticky_bar.add_controller(click);
        }
''',
    r'''        // A finished-block sticky label behaves like Warp's: click it to return
        // to the command at the top of the oversized block.
        {
            let target = sticky_target_id.clone();
            let finished = finished_blocks_rc.clone();
            let scroll = block_scroll.clone();
            let click = gtk::GestureClick::new();
            click.set_button(1);
            click.connect_released(move |_, n_press, _, _| {
                if n_press != 1 {
                    return;
                }
                let Some(id) = target.get() else {
                    return;
                };
                let finished = finished.borrow();
                let Some(block) = finished.iter().find(|block| block.id == id) else {
                    return;
                };
                block.scroll_to_edge(&scroll, false);
            });
            sticky_label.add_controller(click);
        }
        {
            let target = sticky_target_id.clone();
            let finished = finished_blocks_rc.clone();
            let scroll = block_scroll.clone();
            sticky_jump_bottom_btn.connect_clicked(move |_| {
                let Some(id) = target.get() else {
                    return;
                };
                let finished = finished.borrow();
                let Some(block) = finished.iter().find(|block| block.id == id) else {
                    return;
                };
                block.scroll_to_edge(&scroll, true);
            });
        }
''',
    "wire sticky header top and bottom actions",
)

text = replace_once(
    text,
    r'''                                let selected_for_menu = selected_block_id_rc.clone();
                                let bookmarks_for_menu = bookmarks_rc.clone();
                                let visible_for_menu = visible_indices_rc.clone();
''',
    r'''                                let selected_for_menu = selected_block_id_rc.clone();
                                let bookmarks_for_menu = bookmarks_rc.clone();
                                let block_scroll_for_menu = block_scroll_rc.clone();
                                let visible_for_menu = visible_indices_rc.clone();
''',
    "capture block scroll for context menu",
)

copy_block_anchor = r'''                                    {
                                        let item = make_item("Copy Block");
                                        let popover_c = popover.clone();
                                        let finished_for_copy = finished_menu_clone.clone();
                                        let vte_for_action = vte_for_copy.clone();
                                        item.connect_clicked(move |_| {
                                            popover_c.popdown();
                                            let prompt_text = finished_for_copy.prompt_text.clone();
                                            let cmd_text = finished_for_copy.cmd_text.clone();
                                            let output_text = strip_ansi(&finished_for_copy.full_output.borrow());
                                            let full_text = format!("{}\n{}\n{}", prompt_text, cmd_text, output_text);
                                            vte_for_action.clipboard().set_text(&full_text);
                                        });
                                        vbox.append(&item);
                                    }

'''
copy_block_new = copy_block_anchor + r'''                                    {
                                        let item = make_item("Scroll to Top of Block");
                                        let popover_c = popover.clone();
                                        let finished_for_scroll = finished_menu_clone.clone();
                                        let scroll_for_action = block_scroll_for_menu.clone();
                                        item.connect_clicked(move |_| {
                                            popover_c.popdown();
                                            finished_for_scroll
                                                .scroll_to_edge(&scroll_for_action, false);
                                        });
                                        vbox.append(&item);
                                    }
                                    if finished_menu_clone.long_output {
                                        let item = make_item("Jump to Bottom of Block");
                                        let popover_c = popover.clone();
                                        let finished_for_scroll = finished_menu_clone.clone();
                                        let scroll_for_action = block_scroll_for_menu.clone();
                                        item.connect_clicked(move |_| {
                                            popover_c.popdown();
                                            finished_for_scroll
                                                .scroll_to_edge(&scroll_for_action, true);
                                        });
                                        vbox.append(&item);
                                    }
                                    {
                                        let item = make_item("Toggle Output Filter");
                                        let popover_c = popover.clone();
                                        let finished_for_filter = finished_menu_clone.clone();
                                        item.connect_clicked(move |_| {
                                            popover_c.popdown();
                                            (finished_for_filter.toggle_filter)();
                                        });
                                        vbox.append(&item);
                                    }
                                    {
                                        let bookmarked =
                                            bookmarks_for_menu.borrow().contains(&block_id);
                                        let item = make_item(if bookmarked {
                                            "Remove Bookmark"
                                        } else {
                                            "Bookmark Block"
                                        });
                                        let popover_c = popover.clone();
                                        let finished_for_bookmark = finished_menu_clone.clone();
                                        let bookmarks_for_action = bookmarks_for_menu.clone();
                                        item.connect_clicked(move |_| {
                                            popover_c.popdown();
                                            let mut marks = bookmarks_for_action.borrow_mut();
                                            let now_bookmarked = if marks.remove(&block_id) {
                                                false
                                            } else {
                                                marks.insert(block_id);
                                                true
                                            };
                                            finished_for_bookmark
                                                .bookmark_star
                                                .set_visible(now_bookmarked);
                                            if now_bookmarked {
                                                finished_for_bookmark
                                                    .widget()
                                                    .add_css_class("block-bookmarked");
                                            } else {
                                                finished_for_bookmark
                                                    .widget()
                                                    .remove_css_class("block-bookmarked");
                                            }
                                        });
                                        vbox.append(&item);
                                    }
                                    vbox.append(&gtk::Separator::new(
                                        gtk::Orientation::Horizontal,
                                    ));

'''
text = replace_once(
    text,
    copy_block_anchor,
    copy_block_new,
    "add Warp context menu actions",
)

text = replace_once(
    text,
    r'''        // ── Sticky command header ────────────────────────────────────────
        // Running commands keep their existing status header while the user reads
        // history. Finished oversized blocks pin their command when the original
        // header has scrolled above the viewport but the block still spans it.
        {
            let sticky = sticky_bar.clone();
            let sticky_label = sticky_label.clone();
            let sticky_target = sticky_target_id.clone();
            let cmd_running = cmd_running.clone();
            let running_cmd = running_cmd.clone();
            let block_start_time = block_start_time.clone();
            let user_scrolled = user_scrolled_up.clone();
            let finished = finished_blocks_rc.clone();
            let scroll = block_scroll.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(120), move || {
                if sticky.parent().is_none() {
                    return glib::ControlFlow::Break;
                }

                if cmd_running.get() && user_scrolled.get() {
                    sticky_target.set(None);
                    let cmd = running_cmd.borrow();
                    let cmd_disp = cmd.trim();
                    let elapsed = block_start_time
                        .get()
                        .and_then(|st| SystemTime::now().duration_since(st).ok())
                        .map(|duration| duration.as_secs())
                        .unwrap_or(0);
                    let elapsed_str = if elapsed >= 60 {
                        format!("{}m{:02}s", elapsed / 60, elapsed % 60)
                    } else {
                        format!("{}s", elapsed)
                    };
                    let label = if cmd_disp.is_empty() {
                        format!("\u{25b6}  (running)    {}", elapsed_str)
                    } else {
                        format!("\u{25b6}  {}    {}", cmd_disp, elapsed_str)
                    };
                    sticky_label.set_text(&label);
                    sticky.set_visible(true);
                    return glib::ControlFlow::Continue;
                }

                let sticky_height = sticky.height().max(1) as f32;
                let candidate = finished.borrow().iter().find_map(|block| {
                    let header = block.header_row.compute_bounds(&scroll)?;
                    let card = block.widget().compute_bounds(&scroll)?;
                    let header_bottom = header.y() + header.height();
                    let card_bottom = card.y() + card.height();
                    if header_bottom <= 0.0 && card_bottom > sticky_height + 4.0 {
                        let command = block
                            .cmd_text
                            .lines()
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        Some((block.id, command))
                    } else {
                        None
                    }
                });

                if let Some((id, command)) = candidate {
                    sticky_target.set(Some(id));
                    let command = if command.is_empty() {
                        "(empty command)".to_string()
                    } else {
                        command
                    };
                    sticky_label.set_text(&format!("\u{276f}  {}", command));
                    sticky.set_visible(true);
                } else {
                    sticky_target.set(None);
                    sticky.set_visible(false);
                }
                glib::ControlFlow::Continue
            });
        }
''',
    r'''        // ── Sticky command header ────────────────────────────────────────
        // Running commands keep their existing status header while the user reads
        // history. Finished oversized blocks pin their command when the original
        // header has scrolled above the viewport but the block still spans it.
        {
            let sticky = sticky_bar.clone();
            let sticky_label = sticky_label.clone();
            let sticky_jump_bottom = sticky_jump_bottom_btn.clone();
            let sticky_target = sticky_target_id.clone();
            let sticky_minimized = sticky_minimized.clone();
            let cmd_running = cmd_running.clone();
            let running_cmd = running_cmd.clone();
            let block_start_time = block_start_time.clone();
            let user_scrolled = user_scrolled_up.clone();
            let finished = finished_blocks_rc.clone();
            let scroll = block_scroll.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(120), move || {
                if sticky.parent().is_none() {
                    return glib::ControlFlow::Break;
                }

                let minimized = sticky_minimized.get();
                if cmd_running.get() && user_scrolled.get() {
                    sticky_target.set(None);
                    sticky_jump_bottom.set_visible(false);
                    let cmd = running_cmd.borrow();
                    let cmd_disp = cmd.trim();
                    let elapsed = block_start_time
                        .get()
                        .and_then(|st| SystemTime::now().duration_since(st).ok())
                        .map(|duration| duration.as_secs())
                        .unwrap_or(0);
                    let elapsed_str = if elapsed >= 60 {
                        format!("{}m{:02}s", elapsed / 60, elapsed % 60)
                    } else {
                        format!("{}s", elapsed)
                    };
                    let label = if cmd_disp.is_empty() {
                        format!("\u{25b6}  (running)    {}", elapsed_str)
                    } else {
                        format!("\u{25b6}  {}    {}", cmd_disp, elapsed_str)
                    };
                    sticky_label.set_text(&label);
                    sticky_label.set_visible(!minimized);
                    sticky.set_visible(true);
                    return glib::ControlFlow::Continue;
                }

                let sticky_height = sticky.height().max(1) as f32;
                let candidate = finished.borrow().iter().find_map(|block| {
                    let header = block.header_row.compute_bounds(&scroll)?;
                    let card = block.widget().compute_bounds(&scroll)?;
                    let header_bottom = header.y() + header.height();
                    let card_bottom = card.y() + card.height();
                    if header_bottom <= 0.0 && card_bottom > sticky_height + 4.0 {
                        let command = block
                            .cmd_text
                            .lines()
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        Some((block.id, command, block.long_output))
                    } else {
                        None
                    }
                });

                if let Some((id, command, long_output)) = candidate {
                    sticky_target.set(Some(id));
                    let command = if command.is_empty() {
                        "(empty command)".to_string()
                    } else {
                        command
                    };
                    sticky_label.set_text(&format!("\u{276f}  {}", command));
                    sticky_label.set_visible(!minimized);
                    sticky_jump_bottom.set_visible(!minimized && long_output);
                    sticky.set_visible(true);
                } else {
                    sticky_target.set(None);
                    sticky_jump_bottom.set_visible(false);
                    sticky.set_visible(false);
                }
                glib::ControlFlow::Continue
            });
        }
''',
    "align sticky header controls and long block action",
)

path.write_text(text)


# ---------------------------------------------------------------------------
# src/block_view/css.rs
# ---------------------------------------------------------------------------
path = Path("src/block_view/css.rs")
text = path.read_text()

text = replace_once(
    text,
    r'''        .block-success {{
            border-left-color: {ok_stripe};
        }}
''',
    r'''        .block-finished.block-compact {{
            border-radius: 6px;
            min-height: 32px;
            box-shadow: none;
        }}
        .block-success {{
            border-left-color: {ok_stripe};
        }}
''',
    "add compact finished CSS",
)

text = replace_once(
    text,
    r'''        .block-prompt-chevron {{
            color: {accent};
''',
    r'''        .block-active.block-compact {{
            border-radius: 6px;
            margin: 1px 4px;
            padding: 0;
            box-shadow: none;
        }}
        .block-prompt-chevron {{
            color: {accent};
''',
    "add compact active CSS",
)

text = replace_once(
    text,
    r'''        .sticky-running-label {{
            color: {accent};
            font-family: "{font_family}";
            font-size: 0.92em;
            font-weight: bold;
        }}
''',
    r'''        .sticky-running-label {{
            color: {accent};
            font-family: "{font_family}";
            font-size: 0.92em;
            font-weight: bold;
        }}
        .sticky-header-control {{
            color: {dim_fg};
            min-width: 22px;
            min-height: 22px;
            padding: 0 4px;
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.82em;
        }}
        .sticky-header-control:hover {{
            color: {fg_hex};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.12);
        }}
        .sticky-running-header.sticky-minimized {{
            padding: 2px 8px;
            background-color: rgba({bg_r},{bg_g},{bg_b},0.92);
            box-shadow: 0 1px 4px rgba(0,0,0,0.24);
        }}
''',
    "style sticky header controls",
)

path.write_text(text)


# ---------------------------------------------------------------------------
# src/keybindings.rs
# ---------------------------------------------------------------------------
path = Path("src/keybindings.rs")
text = path.read_text()

text = replace_once(
    text,
    r'''        bind("Ctrl+Shift+B", Action::ToggleTabPlacement);
''',
    r'''        // Keep Warp's Ctrl+Shift+B available for block bookmarks.
        bind("Ctrl+Alt+B", Action::ToggleTabPlacement);
''',
    "free Warp bookmark shortcut",
)

text = replace_once(
    text,
    r'''        bind("Ctrl+Shift+Left", Action::FocusPaneLeft);
        bind("Ctrl+Shift+Right", Action::FocusPaneRight);
        bind("Ctrl+Shift+Up", Action::FocusPaneUp);
        bind("Ctrl+Shift+Down", Action::FocusPaneDown);
''',
    r'''        bind("Ctrl+Shift+Left", Action::FocusPaneLeft);
        bind("Ctrl+Shift+Right", Action::FocusPaneRight);
        // Warp reserves Ctrl+Shift+Up/Down for selected-block top/bottom.
        bind("Ctrl+Alt+Shift+Up", Action::FocusPaneUp);
        bind("Ctrl+Alt+Shift+Down", Action::FocusPaneDown);
''',
    "free Warp selected block edge shortcuts",
)

path.write_text(text)
