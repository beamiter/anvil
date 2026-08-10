use gtk::gdk::{Key, ModifierType};
use jterm_core::keybindings::{is_unbind_token, parse, Chord, KeySym, Mods, NamedKey};
use relm4::gtk;
use std::collections::HashMap;

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
    /// Install jsh, or update the installed one, in a dedicated tab.
    InstallJsh,
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
    /// the anvil data directory.
    ExportSessionMarkdown,
    ExportSessionJson,
    ToggleDebugDashboard,
    /// Show or hide the session-level AI panel. This is the canonical action
    /// exposed by defaults, shared configuration, and the command palette.
    ToggleAiPanel,
    /// Open the session-level AI panel for free-form questions about the
    /// current shell context without closing an already-visible panel.
    /// Retained for existing `open_ai_panel` configurations.
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
            Action::ToggleCommandPalette => "Command palette",
            Action::OpenPalette => "Palette: search everything",
            Action::OpenHistoryPalette => "Palette: search history",
            Action::ToggleSettings => "Toggle settings panel",
            Action::ReloadConfig => "Reload configuration",
            Action::OpenWelcome => "Open welcome & quick start",
            Action::InstallJsh => "Install or update jsh (jterm's shell)",
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
            Action::ConnectRemote(index) => match index {
                0 => "Connect to remote host 1",
                1 => "Connect to remote host 2",
                2 => "Connect to remote host 3",
                3 => "Connect to remote host 4",
                4 => "Connect to remote host 5",
                5 => "Connect to remote host 6",
                6 => "Connect to remote host 7",
                7 => "Connect to remote host 8",
                _ => "Connect to remote host 9",
            },
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
            Action::ToggleAiPanel => "Toggle AI panel",
            Action::OpenAiPanel => "Open AI panel",
            Action::AskAiAboutSelectedBlock => "Ask AI about selected block",
            Action::OpenWorkflows => "Open workflows",
            Action::OpenAgent => "Toggle Shell Agent",
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
            // Palette-only: too rare to spend a chord on.
            Action::InstallJsh => None,
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
            Action::ConnectRemote(index) => match index {
                0 => Some("connect_remote_1"),
                1 => Some("connect_remote_2"),
                2 => Some("connect_remote_3"),
                3 => Some("connect_remote_4"),
                4 => Some("connect_remote_5"),
                5 => Some("connect_remote_6"),
                6 => Some("connect_remote_7"),
                7 => Some("connect_remote_8"),
                8 => Some("connect_remote_9"),
                _ => None,
            },
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
            Action::ToggleAiPanel => Some("toggle_ai_panel"),
            Action::OpenAiPanel => Some("open_ai_panel"),
            Action::AskAiAboutSelectedBlock => Some("ask_ai_about_selected_block"),
            Action::OpenWorkflows => Some("open_workflows"),
            Action::OpenAgent => Some("open_agent"),
            Action::CrossBlockSearch => Some("cross_block_search"),
        }
    }

    /// Resolve the visibility requested by an AI-panel action. Keeping this
    /// tiny policy pure makes it difficult for the keyboard and palette
    /// dispatch paths to accidentally turn the legacy open action into a
    /// toggle (or vice versa).
    pub(crate) fn ai_panel_target_visibility(self, currently_visible: bool) -> Option<bool> {
        match self {
            Action::ToggleAiPanel => Some(!currently_visible),
            Action::OpenAiPanel => Some(true),
            _ => None,
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
            Action::InstallJsh,
            Action::ToggleSidebar,
            Action::SplitHorizontal,
            Action::SplitVertical,
            Action::PrevTab,
            Action::NextTab,
            Action::ScrollUp,
            Action::ScrollDown,
            Action::CyclePaneFocusForward,
            Action::CyclePaneFocusBackward,
            Action::ConnectRemote(0),
            Action::ConnectRemote(1),
            Action::ConnectRemote(2),
            Action::ConnectRemote(3),
            Action::ConnectRemote(4),
            Action::ConnectRemote(5),
            Action::ConnectRemote(6),
            Action::ConnectRemote(7),
            Action::ConnectRemote(8),
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
            Action::ToggleAiPanel,
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

/// Translate a GTK key event into a toolkit-neutral [`Chord`], or `None`
/// for keysyms no chord string can name (numpad keysyms, bare modifiers,
/// dead keys).
///
/// Chord parsing, display, and hashing live in `jterm_core::keybindings`;
/// this function is the only place GTK key facts remain:
///
/// - Only Ctrl/Shift/Alt are extracted from the modifier state, exactly
///   as before the migration: a chord still fires with Super or a lock
///   modifier held, and `super+...` config chords parse but never match.
/// - `ISO_Left_Tab` (what GTK reports for Shift+Tab) folds to `Tab`.
/// - `KP_*` keysyms (numpad digits, `KP_Enter`, `KP_Add`, ...) are
///   excluded rather than folded onto the main row — numpad folding is
///   not done at this edge today, and no chord string could name these
///   keysyms before the migration either.
/// - Letters, digits, and symbols map via `Key::to_unicode()`, lowercased
///   to match the core invariant that `KeySym::Char` holds lowercase.
/// - `F1`..`F24` map via the keysym name.
pub(crate) fn chord_from_gdk(keyval: Key, state: ModifierType) -> Option<Chord> {
    let mods = Mods {
        ctrl: state.contains(ModifierType::CONTROL_MASK),
        shift: state.contains(ModifierType::SHIFT_MASK),
        alt: state.contains(ModifierType::ALT_MASK),
        sup: false,
    };
    let key = keysym_from_gdk(keyval)?;
    Some(Chord { mods, key })
}

fn keysym_from_gdk(keyval: Key) -> Option<KeySym> {
    let named = match keyval {
        Key::Tab | Key::ISO_Left_Tab => Some(NamedKey::Tab),
        Key::Escape => Some(NamedKey::Escape),
        Key::Return => Some(NamedKey::Return),
        Key::space => Some(NamedKey::Space),
        Key::BackSpace => Some(NamedKey::Backspace),
        Key::Delete => Some(NamedKey::Delete),
        Key::Home => Some(NamedKey::Home),
        Key::End => Some(NamedKey::End),
        Key::Insert => Some(NamedKey::Insert),
        Key::Page_Up => Some(NamedKey::PageUp),
        Key::Page_Down => Some(NamedKey::PageDown),
        Key::Up => Some(NamedKey::Up),
        Key::Down => Some(NamedKey::Down),
        Key::Left => Some(NamedKey::Left),
        Key::Right => Some(NamedKey::Right),
        _ => None,
    };
    if let Some(named) = named {
        return Some(KeySym::Named(named));
    }
    if let Some(name) = keyval.name() {
        // KP_Enter and the other numpad keysyms stay unmatchable, as they
        // were before the migration (KP_1 must not alias Ctrl+1).
        if name.starts_with("KP_") {
            return None;
        }
        // F25+ exists in GDK, but no chord string can name it.
        if let Some(n) = name
            .strip_prefix('F')
            .and_then(|digits| digits.parse::<u8>().ok())
            .filter(|n| (1..=24).contains(n))
        {
            return Some(KeySym::Function(n));
        }
    }
    // Printable keys, lowercased to the core `Char` invariant. A
    // character whose lowercase expands to multiple chars is unmatchable
    // by `parse` and is dropped, mirroring the parser's rejection.
    let c = keyval.to_unicode().filter(|c| !c.is_control())?;
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(lc), None) => Some(KeySym::Char(lc)),
        _ => None,
    }
}

#[derive(Clone)]
pub(crate) struct KeybindingMap {
    pub(crate) bindings: HashMap<Chord, Action>,
}

impl KeybindingMap {
    pub(crate) fn from_defaults() -> Self {
        let mut bindings = HashMap::new();

        let mut bind = |s: &str, action: Action| {
            if let Ok(chord) = parse(s) {
                bindings.insert(chord, action);
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
        // Letter fallbacks remain usable when GNOME reserves Ctrl+Alt+Arrow.
        bind("Ctrl+Alt+H", Action::FocusPaneLeft);
        bind("Ctrl+Alt+J", Action::FocusPaneDown);
        bind("Ctrl+Alt+K", Action::FocusPaneUp);
        bind("Ctrl+Alt+L", Action::FocusPaneRight);
        bind("Ctrl+Alt+Shift+H", Action::ResizePaneLeft);
        bind("Ctrl+Alt+Shift+J", Action::ResizePaneDown);
        bind("Ctrl+Alt+Shift+K", Action::ResizePaneUp);
        bind("Ctrl+Alt+Shift+L", Action::ResizePaneRight);
        bind("Ctrl+Alt+Shift+A", Action::ToggleAiPanel);
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
        // Accept Forge's canonical spellings so one shared [keybindings]
        // table works in both frontends. Anvil's historical keys remain the
        // emitted/documented names for backward compatibility.
        key_to_action.insert("history_palette", Action::OpenHistoryPalette);
        key_to_action.insert("workflows_palette", Action::OpenWorkflows);

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
            if is_unbind_token(key_str) {
                self.bindings.retain(|_, bound| *bound != action);
                continue;
            }
            let combo = match parse(key_str) {
                Ok(chord) => chord,
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

    pub(crate) fn lookup(&self, chord: &Chord) -> Option<Action> {
        self.bindings.get(chord).copied()
    }

    pub(crate) fn binding_display(&self, action: &Action) -> String {
        let chords: Vec<_> = self
            .bindings
            .iter()
            .filter(|(_, a)| *a == action)
            .map(|(k, _)| k.display())
            .collect();
        chords.join(", ")
    }

    pub(crate) fn all_bound_actions(&self) -> Vec<(Action, String)> {
        let mut result = Vec::new();
        for action in Action::all_actions() {
            // `open_ai_panel` remains dispatchable from legacy configuration,
            // but the palette presents one unambiguous, canonical AI action.
            if action == Action::OpenAiPanel {
                continue;
            }
            let display = self.binding_display(&action);
            result.push((action, display));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jterm_core::keybindings::{CommonAction, ParseError, DEFAULT_CHORDS};

    #[test]
    fn parse_trims_whitespace() {
        let chord = parse(" Ctrl + Shift + P ").expect("valid");
        assert!(chord.mods.ctrl);
        assert!(chord.mods.shift);
        assert_eq!(chord.key, KeySym::Char('p'));
    }

    #[test]
    fn parse_empty_string_is_error() {
        assert_eq!(parse("   "), Err(ParseError::Empty));
    }

    #[test]
    fn parse_allows_lowercase_and_aliased_modifiers() {
        let chord = parse("ctrl+p").expect("valid");
        assert!(chord.mods.ctrl);
        assert_eq!(chord.key, KeySym::Char('p'));
        // The shared grammar deliberately widens what anvil alone used
        // to accept: control/option style aliases parse too now.
        assert_eq!(parse("control+p"), Ok(chord));
        assert_eq!(parse("option+left"), parse("alt+left"));
    }

    #[test]
    fn equal_key_alias_parses_and_displays_canonically() {
        let symbol = parse("Ctrl+=").expect("symbol form is valid");
        let named = parse("Ctrl+equal").expect("named form is valid");
        assert_eq!(symbol, named);
        assert_eq!(symbol.key, KeySym::Char('='));
        assert_eq!(symbol.display(), "Ctrl+=");
    }

    /// The display strings the palette and dialogs show must not change
    /// shape across the core migration (modifier order Ctrl+Shift+Alt,
    /// Return spelled "Enter"). One deliberate exception: backslash now
    /// displays as the literal `\` (the family decision) where the old
    /// code showed the word "backslash".
    #[test]
    fn display_matches_the_legacy_anvil_format() {
        for (input, want) in [
            ("Ctrl+Shift+T", "Ctrl+Shift+T"),
            ("Ctrl+equal", "Ctrl+="),
            ("Ctrl+minus", "Ctrl+-"),
            ("Ctrl+Alt+Shift+Left", "Ctrl+Shift+Alt+Left"),
            ("Ctrl+Enter", "Ctrl+Enter"),
            ("Ctrl+Shift+!", "Ctrl+Shift+!"),
            ("Ctrl+PageUp", "Ctrl+PageUp"),
            ("Ctrl+backslash", "Ctrl+\\"),
            ("F12", "F12"),
        ] {
            assert_eq!(parse(input).unwrap().display(), want, "{input}");
        }
    }

    #[test]
    fn gdk_edge_folds_iso_left_tab_and_lowercases_letters() {
        let ctrl_shift = ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK;
        assert_eq!(
            chord_from_gdk(Key::ISO_Left_Tab, ctrl_shift),
            Some(parse("ctrl+shift+tab").unwrap())
        );
        assert_eq!(
            chord_from_gdk(Key::T, ctrl_shift),
            Some(parse("ctrl+shift+t").unwrap())
        );
    }

    #[test]
    fn gdk_edge_maps_named_function_symbol_and_digit_keys() {
        let ctrl = ModifierType::CONTROL_MASK;
        let cases = [
            (Key::Return, ModifierType::empty(), "enter"),
            (Key::space, ctrl, "ctrl+space"),
            (Key::Page_Up, ctrl, "ctrl+pageup"),
            (Key::F12, ModifierType::empty(), "f12"),
            (Key::backslash, ctrl, "ctrl+backslash"),
            (Key::equal, ctrl, "ctrl+="),
            (Key::exclam, ctrl_shift(), "ctrl+shift+!"),
            (Key::_1, ctrl, "ctrl+1"),
        ];
        for (keyval, state, chord_str) in cases {
            assert_eq!(
                chord_from_gdk(keyval, state),
                Some(parse(chord_str).unwrap()),
                "{chord_str}"
            );
        }
    }

    fn ctrl_shift() -> ModifierType {
        ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK
    }

    #[test]
    fn gdk_edge_excludes_numpad_keysyms_and_bare_modifiers() {
        // Numpad folding is NOT done at this edge: KP_1, KP_Enter, and
        // friends were unmatchable before the migration and must stay so.
        for keyval in [
            Key::KP_1,
            Key::KP_0,
            Key::KP_Enter,
            Key::KP_Add,
            Key::Control_L,
        ] {
            assert_eq!(
                chord_from_gdk(keyval, ModifierType::CONTROL_MASK),
                None,
                "{keyval:?}"
            );
        }
    }

    #[test]
    fn gdk_edge_ignores_super_and_lock_modifier_state() {
        // Only Ctrl/Shift/Alt are extracted, as before the migration: a
        // chord still fires with Super or a lock modifier held.
        let plain = chord_from_gdk(Key::c, ctrl_shift());
        let noisy = chord_from_gdk(
            Key::c,
            ctrl_shift() | ModifierType::SUPER_MASK | ModifierType::LOCK_MASK,
        );
        assert_eq!(plain, noisy);
        assert_eq!(plain, Some(parse("ctrl+shift+c").unwrap()));
    }

    /// The cross-app ergonomic contract: every family default chord maps
    /// onto an anvil action, and `from_defaults` must bind exactly that
    /// chord to it. `ctrl+shift+a` (SelectAllBlocks here) is deliberately
    /// absent from the shared table — see DEFAULT_CHORDS' exclusion list.
    #[test]
    fn common_default_chord_table_is_honored() {
        fn local_action(common: CommonAction) -> Action {
            match common {
                CommonAction::NewTab => Action::NewTab,
                CommonAction::ClosePaneOrTab => Action::ClosePaneOrTab,
                CommonAction::Copy => Action::Copy,
                CommonAction::Paste => Action::Paste,
                CommonAction::NextTab => Action::NextTab,
                CommonAction::PrevTab => Action::PrevTab,
                // anvil binds the ctrl+page pair to the same tab cycling
                // actions as ctrl+tab / ctrl+shift+tab.
                CommonAction::NextTabPage => Action::NextTab,
                CommonAction::PrevTabPage => Action::PrevTab,
                CommonAction::QuickSwitch(n) => Action::QuickSwitchTab(n - 1),
                CommonAction::LastTab => Action::QuickSwitchTab(9),
                CommonAction::FontIncrease => Action::FontIncrease,
                CommonAction::FontDecrease => Action::FontDecrease,
                CommonAction::FontReset => Action::FontReset,
                CommonAction::Search => Action::ToggleSearch,
                CommonAction::CommandPalette => Action::ToggleCommandPalette,
                CommonAction::Settings => Action::ToggleSettings,
                CommonAction::Sidebar => Action::ToggleSidebar,
                CommonAction::DebugPanel => Action::ToggleDebugDashboard,
                CommonAction::ScrollUp => Action::ScrollUp,
                CommonAction::ScrollDown => Action::ScrollDown,
                CommonAction::PaneFocusLeft => Action::FocusPaneLeft,
                CommonAction::PaneFocusRight => Action::FocusPaneRight,
                CommonAction::PaneFocusUp => Action::FocusPaneUp,
                CommonAction::PaneFocusDown => Action::FocusPaneDown,
                CommonAction::PaneResizeLeft => Action::ResizePaneLeft,
                CommonAction::PaneResizeRight => Action::ResizePaneRight,
                CommonAction::PaneResizeUp => Action::ResizePaneUp,
                CommonAction::PaneResizeDown => Action::ResizePaneDown,
                CommonAction::SplitSideBySide => Action::SplitHorizontal,
                CommonAction::SplitStacked => Action::SplitVertical,
                CommonAction::PaneZoom => Action::TogglePaneZoom,
            }
        }

        let map = KeybindingMap::from_defaults();
        for (common, chord_str) in DEFAULT_CHORDS {
            let chord = parse(chord_str).expect("contract chords parse");
            assert_eq!(
                map.lookup(&chord),
                Some(local_action(*common)),
                "family contract: {chord_str} must be bound to {common:?}"
            );
        }
    }

    #[test]
    fn string_unbind_tokens_remove_a_binding() {
        // "unbind" is new with the shared core; empty/none/disabled were
        // already honored before the migration.
        for token in ["unbind", "none", "disabled", ""] {
            let mut map = KeybindingMap::from_defaults();
            let sidebar = parse("ctrl+backslash").unwrap();
            assert_eq!(map.lookup(&sidebar), Some(Action::ToggleSidebar));
            let table = format!("toggle_sidebar = '{token}'")
                .parse::<toml::Table>()
                .unwrap();
            map.apply_user_overrides(&table);
            assert_eq!(map.lookup(&sidebar), None, "token {token:?}");
        }
    }

    #[test]
    fn invalid_override_keeps_default_binding() {
        let mut map = KeybindingMap::from_defaults();
        let original = parse("Ctrl+Shift+T").unwrap();
        let table = "new_tab = 'Ctrl+NoSuchModifier+T'"
            .parse::<toml::Table>()
            .unwrap();
        map.apply_user_overrides(&table);
        assert_eq!(map.lookup(&original), Some(Action::NewTab));
    }

    #[test]
    fn conflicting_override_keeps_both_defaults() {
        let mut map = KeybindingMap::from_defaults();
        let new_tab = parse("Ctrl+Shift+T").unwrap();
        let paste = parse("Ctrl+Shift+V").unwrap();
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
            let combo = parse(binding).expect("valid built-in binding");
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
            ("Ctrl+Alt+H", Action::FocusPaneLeft),
            ("Ctrl+Alt+J", Action::FocusPaneDown),
            ("Ctrl+Alt+K", Action::FocusPaneUp),
            ("Ctrl+Alt+L", Action::FocusPaneRight),
            ("Ctrl+Alt+Shift+H", Action::ResizePaneLeft),
            ("Ctrl+Alt+Shift+J", Action::ResizePaneDown),
            ("Ctrl+Alt+Shift+K", Action::ResizePaneUp),
            ("Ctrl+Alt+Shift+L", Action::ResizePaneRight),
            ("Ctrl+Alt+Shift+A", Action::ToggleAiPanel),
            ("Ctrl+Shift+Q", Action::AskAiAboutSelectedBlock),
            ("Ctrl+Shift+M", Action::OpenWorkflows),
            ("Ctrl+Alt+G", Action::OpenAgent),
            ("Ctrl+Shift+G", Action::CrossBlockSearch),
        ];

        for (binding, expected) in cases {
            let combo = parse(binding).expect("valid default binding");
            assert_eq!(map.lookup(&combo), Some(expected), "{binding}");
        }
        for digit in 1..=8u8 {
            let binding = format!("Ctrl+{digit}");
            let combo = parse(&binding).expect("valid tab digit binding");
            assert_eq!(
                map.lookup(&combo),
                Some(Action::QuickSwitchTab(digit - 1)),
                "{binding}"
            );
        }
        let last = parse("Ctrl+9").expect("valid last-tab binding");
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
        ] {
            let combo = parse(binding).expect("valid reserved binding");
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

    #[test]
    fn ai_panel_parser_and_dispatch_contract_keeps_legacy_open_distinct() {
        let default = parse("Ctrl+Alt+Shift+A").unwrap();
        let mut map = KeybindingMap::from_defaults();
        assert_eq!(map.lookup(&default), Some(Action::ToggleAiPanel));

        let table = "open_ai_panel = 'F5'".parse::<toml::Table>().unwrap();
        map.apply_user_overrides(&table);
        assert_eq!(map.lookup(&parse("F5").unwrap()), Some(Action::OpenAiPanel));

        assert_eq!(
            Action::ToggleAiPanel.ai_panel_target_visibility(false),
            Some(true)
        );
        assert_eq!(
            Action::ToggleAiPanel.ai_panel_target_visibility(true),
            Some(false)
        );
        assert_eq!(
            Action::OpenAiPanel.ai_panel_target_visibility(false),
            Some(true)
        );
        assert_eq!(
            Action::OpenAiPanel.ai_panel_target_visibility(true),
            Some(true)
        );

        let palette_actions = map.all_bound_actions();
        assert!(palette_actions
            .iter()
            .any(|(action, _)| *action == Action::ToggleAiPanel));
        assert!(palette_actions
            .iter()
            .all(|(action, _)| *action != Action::OpenAiPanel));
    }

    #[test]
    fn indexed_remote_actions_share_the_forge_config_contract() {
        let actions = Action::all_actions();
        for index in 0..9u8 {
            let action = Action::ConnectRemote(index);
            let expected = format!("connect_remote_{}", index + 1);
            assert_eq!(action.config_key(), Some(expected.as_str()));
            assert!(actions.contains(&action));
        }
        assert_eq!(Action::ConnectRemote(9).config_key(), None);

        let mut table = toml::Table::new();
        table.insert("connect_remote_1".into(), toml::Value::String("F4".into()));
        let mut map = KeybindingMap::from_defaults();
        map.apply_user_overrides(&table);
        assert_eq!(
            map.lookup(&parse("F4").unwrap()),
            Some(Action::ConnectRemote(0))
        );
    }
}
