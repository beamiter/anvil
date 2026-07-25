use gtk::gdk::Key;
use gtk::gdk::ModifierType;
use gtk::glib::translate::IntoGlib;
use relm4::gtk;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Action {
    NewTab,
    CloseTab,
    ClosePaneOrTab,
    Copy,
    Paste,
    FontIncrease,
    FontDecrease,
    FontReset,
    OpacityIncrease,
    OpacityDecrease,
    ToggleSearch,
    ToggleCommandPalette,
    OpenPalette,
    OpenHistoryPalette,
    ToggleSettings,
    ReloadConfig,
    OpenWelcome,
    ToggleSidebar,
    SplitHorizontal,
    SplitVertical,
    PrevTab,
    NextTab,
    ScrollUp,
    ScrollDown,
    CyclePaneFocusForward,
    CyclePaneFocusBackward,
    QuickSwitchTab(u8),
    ConnectRemote(u8),
    ShowRemotePicker,
    ResizePaneLeft,
    ResizePaneRight,
    ResizePaneUp,
    ResizePaneDown,
    TogglePaneZoom,
    MovePaneToNewTab,
    FocusPaneLeft,
    FocusPaneRight,
    FocusPaneUp,
    FocusPaneDown,
    FilterTabs,
    CloseSelectedTabs,
    MoveTabLeft,
    MoveTabRight,
    DuplicateTab,
    ToggleTabMarked,
    ToggleTabPinned,
    ToggleTabPlacement,
    FilterFailedBlocks,
    FilterSlowBlocks,
    FilterPinnedBlocks,
    ClearBlockFilter,
    /// Select every finished block in the active block-mode pane. Warp exposes
    /// this as a first-class block action rather than ordinary terminal text
    /// selection.
    SelectAllBlocks,
    /// Remove every finished block from the active block-mode pane.
    ClearBlocks,
    /// Restore the blocks removed by the most recent Clear Blocks.
    UndoClearBlocks,
    /// Put the commands from the current block selection back into the live
    /// input editor, preserving terminal order for multi-selection.
    ReinputSelectedCommands,
    /// Jump to the previous / next pinned ("bookmarked") block. Warp parity:
    /// gives users persistent navigation targets through long sessions.
    JumpToPrevPinned,
    JumpToNextPinned,
    /// Jump to the previous / next failed (non-zero exit) block, mirroring the
    /// pinned-block navigation for error triage in long sessions.
    JumpToPrevFailed,
    JumpToNextFailed,
    /// Write every completed block to a timestamped Markdown / JSON file under
    /// the jterm1 data directory.
    ExportSessionMarkdown,
    ExportSessionJson,
    ToggleDebugDashboard,
    /// Open the session-level AI panel for free-form questions about the
    /// current shell context (Ctrl+Alt+Shift+A by default; Ctrl+Shift+A is the
    /// Warp-compatible Select All Blocks action).
    OpenAiPanel,
    /// Send the selected finished Block command/output to the AI panel. The
    /// model response is displayed only; it is never inserted or executed.
    AskAiAboutSelectedBlock,
    /// Open the palette focused on parameterised command templates
    /// ("workflows", `:` prefix). Ctrl+Shift+M by default.
    OpenWorkflows,
    /// Open the multi-turn agent panel (Warp-style). Ctrl+Alt+G by default.
    OpenAgent,
    /// Search command and output lines across all completed blocks.
    CrossBlockSearch,
}

impl Action {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Action::NewTab => "New tab",
            Action::CloseTab => "Close tab",
            Action::ClosePaneOrTab => "Close focused pane or tab",
            Action::Copy => "Copy",
            Action::Paste => "Paste",
            Action::FontIncrease => "Font size increase",
            Action::FontDecrease => "Font size decrease",
            Action::FontReset => "Font size reset",
            Action::OpacityIncrease => "Opacity increase",
            Action::OpacityDecrease => "Opacity decrease",
            Action::ToggleSearch => "Toggle search",
            Action::ToggleCommandPalette => "Command palette (actions)",
            Action::OpenPalette => "Palette: search everything",
            Action::OpenHistoryPalette => "Palette: search history",
            Action::ToggleSettings => "Toggle settings panel",
            Action::ReloadConfig => "Reload configuration",
            Action::OpenWelcome => "Open welcome & quick start",
            Action::ToggleSidebar => "Toggle sidebar",
            Action::SplitHorizontal => "Split left/right",
            Action::SplitVertical => "Split top/bottom",
            Action::PrevTab => "Previous tab",
            Action::NextTab => "Next tab",
            Action::ScrollUp => "Scroll up",
            Action::ScrollDown => "Scroll down",
            Action::CyclePaneFocusForward => "Cycle pane focus forward",
            Action::CyclePaneFocusBackward => "Cycle pane focus backward",
            Action::QuickSwitchTab(n) => match n {
                0 => "Switch to tab 1",
                1 => "Switch to tab 2",
                2 => "Switch to tab 3",
                3 => "Switch to tab 4",
                4 => "Switch to tab 5",
                5 => "Switch to tab 6",
                6 => "Switch to tab 7",
                7 => "Switch to tab 8",
                8 => "Switch to tab 9",
                _ => "Switch to last tab",
            },
            Action::ConnectRemote(_) => "Connect to remote host",
            Action::ShowRemotePicker => "Connect to remote host…",
            Action::ResizePaneLeft => "Resize pane left",
            Action::ResizePaneRight => "Resize pane right",
            Action::ResizePaneUp => "Resize pane up",
            Action::ResizePaneDown => "Resize pane down",
            Action::TogglePaneZoom => "Toggle pane zoom",
            Action::MovePaneToNewTab => "Move pane to new tab",
            Action::FocusPaneLeft => "Focus pane left",
            Action::FocusPaneRight => "Focus pane right",
            Action::FocusPaneUp => "Focus pane up",
            Action::FocusPaneDown => "Focus pane down",
            Action::FilterTabs => "Filter tabs",
            Action::CloseSelectedTabs => "Close selected tabs",
            Action::MoveTabLeft => "Move tab left",
            Action::MoveTabRight => "Move tab right",
            Action::DuplicateTab => "Duplicate tab",
            Action::ToggleTabMarked => "Toggle tab marked",
            Action::ToggleTabPinned => "Toggle tab pinned",
            Action::ToggleTabPlacement => "Toggle tab placement (sidebar/top)",
            // These actions navigate to the first matching block; they do not
            // hide unrelated blocks. Name them accurately in the palette so a
            // shortcut never appears to have silently failed.
            Action::FilterFailedBlocks => "Jump to first failed block",
            Action::FilterSlowBlocks => "Jump to first slow block",
            Action::FilterPinnedBlocks => "Jump to first pinned block",
            Action::JumpToPrevPinned => "Jump to previous pinned block",
            Action::JumpToNextPinned => "Jump to next pinned block",
            Action::JumpToPrevFailed => "Jump to previous failed block",
            Action::JumpToNextFailed => "Jump to next failed block",
            Action::ExportSessionMarkdown => "Export session as Markdown file",
            Action::ExportSessionJson => "Export session as JSON file",
            Action::ClearBlockFilter => "Jump to oldest block",
            Action::SelectAllBlocks => "Select all blocks",
            Action::ClearBlocks => "Clear blocks",
            Action::UndoClearBlocks => "Undo clear blocks",
            Action::ReinputSelectedCommands => "Reinput selected commands",
            Action::ToggleDebugDashboard => "Toggle debug dashboard",
            Action::OpenAiPanel => "Open AI panel",
            Action::AskAiAboutSelectedBlock => "Ask AI about selected block",
            Action::OpenWorkflows => "Open workflows",
            Action::OpenAgent => "Open AI agent",
            Action::CrossBlockSearch => "Search across blocks (ripgrep)",
        }
    }

    pub(crate) fn config_key(&self) -> Option<&'static str> {
        match self {
            Action::NewTab => Some("new_tab"),
            Action::CloseTab => Some("close_tab"),
            Action::ClosePaneOrTab => Some("close_pane_or_tab"),
            Action::Copy => Some("copy"),
            Action::Paste => Some("paste"),
            Action::FontIncrease => Some("font_increase"),
            Action::FontDecrease => Some("font_decrease"),
            Action::FontReset => Some("font_reset"),
            Action::OpacityIncrease => Some("opacity_increase"),
            Action::OpacityDecrease => Some("opacity_decrease"),
            Action::ToggleSearch => Some("toggle_search"),
            Action::ToggleCommandPalette => Some("toggle_command_palette"),
            Action::OpenPalette => Some("open_palette"),
            Action::OpenHistoryPalette => Some("open_history_palette"),
            Action::ToggleSettings => Some("toggle_settings"),
            Action::ReloadConfig => Some("reload_config"),
            Action::OpenWelcome => None,
            Action::ToggleSidebar => Some("toggle_sidebar"),
            Action::SplitHorizontal => Some("split_horizontal"),
            Action::SplitVertical => Some("split_vertical"),
            Action::PrevTab => Some("prev_tab"),
            Action::NextTab => Some("next_tab"),
            Action::ScrollUp => Some("scroll_up"),
            Action::ScrollDown => Some("scroll_down"),
            Action::CyclePaneFocusForward => Some("cycle_pane_focus_forward"),
            Action::CyclePaneFocusBackward => Some("cycle_pane_focus_backward"),
            Action::QuickSwitchTab(_) => None,
            Action::ConnectRemote(_) => None,
            Action::ShowRemotePicker => Some("show_remote_picker"),
            Action::ResizePaneLeft => Some("resize_pane_left"),
            Action::ResizePaneRight => Some("resize_pane_right"),
            Action::ResizePaneUp => Some("resize_pane_up"),
            Action::ResizePaneDown => Some("resize_pane_down"),
            Action::TogglePaneZoom => Some("toggle_pane_zoom"),
            Action::MovePaneToNewTab => Some("move_pane_to_new_tab"),
            Action::FocusPaneLeft => Some("focus_pane_left"),
            Action::FocusPaneRight => Some("focus_pane_right"),
            Action::FocusPaneUp => Some("focus_pane_up"),
            Action::FocusPaneDown => Some("focus_pane_down"),
            Action::FilterTabs => Some("filter_tabs"),
            Action::CloseSelectedTabs => Some("close_selected_tabs"),
            Action::MoveTabLeft => Some("move_tab_left"),
            Action::MoveTabRight => Some("move_tab_right"),
            Action::DuplicateTab => Some("duplicate_tab"),
            Action::ToggleTabMarked => Some("toggle_tab_marked"),
            Action::ToggleTabPinned => Some("toggle_tab_pinned"),
            Action::ToggleTabPlacement => Some("toggle_tab_placement"),
            Action::FilterFailedBlocks => Some("filter_failed_blocks"),
            Action::FilterSlowBlocks => Some("filter_slow_blocks"),
            Action::FilterPinnedBlocks => Some("filter_pinned_blocks"),
            Action::JumpToPrevPinned => Some("jump_to_prev_pinned"),
            Action::JumpToNextPinned => Some("jump_to_next_pinned"),
            Action::JumpToPrevFailed => Some("jump_to_prev_failed"),
            Action::JumpToNextFailed => Some("jump_to_next_failed"),
            Action::ExportSessionMarkdown => Some("export_session_markdown"),
            Action::ExportSessionJson => Some("export_session_json"),
            Action::ClearBlockFilter => Some("clear_block_filter"),
            Action::SelectAllBlocks => Some("select_all_blocks"),
            Action::ClearBlocks => Some("clear_blocks"),
            Action::UndoClearBlocks => Some("undo_clear_blocks"),
            Action::ReinputSelectedCommands => Some("reinput_selected_commands"),
            Action::ToggleDebugDashboard => Some("toggle_debug_dashboard"),
            Action::OpenAiPanel => Some("open_ai_panel"),
            Action::AskAiAboutSelectedBlock => Some("ask_ai_about_selected_block"),
            Action::OpenWorkflows => Some("open_workflows"),
            Action::OpenAgent => Some("open_agent"),
            Action::CrossBlockSearch => Some("cross_block_search"),
        }
    }

    pub(crate) fn all_actions() -> Vec<Action> {
        vec![
            Action::NewTab,
            Action::CloseTab,
            Action::ClosePaneOrTab,
            Action::Copy,
            Action::Paste,
            Action::FontIncrease,
            Action::FontDecrease,
            Action::FontReset,
            Action::OpacityIncrease,
            Action::OpacityDecrease,
            Action::ToggleSearch,
            Action::ToggleCommandPalette,
            Action::OpenPalette,
            Action::OpenHistoryPalette,
            Action::ToggleSettings,
            Action::ReloadConfig,
            Action::OpenWelcome,
            Action::ToggleSidebar,
            Action::SplitHorizontal,
            Action::SplitVertical,
            Action::PrevTab,
            Action::NextTab,
            Action::ScrollUp,
            Action::ScrollDown,
            Action::CyclePaneFocusForward,
            Action::CyclePaneFocusBackward,
            Action::ShowRemotePicker,
            Action::ResizePaneLeft,
            Action::ResizePaneRight,
            Action::ResizePaneUp,
            Action::ResizePaneDown,
            Action::TogglePaneZoom,
            Action::MovePaneToNewTab,
            Action::FocusPaneLeft,
            Action::FocusPaneRight,
            Action::FocusPaneUp,
            Action::FocusPaneDown,
            Action::FilterTabs,
            Action::CloseSelectedTabs,
            Action::MoveTabLeft,
            Action::MoveTabRight,
            Action::DuplicateTab,
            Action::ToggleTabMarked,
            Action::ToggleTabPinned,
            Action::ToggleTabPlacement,
            Action::FilterFailedBlocks,
            Action::FilterSlowBlocks,
            Action::FilterPinnedBlocks,
            Action::JumpToPrevPinned,
            Action::JumpToNextPinned,
            Action::JumpToPrevFailed,
            Action::JumpToNextFailed,
            Action::ExportSessionMarkdown,
            Action::ExportSessionJson,
            Action::ClearBlockFilter,
            Action::SelectAllBlocks,
            Action::ClearBlocks,
            Action::UndoClearBlocks,
            Action::ReinputSelectedCommands,
            Action::ToggleDebugDashboard,
            Action::OpenAiPanel,
            Action::AskAiAboutSelectedBlock,
            Action::OpenWorkflows,
            Action::OpenAgent,
            Action::CrossBlockSearch,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct KeyCombo {
    pub(crate) modifiers: ModifierType,
    pub(crate) key: Key,
}

impl PartialEq for KeyCombo {
    fn eq(&self, other: &Self) -> bool {
        self.modifiers == other.modifiers && self.key == other.key
    }
}

impl Eq for KeyCombo {}

impl Hash for KeyCombo {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.modifiers.bits().hash(state);
        self.key.into_glib().hash(state);
    }
}

pub(crate) fn normalize_key(key: Key) -> Key {
    // ISO_Left_Tab is what GTK sends for Shift+Tab - normalize to Tab
    if key == Key::ISO_Left_Tab {
        return Key::Tab;
    }
    key.to_lower()
}

pub(crate) fn parse_key_combo(s: &str) -> Result<KeyCombo, String> {
    let mut modifiers = ModifierType::empty();
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("Empty key combo".to_string());
    }
    let parts: Vec<&str> = trimmed.split('+').map(|p| p.trim()).collect();

    // The last part is the key, but "+" itself is special:
    // "Ctrl+Shift++" means Ctrl+Shift and key is "+"
    let (mod_parts, key_str) = if trimmed.ends_with("++") && parts.len() >= 3 {
        (&parts[..parts.len() - 2], "+")
    } else if parts.last() == Some(&"") && parts.len() >= 2 {
        // "Ctrl++" case
        (&parts[..parts.len() - 2], "+")
    } else {
        (&parts[..parts.len() - 1], *parts.last().unwrap())
    };

    for part in mod_parts {
        match *part {
            _ if part.eq_ignore_ascii_case("Ctrl") => modifiers |= ModifierType::CONTROL_MASK,
            _ if part.eq_ignore_ascii_case("Shift") => modifiers |= ModifierType::SHIFT_MASK,
            _ if part.eq_ignore_ascii_case("Alt") => modifiers |= ModifierType::ALT_MASK,
            other => return Err(format!("Unknown modifier: {other}")),
        }
    }

    let key = match key_str {
        "+" | "plus" => Key::plus,
        "=" | "equal" => Key::equal,
        "-" | "minus" => Key::minus,
        k if k.eq_ignore_ascii_case("PageUp") => Key::Page_Up,
        k if k.eq_ignore_ascii_case("PageDown") => Key::Page_Down,
        k if k.eq_ignore_ascii_case("Tab") => Key::Tab,
        k if k.eq_ignore_ascii_case("Escape") || k.eq_ignore_ascii_case("Esc") => Key::Escape,
        k if k.eq_ignore_ascii_case("Return") || k.eq_ignore_ascii_case("Enter") => Key::Return,
        k if k.eq_ignore_ascii_case("Up") => Key::Up,
        k if k.eq_ignore_ascii_case("Down") => Key::Down,
        k if k.eq_ignore_ascii_case("Left") => Key::Left,
        k if k.eq_ignore_ascii_case("Right") => Key::Right,
        k if k.eq_ignore_ascii_case("!") || k.eq_ignore_ascii_case("exclam") => Key::exclam,
        k if k.eq_ignore_ascii_case("Space") => Key::space,
        k if k.eq_ignore_ascii_case("Backspace") => Key::BackSpace,
        k if k.eq_ignore_ascii_case("Delete") => Key::Delete,
        k if k.eq_ignore_ascii_case("Home") => Key::Home,
        k if k.eq_ignore_ascii_case("End") => Key::End,
        k if k.eq_ignore_ascii_case("Insert") => Key::Insert,
        s if s.len() == 1 => {
            let c = s.chars().next().unwrap();
            if c.is_ascii_digit() {
                match c {
                    '0' => Key::_0,
                    '1' => Key::_1,
                    '2' => Key::_2,
                    '3' => Key::_3,
                    '4' => Key::_4,
                    '5' => Key::_5,
                    '6' => Key::_6,
                    '7' => Key::_7,
                    '8' => Key::_8,
                    '9' => Key::_9,
                    _ => unreachable!(),
                }
            } else if c.is_ascii_alphabetic() {
                Key::from_name(c.to_lowercase().to_string())
                    .ok_or_else(|| format!("Unknown key: {s}"))?
            } else {
                return Err(format!("Unknown key: {s}"));
            }
        }
        s => Key::from_name(s).ok_or_else(|| format!("Unknown key: {s}"))?,
    };

    Ok(KeyCombo {
        modifiers,
        key: normalize_key(key),
    })
}

pub(crate) fn key_combo_to_string(combo: &KeyCombo) -> String {
    let mut parts = Vec::new();
    if combo.modifiers.contains(ModifierType::CONTROL_MASK) {
        parts.push("Ctrl");
    }
    if combo.modifiers.contains(ModifierType::SHIFT_MASK) {
        parts.push("Shift");
    }
    if combo.modifiers.contains(ModifierType::ALT_MASK) {
        parts.push("Alt");
    }

    let key_name = match combo.key {
        Key::plus => "+".to_string(),
        Key::equal => "=".to_string(),
        Key::minus => "-".to_string(),
        Key::Page_Up => "PageUp".to_string(),
        Key::Page_Down => "PageDown".to_string(),
        Key::Tab | Key::ISO_Left_Tab => "Tab".to_string(),
        Key::Escape => "Escape".to_string(),
        Key::Return => "Enter".to_string(),
        Key::Up => "Up".to_string(),
        Key::Down => "Down".to_string(),
        Key::Left => "Left".to_string(),
        Key::Right => "Right".to_string(),
        Key::exclam => "!".to_string(),
        Key::space => "Space".to_string(),
        Key::BackSpace => "Backspace".to_string(),
        Key::Delete => "Delete".to_string(),
        Key::Home => "Home".to_string(),
        Key::End => "End".to_string(),
        k => k
            .name()
            .map(|n| {
                let s = n.to_string();
                if s.len() == 1 {
                    s.to_uppercase()
                } else {
                    s
                }
            })
            .unwrap_or_else(|| "?".to_string()),
    };

    let mut result = parts.join("+");
    if !result.is_empty() {
        result.push('+');
    }
    result.push_str(&key_name);
    result
}

#[derive(Clone)]
pub(crate) struct KeybindingMap {
    pub(crate) bindings: HashMap<KeyCombo, Action>,
}

impl KeybindingMap {
    pub(crate) fn from_defaults() -> Self {
        let mut bindings = HashMap::new();

        let mut bind = |s: &str, action: Action| {
            if let Ok(combo) = parse_key_combo(s) {
                bindings.insert(combo, action);
            }
        };

        // Existing keybindings
        bind("Ctrl+Shift+T", Action::NewTab);
        bind("Ctrl+Shift+W", Action::ClosePaneOrTab);
        bind("Ctrl+Shift+C", Action::Copy);
        bind("Ctrl+Shift+V", Action::Paste);
        bind("Ctrl+=", Action::FontIncrease);
        bind("Ctrl+minus", Action::FontDecrease);
        bind("Ctrl+0", Action::FontReset);
        bind("Ctrl+Alt+=", Action::OpacityIncrease);
        bind("Ctrl+Alt+minus", Action::OpacityDecrease);
        bind("Ctrl+Shift+F", Action::ToggleSearch);
        bind("Ctrl+Shift+P", Action::ToggleCommandPalette);
        // Preserve Ctrl+R and Ctrl+P for shell/readline history navigation.
        bind("Ctrl+Shift+H", Action::OpenHistoryPalette);
        bind("Ctrl+Shift+O", Action::ToggleSettings);
        bind("Ctrl+Shift+R", Action::ReloadConfig);
        bind("Ctrl+backslash", Action::ToggleSidebar);
        bind("Ctrl+Shift+L", Action::FilterTabs);
        bind("Ctrl+Shift+X", Action::FilterFailedBlocks);
        bind("Ctrl+Shift+N", Action::ClearBlockFilter);
        bind("Ctrl+Shift+A", Action::SelectAllBlocks);
        bind("Ctrl+Shift+K", Action::ClearBlocks);
        bind("Ctrl+Shift+I", Action::ReinputSelectedCommands);
        bind("Ctrl+Shift+E", Action::SplitHorizontal);
        bind("Ctrl+Shift+D", Action::SplitVertical);
        bind("Ctrl+Shift+Tab", Action::PrevTab);
        bind("Ctrl+Tab", Action::NextTab);
        bind("Ctrl+Up", Action::ScrollUp);
        bind("Ctrl+Down", Action::ScrollDown);
        bind("Ctrl+PageUp", Action::PrevTab);
        bind("Ctrl+PageDown", Action::NextTab);
        for digit in 1..=8u8 {
            bind(&format!("Ctrl+{digit}"), Action::QuickSwitchTab(digit - 1));
        }
        bind("Ctrl+9", Action::QuickSwitchTab(9));
        bind("Ctrl+Shift+S", Action::ShowRemotePicker);

        bind("Ctrl+Alt+Shift+Left", Action::ResizePaneLeft);
        bind("Ctrl+Alt+Shift+Right", Action::ResizePaneRight);
        bind("Ctrl+Alt+Shift+Up", Action::ResizePaneUp);
        bind("Ctrl+Alt+Shift+Down", Action::ResizePaneDown);
        // Keep Warp's Ctrl+Shift+B available for block bookmarks.
        bind("Ctrl+Alt+B", Action::ToggleTabPlacement);
        bind("Ctrl+Shift+Z", Action::TogglePaneZoom);
        bind("Ctrl+Shift+!", Action::MovePaneToNewTab);
        bind("F12", Action::ToggleDebugDashboard);
        bind("Ctrl+Alt+Left", Action::FocusPaneLeft);
        bind("Ctrl+Alt+Right", Action::FocusPaneRight);
        bind("Ctrl+Alt+Up", Action::FocusPaneUp);
        bind("Ctrl+Alt+Down", Action::FocusPaneDown);
        bind("Ctrl+Alt+Shift+A", Action::OpenAiPanel);
        bind("Ctrl+Shift+Q", Action::AskAiAboutSelectedBlock);
        bind("Ctrl+Shift+M", Action::OpenWorkflows);
        bind("Ctrl+Alt+G", Action::OpenAgent);
        bind("Ctrl+Shift+G", Action::CrossBlockSearch);

        KeybindingMap { bindings }
    }

    pub(crate) fn apply_user_overrides(&mut self, table: &toml::Table) {
        // Build reverse map: config_key -> Action
        let mut key_to_action: HashMap<&str, Action> = HashMap::new();
        for action in Action::all_actions() {
            if let Some(key) = action.config_key() {
                key_to_action.insert(key, action);
            }
        }

        for (config_key, value) in table {
            let Some(&action) = key_to_action.get(config_key.as_str()) else {
                log::warn!("Unknown keybinding action: {config_key}");
                continue;
            };
            if value.as_bool() == Some(false) {
                self.bindings.retain(|_, bound| *bound != action);
                continue;
            }
            let Some(key_str) = value.as_str() else {
                log::warn!("Keybinding value for {config_key} must be a chord string or false");
                continue;
            };
            if key_str.trim().is_empty()
                || key_str.eq_ignore_ascii_case("none")
                || key_str.eq_ignore_ascii_case("disabled")
            {
                self.bindings.retain(|_, bound| *bound != action);
                continue;
            }
            let combo = match parse_key_combo(key_str) {
                Ok(combo) => combo,
                Err(e) => {
                    // Keep the previous/default binding. A typo in config must
                    // not make an action unreachable.
                    log::warn!("Invalid keybinding '{key_str}' for {config_key}: {e}");
                    continue;
                }
            };
            if let Some(existing) = self.bindings.get(&combo).copied() {
                if existing != action {
                    // Reject ambiguous overrides instead of silently stealing
                    // another action's key and leaving it unbound.
                    log::warn!(
                        "Keybinding '{key_str}' for {config_key} conflicts with '{}'",
                        existing.name()
                    );
                    continue;
                }
            }

            // Only mutate after parsing and conflict checks have succeeded.
            self.bindings.retain(|_, a| *a != action);
            self.bindings.insert(combo, action);
        }
    }

    pub(crate) fn lookup(&self, combo: &KeyCombo) -> Option<Action> {
        self.bindings.get(combo).copied()
    }

    pub(crate) fn binding_display(&self, action: &Action) -> String {
        let combos: Vec<_> = self
            .bindings
            .iter()
            .filter(|(_, a)| *a == action)
            .map(|(k, _)| key_combo_to_string(k))
            .collect();
        combos.join(", ")
    }

    pub(crate) fn all_bound_actions(&self) -> Vec<(Action, String)> {
        let mut result = Vec::new();
        for action in Action::all_actions() {
            let display = self.binding_display(&action);
            result.push((action, display));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_key_combo_trims_whitespace() {
        let combo = parse_key_combo(" Ctrl + Shift + P ").expect("valid");
        assert!(combo.modifiers.contains(ModifierType::CONTROL_MASK));
        assert!(combo.modifiers.contains(ModifierType::SHIFT_MASK));
        assert_eq!(combo.key, Key::p);
    }

    #[test]
    fn parse_key_combo_empty_string_is_error() {
        let err = parse_key_combo("   ").expect_err("empty input should fail");
        assert_eq!(err, "Empty key combo");
    }

    #[test]
    fn parse_key_combo_allows_lowercase_modifiers() {
        let combo = parse_key_combo("ctrl+p").expect("valid");
        assert!(combo.modifiers.contains(ModifierType::CONTROL_MASK));
        assert_eq!(combo.key, Key::p);
    }

    #[test]
    fn equal_key_alias_parses_and_displays_canonically() {
        let symbol = parse_key_combo("Ctrl+=").expect("symbol form is valid");
        let named = parse_key_combo("Ctrl+equal").expect("named form is valid");
        assert_eq!(symbol, named);
        assert_eq!(symbol.key, Key::equal);
        assert_eq!(key_combo_to_string(&symbol), "Ctrl+=");
    }

    #[test]
    fn invalid_override_keeps_default_binding() {
        let mut map = KeybindingMap::from_defaults();
        let original = parse_key_combo("Ctrl+Shift+T").unwrap();
        let table = "new_tab = 'Ctrl+NoSuchModifier+T'"
            .parse::<toml::Table>()
            .unwrap();
        map.apply_user_overrides(&table);
        assert_eq!(map.lookup(&original), Some(Action::NewTab));
    }

    #[test]
    fn conflicting_override_keeps_both_defaults() {
        let mut map = KeybindingMap::from_defaults();
        let new_tab = parse_key_combo("Ctrl+Shift+T").unwrap();
        let paste = parse_key_combo("Ctrl+Shift+V").unwrap();
        let table = "new_tab = 'Ctrl+Shift+V'".parse::<toml::Table>().unwrap();
        map.apply_user_overrides(&table);
        assert_eq!(map.lookup(&new_tab), Some(Action::NewTab));
        assert_eq!(map.lookup(&paste), Some(Action::Paste));
    }

    #[test]
    fn warp_block_action_defaults_are_not_shadowed() {
        let map = KeybindingMap::from_defaults();
        let cases = [
            ("Ctrl+Shift+A", Action::SelectAllBlocks),
            ("Ctrl+Shift+I", Action::ReinputSelectedCommands),
            ("Ctrl+Shift+K", Action::ClearBlocks),
        ];
        for (binding, expected) in cases {
            let combo = parse_key_combo(binding).expect("valid built-in binding");
            assert_eq!(map.lookup(&combo), Some(expected), "{binding}");
        }
    }

    #[test]
    fn unified_default_binding_matrix_is_stable() {
        let map = KeybindingMap::from_defaults();
        let cases = [
            ("Ctrl+Shift+T", Action::NewTab),
            ("Ctrl+Shift+W", Action::ClosePaneOrTab),
            ("Ctrl+Shift+C", Action::Copy),
            ("Ctrl+Shift+V", Action::Paste),
            ("Ctrl+=", Action::FontIncrease),
            ("Ctrl+minus", Action::FontDecrease),
            ("Ctrl+0", Action::FontReset),
            ("Ctrl+Alt+=", Action::OpacityIncrease),
            ("Ctrl+Alt+minus", Action::OpacityDecrease),
            ("Ctrl+Shift+F", Action::ToggleSearch),
            ("Ctrl+Shift+P", Action::ToggleCommandPalette),
            ("Ctrl+Shift+H", Action::OpenHistoryPalette),
            ("Ctrl+Shift+O", Action::ToggleSettings),
            ("Ctrl+Shift+R", Action::ReloadConfig),
            ("Ctrl+backslash", Action::ToggleSidebar),
            ("Ctrl+Shift+L", Action::FilterTabs),
            ("Ctrl+Shift+X", Action::FilterFailedBlocks),
            ("Ctrl+Shift+N", Action::ClearBlockFilter),
            ("Ctrl+Shift+A", Action::SelectAllBlocks),
            ("Ctrl+Shift+K", Action::ClearBlocks),
            ("Ctrl+Shift+I", Action::ReinputSelectedCommands),
            ("Ctrl+Shift+E", Action::SplitHorizontal),
            ("Ctrl+Shift+D", Action::SplitVertical),
            ("Ctrl+Shift+Tab", Action::PrevTab),
            ("Ctrl+Tab", Action::NextTab),
            ("Ctrl+Up", Action::ScrollUp),
            ("Ctrl+Down", Action::ScrollDown),
            ("Ctrl+PageUp", Action::PrevTab),
            ("Ctrl+PageDown", Action::NextTab),
            ("Ctrl+Shift+S", Action::ShowRemotePicker),
            ("Ctrl+Alt+Shift+Left", Action::ResizePaneLeft),
            ("Ctrl+Alt+Shift+Right", Action::ResizePaneRight),
            ("Ctrl+Alt+Shift+Up", Action::ResizePaneUp),
            ("Ctrl+Alt+Shift+Down", Action::ResizePaneDown),
            ("Ctrl+Alt+B", Action::ToggleTabPlacement),
            ("Ctrl+Shift+Z", Action::TogglePaneZoom),
            ("Ctrl+Shift+!", Action::MovePaneToNewTab),
            ("F12", Action::ToggleDebugDashboard),
            ("Ctrl+Alt+Left", Action::FocusPaneLeft),
            ("Ctrl+Alt+Right", Action::FocusPaneRight),
            ("Ctrl+Alt+Up", Action::FocusPaneUp),
            ("Ctrl+Alt+Down", Action::FocusPaneDown),
            ("Ctrl+Alt+Shift+A", Action::OpenAiPanel),
            ("Ctrl+Shift+Q", Action::AskAiAboutSelectedBlock),
            ("Ctrl+Shift+M", Action::OpenWorkflows),
            ("Ctrl+Alt+G", Action::OpenAgent),
            ("Ctrl+Shift+G", Action::CrossBlockSearch),
        ];

        for (binding, expected) in cases {
            let combo = parse_key_combo(binding).expect("valid default binding");
            assert_eq!(map.lookup(&combo), Some(expected), "{binding}");
        }
        for digit in 1..=8u8 {
            let binding = format!("Ctrl+{digit}");
            let combo = parse_key_combo(&binding).expect("valid tab digit binding");
            assert_eq!(
                map.lookup(&combo),
                Some(Action::QuickSwitchTab(digit - 1)),
                "{binding}"
            );
        }
        let last = parse_key_combo("Ctrl+9").expect("valid last-tab binding");
        assert_eq!(map.lookup(&last), Some(Action::QuickSwitchTab(9)));

        assert_eq!(
            map.bindings.len(),
            cases.len() + 9,
            "unexpected or duplicate default binding"
        );
    }

    #[test]
    fn shell_and_context_reserved_chords_have_no_global_default() {
        let map = KeybindingMap::from_defaults();
        for binding in [
            "Ctrl+R",
            "Ctrl+P",
            "Ctrl+comma",
            "Ctrl+period",
            "Ctrl+Shift+PageUp",
            "Ctrl+Shift+PageDown",
            "Ctrl+Shift+Left",
            "Ctrl+Shift+Right",
            "Ctrl+Shift+Y",
            "Ctrl+Shift++",
            "Ctrl+Shift+J",
            "Ctrl+Alt+Shift+K",
        ] {
            let combo = parse_key_combo(binding).expect("valid reserved binding");
            assert_eq!(map.lookup(&combo), None, "{binding} must stay unbound");
        }
        assert!(map
            .binding_display(&Action::CyclePaneFocusForward)
            .is_empty());
        assert!(map
            .binding_display(&Action::CyclePaneFocusBackward)
            .is_empty());
        assert!(map.binding_display(&Action::FilterSlowBlocks).is_empty());
        assert!(map.binding_display(&Action::FilterPinnedBlocks).is_empty());
        assert!(map.binding_display(&Action::JumpToPrevPinned).is_empty());
        assert!(map.binding_display(&Action::JumpToNextPinned).is_empty());
        assert!(map.binding_display(&Action::JumpToPrevFailed).is_empty());
        assert!(map.binding_display(&Action::JumpToNextFailed).is_empty());
        assert!(map.binding_display(&Action::UndoClearBlocks).is_empty());
        assert!(map
            .binding_display(&Action::ExportSessionMarkdown)
            .is_empty());
        assert!(map.binding_display(&Action::ExportSessionJson).is_empty());
    }
}
