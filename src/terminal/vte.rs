//! VTE terminal backend as a relm4 Component.
//!
//! Wraps a `vte4::Terminal` + `gtk::Scrollbar` row and an optional native
//! display-only companion outside its grid. The shell
//! is spawned on init. VTE signals (cwd/exit/bell/title/activity) are forwarded
//! as component Output messages instead of forge's callback-Vec observer model.

use gtk::gdk::RGBA;
use gtk::gio;
use gtk::prelude::*;
use gtk::Orientation;
use relm4::gtk;
use relm4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use vte4::{CursorBlinkMode, CursorShape, Terminal};
use vte4::{TerminalExt, TerminalExtManual};

#[cfg(test)]
use super::InitialCommands;
use super::{PaneProbe, VteInit, VteInput, VteOutput};
use crate::child_env;
use crate::config::Config;
use crate::search::{invalid_regex_message, SearchStatus};

// ─── Terminal widget construction (ported from forge terminal.rs) ──────────

/// Immediate explicit focus followed by an ownership-scoped remap repair.
/// A reopened search or another pane chosen before idle always wins.
fn focus_terminal(terminal: &Terminal) {
    terminal.grab_focus();
    if terminal.is_mapped() && terminal.has_focus() {
        crate::block_view::restore_live_focus_deferred(terminal);
    } else {
        // Preserve the initial-map retry; mapped focus returns above must
        // not carry this unconditional startup authority.
        let terminal = terminal.clone();
        gtk::glib::idle_add_local_once(move || {
            terminal.grab_focus();
        });
    }
}

pub(crate) fn create_terminal(config: &Config) -> Terminal {
    let font_scale = config.default_font_scale;
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(true)
        .name("term_name")
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(true)
        .scrollback_lines(config.terminal_scrollback_lines)
        .cursor_blink_mode(CursorBlinkMode::System)
        .cursor_shape(CursorShape::Block)
        .font_scale(font_scale)
        .opacity(1.0)
        .pointer_autohide(true)
        .enable_sixel(true)
        .build();

    terminal.set_mouse_autohide(true);
    // Backspace must send DEL (0x7f), not BS (0x08); readline/most shells only
    // erase on DEL, so the default binding leaves Backspace unable to delete.
    terminal.set_backspace_binding(vte4::EraseBinding::AsciiDelete);
    terminal.set_delete_binding(vte4::EraseBinding::DeleteSequence);

    let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
    terminal.set_colors(
        Some(&config.foreground),
        Some(&config.background),
        &palette_refs,
    );
    terminal.set_color_bold(None);
    terminal.set_color_cursor(Some(&config.cursor));
    terminal.set_color_cursor_foreground(Some(&config.cursor_foreground));

    let font_desc = crate::font::terminal_font_description(&config.font_desc, config);
    terminal.set_font(Some(&font_desc));

    crate::block_view::add_url_match_regex(&terminal);

    terminal
}

/// Wrap a terminal in an hbox with a scrollbar on the right side.
pub(crate) fn wrap_with_scrollbar(terminal: &Terminal) -> gtk::Box {
    let hbox = gtk::Box::new(Orientation::Horizontal, 0);
    hbox.set_hexpand(true);
    hbox.set_vexpand(true);
    hbox.add_css_class("terminal-box");
    let scrollbar = gtk::Scrollbar::new(Orientation::Vertical, terminal.vadjustment().as_ref());
    hbox.append(terminal);
    hbox.append(&scrollbar);
    hbox
}

/// Mount the production VTE/scrollbar grid in its caller-owned pane root.
/// Widget assembly only: no PTY, child process, signal routing or ownership policy.
pub(crate) fn mount_terminal_grid(root: &gtk::Box, terminal: &Terminal) -> gtk::Box {
    root.set_hexpand(true);
    root.set_vexpand(true);
    root.add_css_class("terminal-box");
    let scrollbar = gtk::Scrollbar::new(Orientation::Vertical, terminal.vadjustment().as_ref());
    let grid = gtk::Box::new(Orientation::Horizontal, 0);
    grid.set_hexpand(true);
    grid.set_vexpand(true);
    grid.append(terminal);
    grid.append(&scrollbar);
    root.append(&grid);
    grid
}

pub(crate) fn terminal_working_directory(terminal: &Terminal) -> Option<String> {
    if let Some(uri) = terminal.current_directory_uri() {
        let file = gio::File::for_uri(uri.as_str());
        if let Some(path) = file
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
        {
            return Some(path);
        }
    }
    let pid: i32 = unsafe { *terminal.data::<i32>("child-pid")?.as_ref() };
    jterm_core::process::process_cwd(pid)
}

pub(crate) use super::legacy_view::setup_terminal_click_handler;

fn launch_failure_message(error: &impl std::fmt::Display) -> String {
    format!("Terminal failed to start: {error}. Check the shell, remote command, or host bridge.")
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_shell(
    terminal: &Terminal,
    argv_owned: &[String],
    working_directory: Option<&str>,
    session_id: Option<&str>,
    cwd_token: &str,
    initial_commands: &[String],
    env_extra: &[(String, String)],
    auto_gui: bool,
    probe: PaneProbe,
    sender: ComponentSender<VteTerminal>,
    on_launch_failed: impl Fn() + 'static,
) {
    let (argv_vec, _) = crate::config::shell_argv_with_session(argv_owned, session_id);
    let home = std::env::var("HOME").ok();
    let requested_working_directory = working_directory.or(home.as_deref());
    let effective_working_directory = requested_working_directory.filter(|directory| {
        let available = crate::host::working_directory_available(directory);
        if !available {
            log::warn!("VTE working directory is unavailable; using the application directory");
        }
        available
    });
    // The integration marker and cwd-authentication token must be encoded in
    // the host wrapper as well as VTE's environment. The shell integration
    // immediately removes the token from its exported environment.
    let argv_vec = crate::agent_cli::inline_argv(&argv_vec);
    let vim_environment = if env_extra.is_empty() {
        crate::vim9_ide::shell_environment(argv_owned, auto_gui)
    } else {
        Vec::new()
    };
    let mut host_environment = Vec::from(crate::terminal::cwd_token_environment(cwd_token));
    host_environment.extend(
        vim_environment
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    host_environment.extend(
        env_extra
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    let argv_vec =
        crate::host::wrap_argv(&argv_vec, effective_working_directory, &host_environment);
    let argv: Vec<&str> = argv_vec.iter().map(|s| s.as_str()).collect();

    // `vte_envv_from_captured` is the child's complete environment, built from
    // the launch-time snapshot `main` froze; the cwd-authentication token rides
    // along as the per-call extra. The owned launcher passes that complete
    // block directly to execve, keeping later GTK/frontend environment writes
    // out of the shell. Explicit TERM/VTE_VERSION entries remain intact.
    let envv_owned: Vec<String> = match child_env::vte_envv_from_captured(
        &child_env::ChildEnv::from_identity(),
        &host_environment,
    ) {
        Ok(envv) => envv,
        Err(error) => {
            on_launch_failed();
            let message = launch_failure_message(&error);
            log::error!("{message}");
            terminal.feed(format!("\r\nanvil: {message}\r\n").as_bytes());
            let _ = sender.output(VteOutput::LaunchFailed(message));
            return;
        }
    };
    let envv_owned = crate::agent_cli::vte_environment(envv_owned);
    let envv: Vec<&str> = envv_owned.iter().map(String::as_str).collect();
    let spawn_working_directory = if crate::host::is_flatpak() {
        None
    } else {
        effective_working_directory
    };
    let terminal_for_pid = terminal.clone();

    let init_cmds = initial_commands.to_vec();

    crate::gtk_spawn::spawn(
        terminal,
        &argv,
        &envv,
        false,
        spawn_working_directory,
        move |result| {
            log::debug!(
                "owned terminal launch: {:?}",
                result.as_ref().map(|child| child.pid())
            );
            let launched_child = match result {
                Ok(child) => {
                    if crate::gtk_process::vte_closed(&terminal_for_pid) {
                        return;
                    }
                    let pid = child.pid();
                    unsafe {
                        terminal_for_pid.set_data::<i32>("child-pid", pid);
                    }
                    probe.shell_pid.set(pid);
                    if let Some(pty) = terminal_for_pid.pty() {
                        use std::os::fd::AsRawFd;
                        probe.pty_fd.set(pty.fd().as_raw_fd());
                    }
                    let _ = sender.output(VteOutput::Launched);
                    child
                }
                Err(error) => {
                    on_launch_failed();
                    if crate::gtk_process::vte_closed(&terminal_for_pid) {
                        return;
                    }
                    let message = launch_failure_message(&error);
                    log::error!("{message}");
                    terminal_for_pid.feed(format!("\r\nanvil: {message}\r\n").as_bytes());
                    let _ = sender.output(VteOutput::LaunchFailed(message));
                    return;
                }
            };
            if !init_cmds.is_empty() {
                crate::gtk_process::schedule_initial_commands(
                    &terminal_for_pid,
                    launched_child,
                    move || init_cmds,
                );
            }
        },
    );
}

// ─── VteTerminal relm4 Component ────────────────────────────────────────────

pub struct VteTerminal {
    terminal: Terminal,
    config: Rc<RefCell<Config>>,
    search_status: SearchStatus,
    companion: Rc<crate::organism_ui::vte_surface::VteCompanion>,
}

impl Drop for VteTerminal {
    fn drop(&mut self) {
        crate::gtk_process::close_vte(&self.terminal);
    }
}

impl VteTerminal {
    pub(crate) fn companion(&self) -> Rc<crate::organism_ui::vte_surface::VteCompanion> {
        Rc::clone(&self.companion)
    }
    pub fn terminal(&self) -> &Terminal {
        &self.terminal
    }
}

impl Component for VteTerminal {
    type Init = VteInit;
    type Input = VteInput;
    type Output = VteOutput;
    type CommandOutput = ();
    type Root = gtk::Box;
    type Widgets = ();

    fn init_root() -> Self::Root {
        gtk::Box::new(Orientation::Vertical, 0)
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let terminal = create_terminal(&init.config.borrow());
        if crate::agent_cli::is_inline_launch(&init.shell_argv) {
            crate::block_view::install_inline_cli_reading(&terminal);
        }

        mount_terminal_grid(&root, &terminal);
        let companion = crate::organism_ui::vte_surface::VteCompanion::new(
            &root,
            &terminal,
            init.config.clone(),
            !init.working_directory_external,
        );

        setup_terminal_click_handler(&terminal);

        // Forward VTE signals as Output messages.
        {
            let sender = sender.clone();
            let term_for_cwd = terminal.clone();
            let probe_for_cwd = init.probe.clone();
            let cwd_token = init.cwd_token.clone();
            let companion = Rc::downgrade(&companion);
            terminal.connect_current_directory_uri_notify(move |_| {
                if let Some(uri) = term_for_cwd.current_directory_uri() {
                    if let Ok((path, host)) = gtk::glib::filename_from_uri(uri.as_str()) {
                        let path = path.to_string_lossy().to_string();
                        if path.is_empty() {
                            return;
                        }
                        let authority =
                            crate::terminal::classify_cwd_authority(host.as_deref(), &cwd_token);
                        let foreground = crate::process::foreground_uses_external_cwd(
                            probe_for_cwd.pty_fd.get(),
                            probe_for_cwd.shell_pid.get(),
                        );
                        let external = crate::terminal::resolve_cwd_external(authority, foreground);
                        if let Some(companion) = companion.upgrade() {
                            companion.set_local(!external);
                        }
                        let _ = sender.output(VteOutput::CwdChanged { path, external });
                    }
                }
            });
        }
        {
            let sender = sender.clone();
            let companion = Rc::downgrade(&companion);
            terminal.connect_child_exited(move |term, status| {
                let Some(code) = jterm_core::process::exit_code_from_wait_status(status) else {
                    return;
                };
                if let Some(companion) = companion.upgrade() {
                    companion.close();
                }
                if !crate::gtk_process::vte_closed(term) {
                    let _ = sender.output(VteOutput::Exited(code));
                }
            });
        }
        {
            let sender = sender.clone();
            terminal.connect_bell(move |_term| {
                let _ = sender.output(VteOutput::Bell);
            });
        }
        {
            let sender = sender.clone();
            let term_for_title = terminal.clone();
            terminal.connect_window_title_changed(move |_term| {
                // An empty title is forwarded too, as the Block view does: it
                // is a program resetting the title it set (claude sends
                // `OSC 0 ;` on exit), and the tab falls back to its default.
                if let Some(title) = term_for_title.window_title() {
                    let _ = sender.output(VteOutput::TitleChanged(title.to_string()));
                }
            });
        }
        {
            let sender = sender.clone();
            terminal.connect_contents_changed(move |_term| {
                let _ = sender.output(VteOutput::Activity);
            });
        }

        spawn_shell(
            &terminal,
            &init.shell_argv,
            init.working_directory.as_deref(),
            init.session_id.as_deref(),
            &init.cwd_token,
            init.initial_commands.as_slice(),
            &init.env_extra,
            init.config.borrow().vim9.auto_gui,
            init.probe.clone(),
            sender.clone(),
            {
                let companion = Rc::downgrade(&companion);
                move || {
                    if let Some(companion) = companion.upgrade() {
                        companion.close();
                    }
                }
            },
        );

        // Grab focus once the widget is realized.
        {
            let term_for_focus = terminal.clone();
            terminal.connect_realize(move |_| {
                term_for_focus.grab_focus();
            });
        }

        // Report focus-enter so the app can track the active pane.
        {
            let sender = sender.clone();
            let focus_ctl = gtk::EventControllerFocus::new();
            focus_ctl.connect_enter(move |_| {
                let _ = sender.output(VteOutput::Focused);
            });
            terminal.add_controller(focus_ctl);
        }

        let model = VteTerminal {
            terminal,
            config: init.config,
            search_status: SearchStatus::Idle,
            companion,
        };
        ComponentParts { model, widgets: () }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            VteInput::WriteInput(data) => self.terminal.feed_child(&data),
            VteInput::InsertReviewText(data) => {
                crate::enter_ownership::claim_held();
                self.terminal.feed_child(&data);
            }
            // The plain VTE keeps its drop on the write path it always used.
            VteInput::PasteText(text) => {
                crate::enter_ownership::claim_held();
                self.terminal.feed_child(text.as_bytes());
            }
            VteInput::RunAgentCommand { execution, .. } => {
                let _ = sender.output(VteOutput::AgentExecutionStartFailed { execution });
            }
            VteInput::Resize(cols, rows) => {
                if let Some(pty) = self.terminal.pty() {
                    let _ = pty.set_size(rows as i32, cols as i32);
                }
            }
            VteInput::GrabFocus => {
                focus_terminal(&self.terminal);
            }
            VteInput::Copy | VteInput::CopyOutputOnly => {
                if let Some(message) = copy_empty_notice(self.terminal.has_selection()) {
                    let _ = sender.output(VteOutput::Notice(message.to_string()));
                } else {
                    crate::vte_reference::copy_selection(&self.terminal);
                }
            }
            VteInput::Paste => self.terminal.paste_clipboard(),
            VteInput::SetFontScale(scale) => self.terminal.set_font_scale(scale),
            VteInput::SetFont(desc) => {
                let fd = crate::font::terminal_font_description(&desc, &self.config.borrow());
                self.terminal.set_font(Some(&fd));
            }
            VteInput::SetScrollback(lines) => self.terminal.set_scrollback_lines(lines),
            VteInput::ScrollLines(lines) => {
                if let Some(adj) = self.terminal.vadjustment() {
                    let delta = adj.step_increment() * lines as f64;
                    let max_val = adj.upper() - adj.page_size();
                    let new_val =
                        (adj.value() + delta).clamp(adj.lower(), max_val.max(adj.lower()));
                    adj.set_value(new_val);
                }
            }
            VteInput::ApplyTheme => {
                let config = self.config.borrow();
                let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
                self.terminal.set_colors(
                    Some(&config.foreground),
                    Some(&config.background),
                    &palette_refs,
                );
                self.terminal.set_color_bold(None);
                self.terminal.set_color_cursor(Some(&config.cursor));
                self.terminal
                    .set_color_cursor_foreground(Some(&config.cursor_foreground));
            }
            VteInput::SyncConfig => self.companion.refresh(),
            VteInput::Kill => {
                self.companion.close();
                crate::gtk_process::close_vte(&self.terminal);
            }
            VteInput::ReinputSelectedCommands => {
                let _ = sender.output(VteOutput::Notice(vte_block_mode_only_notice(
                    "Selected-command recall",
                )));
            }
            VteInput::SelectAllBlocks => {
                let _ = sender.output(VteOutput::Notice(vte_block_mode_only_notice(
                    "Block selection",
                )));
            }
            VteInput::DeleteSelectedBlocks => {
                let _ = sender.output(VteOutput::Notice(vte_block_mode_only_notice(
                    "Deleting selected blocks",
                )));
            }
            VteInput::ClearBlocks => {
                let _ = sender.output(VteOutput::Notice(vte_block_mode_only_notice(
                    "Clearing finished blocks",
                )));
            }
            VteInput::UndoClearBlocks => {
                let _ = sender.output(VteOutput::Notice(vte_block_mode_only_notice(
                    "Restoring cleared blocks",
                )));
            }
            VteInput::FilterFailedBlocks => {
                let _ = sender.output(VteOutput::Notice(vte_block_mode_only_notice(
                    "Failed-block navigation",
                )));
            }
            VteInput::FilterSlowBlocks => {
                let _ = sender.output(VteOutput::Notice(vte_block_mode_only_notice(
                    "Slow-block navigation",
                )));
            }
            VteInput::FilterPinnedBlocks => {
                let _ = sender.output(VteOutput::Notice(vte_block_mode_only_notice(
                    "Bookmarked-block navigation",
                )));
            }
            VteInput::ClearBlockFilter => {
                let _ = sender.output(VteOutput::Notice(vte_block_mode_only_notice(
                    "Block navigation",
                )));
            }
            VteInput::JumpToPrevPinned | VteInput::JumpToNextPinned => {
                let _ = sender.output(VteOutput::Notice(vte_block_mode_only_notice(
                    "Bookmarked-block navigation",
                )));
            }
            VteInput::JumpToPrevFailed | VteInput::JumpToNextFailed => {
                let _ = sender.output(VteOutput::Notice(vte_block_mode_only_notice(
                    "Failed-block navigation",
                )));
            }
            VteInput::CollapseAllBlocks
            | VteInput::ExpandAllBlocks
            | VteInput::CollapseSelectedBlocks
            | VteInput::ExpandSelectedBlocks
            | VteInput::ToggleBlockCollapsed => {
                let _ = sender.output(VteOutput::Notice(vte_block_mode_only_notice(
                    "Block folding",
                )));
            }
            VteInput::ExportSessionMarkdown | VteInput::ExportSessionJson => {
                let _ = sender.output(VteOutput::Notice(vte_block_mode_only_notice(
                    "Session export",
                )));
            }
            VteInput::SearchSet(query, use_regex, case_sensitive) => {
                self.search_status =
                    if let Some(status) = crate::search::oversize_query_status(&query) {
                        self.terminal.search_set_regex(None::<&vte4::Regex>, 0);
                        status
                    } else {
                        search_terminal(&self.terminal, &query, use_regex, case_sensitive)
                    };
                let _ = sender.output(VteOutput::SearchStatus(self.search_status.clone()));
            }
            VteInput::SearchNext => {
                let found = self.terminal.search_find_next();
                self.search_status = self.search_status.stepped(1, found);
                let _ = sender.output(VteOutput::SearchStatus(self.search_status.clone()));
            }
            VteInput::SearchPrev => {
                let found = self.terminal.search_find_previous();
                self.search_status = self.search_status.stepped(-1, found);
                let _ = sender.output(VteOutput::SearchStatus(self.search_status.clone()));
            }
            VteInput::SearchClear => {
                self.terminal.search_set_regex(None::<&vte4::Regex>, 0);
                self.search_status = SearchStatus::Idle;
                let _ = sender.output(VteOutput::SearchStatus(SearchStatus::Idle));
            }
            VteInput::CrossBlockSearch => {
                let _ = sender.output(VteOutput::Notice(vte_block_mode_only_notice(
                    "Cross-block search",
                )));
            }
            VteInput::CompareCommandOutput => {
                let _ = sender.output(VteOutput::Notice(vte_block_mode_only_notice(
                    "Command output comparison",
                )));
            }
            VteInput::PreviousCommand
            | VteInput::NextCommand
            | VteInput::ReturnToLive
            | VteInput::CopyCurrentCommand => {
                let _ = sender.output(VteOutput::Notice(
                    "Command navigation is available in Unified mode.".into(),
                ));
            }
            VteInput::AskAiAboutSelectedBlock => {
                let _ = sender.output(VteOutput::Notice(ask_ai_empty_notice().to_string()));
            }
        }
    }
}

pub(crate) use super::copy_empty_notice;

pub(crate) use super::ask_ai_empty_notice;

/// Block-document actions that still reach the live VTE through
/// `uses_term_view()` used to look like a dead shortcut.
pub(crate) fn vte_block_mode_only_notice(feature: &str) -> String {
    format!("{feature} is available only in a Block-mode pane.")
}

/// Install `query` as `terminal`'s native search, step to the first match
/// and count what the bounded snapshot can see. The query must already have
/// passed [`crate::search::oversize_query_status`].
pub(super) fn search_pcre2_flags(case_sensitive: bool) -> u32 {
    let mut flags = pcre2_sys::PCRE2_MULTILINE;
    if !case_sensitive {
        flags |= pcre2_sys::PCRE2_CASELESS;
    }
    flags
}

pub(super) fn search_terminal(
    terminal: &Terminal,
    query: &str,
    use_regex: bool,
    case_sensitive: bool,
) -> SearchStatus {
    let pattern = search_pattern(query, use_regex);
    match vte4::Regex::for_search(&pattern, search_pcre2_flags(case_sensitive)) {
        Ok(regex) => {
            terminal.search_set_regex(Some(&regex), 0);
            terminal.search_set_wrap_around(true);
            let found = terminal.search_find_next();
            search_status_for_vte(
                compile_count_regex(&pattern, case_sensitive).ok().as_ref(),
                terminal_search_snapshot(terminal),
                found,
                !use_regex,
            )
        }
        Err(error) => {
            terminal.search_set_regex(None::<&vte4::Regex>, 0);
            SearchStatus::Error(invalid_regex_message(error))
        }
    }
}

pub(super) use super::search::{compile_count_regex, search_pattern};

const VTE_SEARCH_SCAN_MAX_ROWS: i64 = 10_000;
const VTE_SEARCH_SCAN_MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_UTF8_BYTES_PER_CELL: usize = 4;

#[derive(Debug)]
struct TerminalSearchSnapshot {
    text: String,
    truncated: bool,
}

/// VTE exposes next/previous success but no total match count. Count only a
/// bounded suffix: at most 10k rows, with a conservative cell-derived 2 MiB
/// budget and a hard post-extraction byte cap. The returned `truncated` bit is
/// part of the user-visible result (`N+`), never hidden as an exact total.
fn terminal_search_snapshot(terminal: &Terminal) -> TerminalSearchSnapshot {
    let columns = terminal.column_count();
    if columns <= 0 {
        return TerminalSearchSnapshot {
            text: String::new(),
            truncated: false,
        };
    }
    let (_, cursor_row) = terminal.cursor_position();
    let (start_row, end_row) = terminal
        .vadjustment()
        .map_or((0, cursor_row), |adjustment| {
            let start = adjustment.lower().floor() as i64;
            let adjustment_end = (adjustment.upper().ceil() as i64).saturating_sub(1);
            (start.min(cursor_row), adjustment_end.max(cursor_row))
        });
    if end_row < start_row {
        return TerminalSearchSnapshot {
            text: String::new(),
            truncated: false,
        };
    }

    let columns_usize = usize::try_from(columns).unwrap_or(usize::MAX).max(1);
    let rows_from_cell_budget = VTE_SEARCH_SCAN_MAX_BYTES
        .saturating_sub(VTE_SEARCH_SCAN_MAX_ROWS as usize)
        .checked_div(columns_usize.saturating_mul(MAX_UTF8_BYTES_PER_CELL).max(1))
        .unwrap_or(0)
        .max(1);
    let row_budget = VTE_SEARCH_SCAN_MAX_ROWS.min(rows_from_cell_budget as i64);
    let scan_start = end_row
        .saturating_sub(row_budget.saturating_sub(1))
        .max(start_row);
    let rows_truncated = scan_start > start_row;
    let mut text = terminal
        .text_range_format(
            vte4::Format::Text,
            scan_start,
            0,
            end_row,
            columns.saturating_sub(1),
        )
        .0
        .map(|text| text.to_string())
        .unwrap_or_default();
    let bytes_truncated = text.len() > VTE_SEARCH_SCAN_MAX_BYTES;
    if bytes_truncated {
        let mut keep_from = text.len() - VTE_SEARCH_SCAN_MAX_BYTES;
        while !text.is_char_boundary(keep_from) {
            keep_from += 1;
        }
        text = text.split_off(keep_from);
    }
    TerminalSearchSnapshot {
        text,
        truncated: rows_truncated || bytes_truncated,
    }
}

fn search_status_for_vte(
    counter: Option<&regex::Regex>,
    snapshot: TerminalSearchSnapshot,
    native_found: bool,
    counter_matches_native_semantics: bool,
) -> SearchStatus {
    // Native VTE searched the complete scrollback, so failure is an exact zero
    // even when our count snapshot was bounded.
    if !native_found {
        return SearchStatus::results(0, 0);
    }
    let Some(counter) = counter else {
        // Legal PCRE2 outside Rust regex's grammar remains fully searchable.
        return SearchStatus::partial_results(1, 1);
    };
    let counted = counter.find_iter(&snapshot.text).count();
    if !counter_matches_native_semantics || snapshot.truncated || counted == 0 {
        SearchStatus::partial_results(1, counted.max(1))
    } else {
        SearchStatus::results(1, counted)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "requires DISPLAY; run explicitly under Xvfb"]
    fn explicit_focus_return_respects_reopened_search() {
        use gtk::prelude::*;
        use relm4::gtk;
        use std::time::{Duration, Instant};
        gtk::init().unwrap();
        let terminal = vte4::Terminal::new();
        let search = gtk::SearchEntry::new();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&terminal);
        root.append(&search);
        let window = gtk::Window::builder().child(&root).build();
        window.present();
        let context = gtk::glib::MainContext::default();
        let until = Instant::now() + Duration::from_millis(100);
        while Instant::now() < until {
            while context.iteration(false) {}
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(terminal.is_mapped());
        for transient_owner in [false, true] {
            // This is the production entry point used when closing Find.
            super::focus_terminal(&terminal);
            assert!(terminal.has_focus());
            assert!(search.grab_focus());
            if transient_owner {
                GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
            }
            while context.iteration(false) {}
            assert!(!terminal.has_focus(), "newer search ownership wins at idle");
            if !transient_owner {
                let owner = GtkWindowExt::focus(&window).unwrap();
                assert!(owner == *search.upcast_ref::<gtk::Widget>() || owner.is_ancestor(&search));
            }
        }
        // A temporary absence of focus without a newer owner still repairs.
        super::focus_terminal(&terminal);
        GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
        while context.iteration(false) {}
        assert!(terminal.has_focus());
        // Pending repair does not keep a removed terminal alive.
        super::focus_terminal(&terminal);
        let weak = terminal.downgrade();
        let weak_window = window.downgrade();
        // Release GTK's window focus ownership before testing the pending
        // callback's lifetime. Do not drain idle until after these assertions.
        GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
        assert!(GtkWindowExt::focus(&window).is_none());
        root.remove(&terminal);
        window.destroy();
        drop(terminal);
        drop(window);
        assert!(weak.upgrade().is_none());
        assert!(weak_window.upgrade().is_none());
        while context.iteration(false) {}
    }

    use super::{
        ask_ai_empty_notice, compile_count_regex, copy_empty_notice, launch_failure_message,
        search_pattern, search_pcre2_flags, search_status_for_vte, vte_block_mode_only_notice,
        InitialCommands, TerminalSearchSnapshot,
    };
    use crate::search::SearchStatus;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn copy_with_no_selection_explains_itself() {
        assert_eq!(copy_empty_notice(false), Some("Nothing selected to copy."));
        assert_eq!(copy_empty_notice(true), None);
    }

    #[test]
    fn ask_ai_without_a_finished_block_explains_itself() {
        assert!(ask_ai_empty_notice().contains("finished block"));
    }

    #[test]
    fn cross_block_search_on_live_vte_explains_block_mode() {
        assert_eq!(
            vte_block_mode_only_notice("Cross-block search"),
            "Cross-block search is available only in a Block-mode pane."
        );
    }

    #[test]
    fn session_export_on_live_vte_explains_block_mode() {
        assert_eq!(
            vte_block_mode_only_notice("Session export"),
            "Session export is available only in a Block-mode pane."
        );
    }

    #[test]
    fn block_folding_on_live_vte_explains_block_mode() {
        assert_eq!(
            vte_block_mode_only_notice("Block folding"),
            "Block folding is available only in a Block-mode pane."
        );
    }

    #[test]
    fn block_jump_on_live_vte_explains_block_mode() {
        assert_eq!(
            vte_block_mode_only_notice("Bookmarked-block navigation"),
            "Bookmarked-block navigation is available only in a Block-mode pane."
        );
        assert_eq!(
            vte_block_mode_only_notice("Failed-block navigation"),
            "Failed-block navigation is available only in a Block-mode pane."
        );
    }

    #[test]
    fn block_filter_on_live_vte_explains_block_mode() {
        assert_eq!(
            vte_block_mode_only_notice("Slow-block navigation"),
            "Slow-block navigation is available only in a Block-mode pane."
        );
        assert_eq!(
            vte_block_mode_only_notice("Block navigation"),
            "Block navigation is available only in a Block-mode pane."
        );
    }

    #[test]
    fn clear_blocks_on_live_vte_explains_block_mode() {
        assert_eq!(
            vte_block_mode_only_notice("Clearing finished blocks"),
            "Clearing finished blocks is available only in a Block-mode pane."
        );
        assert_eq!(
            vte_block_mode_only_notice("Restoring cleared blocks"),
            "Restoring cleared blocks is available only in a Block-mode pane."
        );
    }

    #[test]
    fn block_selection_on_live_vte_explains_block_mode() {
        assert_eq!(
            vte_block_mode_only_notice("Block selection"),
            "Block selection is available only in a Block-mode pane."
        );
        assert_eq!(
            vte_block_mode_only_notice("Deleting selected blocks"),
            "Deleting selected blocks is available only in a Block-mode pane."
        );
    }

    #[test]
    fn reinput_on_live_vte_explains_block_mode() {
        assert_eq!(
            vte_block_mode_only_notice("Selected-command recall"),
            "Selected-command recall is available only in a Block-mode pane."
        );
    }

    #[test]
    fn configured_commands_are_split_only_at_the_boundary() {
        let commands = InitialCommands::from_config(Some("cd /tmp, printf ready"));
        assert_eq!(
            commands.as_slice(),
            strings(&["cd /tmp", "printf ready"]).as_slice()
        );
    }

    #[test]
    fn search_query_compilation_is_literal_by_default_and_reports_bad_block_regex() {
        let literal = compile_count_regex(&search_pattern("a+b", false), false).unwrap();
        assert_eq!(literal.find_iter("A+B aab").count(), 1);

        let regex = compile_count_regex(&search_pattern("a+b", true), false).unwrap();
        assert_eq!(regex.find_iter("A+B aab").count(), 1);

        let error = compile_count_regex(&search_pattern("(", true), false).unwrap_err();
        assert!(error.starts_with("Invalid regex:"));
        assert!(!error.contains('\n'));

        let sensitive = compile_count_regex("Ab", true).unwrap();
        assert_eq!(sensitive.find_iter("ab AB Ab").count(), 1);
        let insensitive = compile_count_regex("Ab", false).unwrap();
        assert_eq!(insensitive.find_iter("ab AB Ab").count(), 3);
        assert_ne!(search_pcre2_flags(false) & pcre2_sys::PCRE2_CASELESS, 0);
        assert_eq!(search_pcre2_flags(true) & pcre2_sys::PCRE2_CASELESS, 0);
    }

    #[test]
    fn bounded_or_pcre_only_counts_are_explicitly_inexact() {
        let counter = compile_count_regex("hit", false).unwrap();
        assert_eq!(
            search_status_for_vte(
                Some(&counter),
                TerminalSearchSnapshot {
                    text: "hit hit".into(),
                    truncated: false,
                },
                true,
                true,
            ),
            SearchStatus::results(1, 2)
        );
        let partial = search_status_for_vte(
            Some(&counter),
            TerminalSearchSnapshot {
                text: "hit hit".into(),
                truncated: true,
            },
            true,
            true,
        );
        assert_eq!(partial, SearchStatus::partial_results(1, 2));

        // Look-ahead is valid PCRE2 but deliberately outside Rust regex. VTE
        // accepts it natively; lack of a mirror counter must not become an error.
        assert!(compile_count_regex("(?=hit)", false).is_err());
        assert!(vte4::Regex::for_search("(?=hit)", pcre2_sys::PCRE2_CASELESS).is_ok());
        assert_eq!(
            search_status_for_vte(
                None,
                TerminalSearchSnapshot {
                    text: "hit".into(),
                    truncated: false,
                },
                true,
                false,
            ),
            SearchStatus::partial_results(1, 1)
        );
        assert_eq!(
            search_status_for_vte(
                None,
                TerminalSearchSnapshot {
                    text: String::new(),
                    truncated: true,
                },
                false,
                false,
            ),
            SearchStatus::results(0, 0)
        );
    }

    #[test]
    fn regex_counts_are_partial_even_when_both_engines_compile() {
        let pattern = "[a&&a]";
        let counter = compile_count_regex(pattern, false).unwrap();
        assert!(vte4::Regex::for_search(
            pattern,
            pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
        )
        .is_ok());
        assert_eq!(
            search_status_for_vte(
                Some(&counter),
                TerminalSearchSnapshot {
                    text: "a".into(),
                    truncated: false,
                },
                true,
                false,
            ),
            SearchStatus::partial_results(1, 1)
        );
    }

    #[test]
    fn mirror_counter_uses_the_native_multiline_anchor_mode() {
        let counter = compile_count_regex("^hit$", false).unwrap();
        assert_eq!(counter.find_iter("miss\nhit\ntail").count(), 1);
    }

    #[test]
    fn restored_argv_is_always_one_command_even_when_arguments_contain_commas() {
        let argv = strings(&["ssh", "host", "printf '%s, %s' one two"]);
        let commands = InitialCommands::from_restored_argv(Some(&argv), &strings(&["bash"]));
        assert_eq!(commands.as_slice().len(), 1);
        assert_eq!(
            commands.as_slice()[0],
            "'ssh' 'host' 'printf '\"'\"'%s, %s'\"'\"' one two'"
        );
    }

    #[test]
    fn unsafe_restored_argv_is_not_replayed() {
        let argv = strings(&["ssh", "host", "echo first\necho second"]);
        assert!(
            InitialCommands::from_restored_argv(Some(&argv), &strings(&["bash"]))
                .as_slice()
                .is_empty()
        );
    }

    #[test]
    fn restored_argv_uses_powershell_call_syntax() {
        let argv = strings(&["ssh", "host", "printf 'safe'; one argument"]);
        let commands =
            InitialCommands::from_restored_argv(Some(&argv), &strings(&["/usr/bin/pwsh"]));
        assert_eq!(
            commands.as_slice(),
            strings(&["& 'ssh' 'host' 'printf ''safe''; one argument'"]).as_slice()
        );
    }

    #[test]
    fn launch_failure_message_includes_the_underlying_io_error_and_recovery_hint() {
        let error = std::io::Error::new(std::io::ErrorKind::NotFound, "flatpak-spawn missing");
        let message = launch_failure_message(&error);

        assert_eq!(
            message,
            "Terminal failed to start: flatpak-spawn missing. \
             Check the shell, remote command, or host bridge."
        );
    }

    /// A Find while an alternate-screen app owns a Block pane searches the
    /// live terminal's own screen with this helper; the text there is what the
    /// user sees, not the hidden cards.
    #[test]
    #[ignore = "requires DISPLAY"]
    fn live_screen_search_finds_text_on_the_alternate_screen() {
        use gtk::glib;
        use gtk::prelude::*;
        use relm4::gtk;
        use std::time::{Duration, Instant};
        use vte4::{Terminal, TerminalExt};

        gtk::init().expect("gtk init");
        let terminal = Terminal::new();
        terminal.set_size(40, 6);
        let window = gtk::Window::new();
        window.set_child(Some(&terminal));
        window.present();
        terminal.feed(b"primary-only\r\n\x1b[?1049h\x1b[H\x1b[2Jclaude transcript: needle here");
        let context = glib::MainContext::default();
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(100) {
            while context.iteration(false) {}
            std::thread::sleep(Duration::from_millis(2));
        }

        assert_eq!(
            super::search_terminal(&terminal, "NEEDLE", false, false),
            SearchStatus::results(1, 1)
        );
        let selected = terminal
            .text_selected(vte4::Format::Text)
            .map(|text| text.to_string())
            .unwrap_or_default();
        assert_eq!(selected, "needle");
        assert!(matches!(
            super::search_terminal(&terminal, "(", true, false),
            SearchStatus::Error(_)
        ));
        window.close();
        while context.iteration(false) {}
    }
}
