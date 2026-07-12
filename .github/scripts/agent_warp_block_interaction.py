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


path = Path("src/block_view/mod.rs")
text = path.read_text()

selection_anchor = '''fn select_finished_block(
    finished: &[FinishedBlock],
    selected_block_id: &Rc<Cell<Option<u64>>>,
    new_id: Option<u64>,
) {
    let prev = selected_block_id.get();
    if let Some(pid) = prev {
        if let Some(b) = finished.iter().find(|b| b.id == pid) {
            b.widget().remove_css_class("block-selected");
            b.action_box.set_visible(false);
        }
    }
    if let Some(nid) = new_id {
        if let Some(b) = finished.iter().find(|b| b.id == nid) {
            b.widget().add_css_class("block-selected");
            b.action_box.set_visible(true);
        }
    }
    selected_block_id.set(new_id);
}
'''
selection_replacement = selection_anchor + '''
/// Bring a selected block into the upper third of the viewport. `compute_point`
/// returns viewport-relative coordinates, so add the current adjustment before
/// calculating the new absolute scroll position.
fn scroll_finished_block_into_view(block: &FinishedBlock, scroll: &ScrolledWindow) {
    let widget = block.widget().clone();
    let scroll = scroll.clone();
    glib::idle_add_local_once(move || {
        if let Some(point) = widget.compute_point(&scroll, &gtk::graphene::Point::new(0.0, 0.0)) {
            let adj = scroll.vadjustment();
            let max_value = (adj.upper() - adj.page_size()).max(adj.lower());
            let target = adj.value() + point.y() as f64 - adj.page_size() / 3.0;
            adj.set_value(target.clamp(adj.lower(), max_value));
        }
    });
}

/// Move the Warp-style block selection by one item. Moving up with no current
/// selection starts at the newest finished block; moving down past the newest
/// block returns focus to the live prompt.
fn move_finished_block_selection(
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
'''
text = replace_once(text, selection_anchor, selection_replacement, "add block navigation helpers")

page_pattern = r'''            // Shift\+PageUp/PageDown: page the block history locally\..*?            if shift && !ctrl && !alt && matches!\(keyval, Key::Page_Up \| Key::Page_Down\) \{
                let adj = block_scroll_for_key\.vadjustment\(\);
                let step = \(adj\.page_size\(\) \* 0\.9\)\.max\(1\.0\);
                let delta = if keyval == Key::Page_Up \{ -step \} else \{ step \};
                let max_val = \(adj\.upper\(\) - adj\.page_size\(\)\)\.max\(adj\.lower\(\)\);
                adj\.set_value\(\(adj\.value\(\) \+ delta\)\.clamp\(adj\.lower\(\), max_val\)\);
                return glib::Propagation::Stop;
            \}

            // Ctrl\+Shift\+\[/\]: move the finished-block selection\.'''
page_replacement = '''            // Warp pages block history with PageUp/PageDown. While a command or
            // fullscreen/raw terminal owns the viewport, leave these keys to VTE.
            let history_navigation = !matches!(
                bstate_for_key.get(),
                BlockState::CollectingOutput | BlockState::AltScreen | BlockState::RawFallback
            );
            if !ctrl
                && !alt
                && history_navigation
                && matches!(keyval, Key::Page_Up | Key::Page_Down)
            {
                let adj = block_scroll_for_key.vadjustment();
                let step = (adj.page_size() * 0.9).max(1.0);
                let delta = if keyval == Key::Page_Up { -step } else { step };
                let max_val = (adj.upper() - adj.page_size()).max(adj.lower());
                adj.set_value((adj.value() + delta).clamp(adj.lower(), max_val));
                return glib::Propagation::Stop;
            }

            // Once a block is selected, plain Up/Down walks blocks instead of
            // editing shell history. Without a selection the keys still reach VTE.
            if !ctrl
                && !shift
                && !alt
                && selected_block_id_for_key.get().is_some()
                && matches!(keyval, Key::Up | Key::Down)
            {
                let finished = finished_blocks_for_key.borrow();
                let direction = if keyval == Key::Up { -1 } else { 1 };
                move_finished_block_selection(
                    &finished,
                    &selected_block_id_for_key,
                    &block_scroll_for_key,
                    direction,
                );
                return glib::Propagation::Stop;
            }

            // Ctrl+Shift+[/]: move the finished-block selection.'''
text = regex_once(text, page_pattern, page_replacement, "align page and arrow navigation")

bracket_pattern = r'''            // Ctrl\+Shift\+\[/\]: move the finished-block selection\..*?            if ctrl && shift && !alt && matches!\(keyval, Key::bracketleft \| Key::bracketright\) \{.*?                return glib::Propagation::Stop;
            \}

            // Enter while a block is selected'''
bracket_replacement = '''            // Keep the existing bracket aliases for window-manager/keybinding
            // conflicts, but route them through the same selection semantics.
            if ctrl && shift && !alt && matches!(keyval, Key::bracketleft | Key::bracketright) {
                let finished = finished_blocks_for_key.borrow();
                let direction = if keyval == Key::bracketleft { -1 } else { 1 };
                move_finished_block_selection(
                    &finished,
                    &selected_block_id_for_key,
                    &block_scroll_for_key,
                    direction,
                );
                return glib::Propagation::Stop;
            }

            // Enter while a block is selected'''
text = regex_once(text, bracket_pattern, bracket_replacement, "reuse block selection helper")

filter_hotkey_anchor = '''            // Ctrl+B: toggle a bookmark on the selected block (Warp's
'''
filter_hotkey = '''            // Linux Warp toggles the output filter editor with Alt+Shift+F. Target
            // the selected block, or the latest block when selection is empty.
            if alt
                && shift
                && !ctrl
                && matches!(keyval, Key::f | Key::F)
                && bstate_for_key.get() != BlockState::AltScreen
            {
                let finished = finished_blocks_for_key.borrow();
                let target = selected_block_id_for_key
                    .get()
                    .and_then(|id| finished.iter().find(|block| block.id == id))
                    .or_else(|| finished.last());
                if let Some(block) = target {
                    (block.toggle_filter)();
                    return glib::Propagation::Stop;
                }
            }

'''
text = replace_once(text, filter_hotkey_anchor, filter_hotkey + filter_hotkey_anchor, "add Warp filter hotkey")

scroll_old = '''    pub fn scroll_lines(&self, lines: i32) {
        let adj = self.block_scroll.vadjustment();
        let cell_h = self.active_vte.char_height() as f64;
        let step = if cell_h > 0.0 {
            cell_h
        } else {
            adj.step_increment()
        };
        let max_val = (adj.upper() - adj.page_size()).max(adj.lower());
        let value = (adj.value() + step * lines as f64).clamp(adj.lower(), max_val);
        adj.set_value(value);
    }
'''
scroll_new = '''    pub fn scroll_lines(&self, lines: i32) {
        // Ctrl+Up enters Warp-style block selection at the newest block; once a
        // block is selected Ctrl+Up/Down continue moving the selection. Ctrl+Down
        // with no selection retains the ordinary small scroll behavior.
        {
            let finished = self.finished_blocks.borrow();
            if (lines < 0 || self.selected_block_id.get().is_some())
                && move_finished_block_selection(
                    &finished,
                    &self.selected_block_id,
                    &self.block_scroll,
                    lines.signum(),
                )
            {
                return;
            }
        }

        let adj = self.block_scroll.vadjustment();
        let cell_h = self.active_vte.char_height() as f64;
        let step = if cell_h > 0.0 {
            cell_h
        } else {
            adj.step_increment()
        };
        let max_val = (adj.upper() - adj.page_size()).max(adj.lower());
        let value = (adj.value() + step * lines as f64).clamp(adj.lower(), max_val);
        adj.set_value(value);
    }
'''
text = replace_once(text, scroll_old, scroll_new, "route Ctrl arrows through block selection")

sticky_anchor = '''        sticky_bar.set_visible(false);
        sticky_bar.set_can_focus(false);
'''
sticky_replacement = '''        sticky_bar.set_visible(false);
        sticky_bar.set_can_focus(false);
        // Some sticky headers represent a finished, oversized block. Store that
        // block id so clicking the header can jump back to its command start.
        let sticky_target_id: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));
'''
text = replace_once(text, sticky_anchor, sticky_replacement, "add sticky block target")

finished_anchor = '''        let finished_blocks_rc: Rc<RefCell<Vec<FinishedBlock>>> = Rc::new(RefCell::new(Vec::new()));

        let pending_exit_code: Rc<Cell<i32>> = Rc::new(Cell::new(0));
'''
finished_replacement = '''        let finished_blocks_rc: Rc<RefCell<Vec<FinishedBlock>>> = Rc::new(RefCell::new(Vec::new()));

        // A finished-block sticky header behaves like Warp's: click it to return
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

        let pending_exit_code: Rc<Cell<i32>> = Rc::new(Cell::new(0));
'''
text = replace_once(text, finished_anchor, finished_replacement, "make sticky header clickable")

sticky_pattern = r'''        // ── Sticky running-command header: poll-driven refresh ────────────.*?        \{
            let sticky = sticky_bar\.clone\(\);.*?            glib::timeout_add_local\(std::time::Duration::from_millis\(500\), move \|\| \{.*?            \}\);
        \}

        // ── VTE is used as a display-only widget'''
sticky_new = '''        // ── Sticky command header ────────────────────────────────────────
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
                        format!("\\u{25b6}  (running)    {}", elapsed_str)
                    } else {
                        format!("\\u{25b6}  {}    {}", cmd_disp, elapsed_str)
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
                    sticky_label.set_text(&format!("\\u{276f}  {}", command));
                    sticky.set_visible(true);
                } else {
                    sticky_target.set(None);
                    sticky.set_visible(false);
                }
                glib::ControlFlow::Continue
            });
        }

        // ── VTE is used as a display-only widget'''
text = regex_once(text, sticky_pattern, sticky_new, "add finished block sticky header")

path.write_text(text)
print("Warp block interaction patch applied")
