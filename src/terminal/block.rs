//! Relm4 adapter around the forge block view.
//!
//! anvil's application shell expects terminal backends to be Relm4
//! components that speak `VteInit`/`VteInput`/`VteOutput`. The block-mode
//! implementation itself is now the forge `block_view::TermView`; this file
//! only adapts that GTK view to the existing anvil component surface.

use gtk::pango::FontDescription;
use gtk::prelude::*;
use relm4::gtk;
use relm4::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;
use vte4::prelude::TerminalExt;

use crate::block_view::{
    FindNavigationResult, FindProgress, FindSearchResult, RecordNavigationResult, TermView,
};
use crate::search::SearchStatus;

use super::{cross_block_search, record_snapshot};

pub use super::vte::{VteInit, VteInput, VteOutput};

/// Inactive-tab styling for a finished command: failures get the bell style,
/// everything else the lighter activity style.
///
/// Only a status the shell actually reported can be a failure. `None` — a shell
/// that emits bare OSC 133 marks — used to arrive here as `0` and be styled as a
/// success; it is now styled as plain activity, which claims neither outcome.
fn command_finished_output(exit_code: Option<i32>) -> VteOutput {
    VteOutput::CommandFinished(exit_code.is_none_or(|code| code == 0))
}

/// Toast text for a session-export attempt.
fn export_notice(result: std::io::Result<std::path::PathBuf>) -> String {
    match result {
        Ok(path) => format!("Session exported to {}", path.display()),
        Err(err) => format!("Session export failed: {err}"),
    }
}

fn record_navigation_notice(result: RecordNavigationResult) -> Option<&'static str> {
    match result {
        RecordNavigationResult::LocationUnavailable => {
            Some("This record has no exact terminal location and no retained output snapshot.")
        }
        // SnapshotView opens the read-only view instead of reporting anything.
        RecordNavigationResult::Navigated
        | RecordNavigationResult::NoMatchingRecord
        | RecordNavigationResult::SnapshotView { .. } => None,
    }
}

fn report_record_navigation(
    sender: &ComponentSender<BlockTerminal>,
    view: &Rc<TermView>,
    dialog_slot: &Rc<RefCell<Option<relm4::adw::Dialog>>>,
    result: RecordNavigationResult,
) {
    let notice = match result {
        RecordNavigationResult::SnapshotView { record_id } => {
            record_snapshot::present(view, dialog_slot, record_id)
        }
        result => record_navigation_notice(result),
    };
    if let Some(message) = notice {
        let _ = sender.output(VteOutput::Notice(message.to_string()));
    }
}

pub struct BlockTerminal {
    view: Option<Rc<TermView>>,
    mode: crate::config::TerminalMode,
    /// Block PTY construction runs synchronously inside Relm4 component init.
    /// Split preparation can inspect this after `launch()` and avoid committing
    /// an error page into the pane tree.
    launch_error: Option<String>,
    terminal_done: Rc<Cell<bool>>,
    config: Rc<RefCell<crate::config::Config>>,
    cross_block_search_dialog: Rc<RefCell<Option<relm4::adw::Dialog>>>,
    /// Pane-lifetime search intent only; never serialized into config or a
    /// restored session.
    cross_block_search_memory: Rc<RefCell<cross_block_search::Memory>>,
    /// The read-only snapshot view opened by record navigation. One slot per
    /// pane, so a second navigation replaces the open view instead of stacking.
    record_snapshot_dialog: Rc<RefCell<Option<relm4::adw::Dialog>>>,
    /// Last result belongs to the regex currently installed in this pane.
    /// Keeping it backend-side prevents navigation after a compile error from
    /// turning that diagnostic into a misleading `(0, 0)` result.
    search_status: SearchStatus,
    /// Query that produced `search_status`. A card resize/filter/expand can
    /// invalidate VTE's native cursor between Next/Previous actions; retaining
    /// the query lets the adapter rebuild the pass instead of claiming the
    /// still-visible text disappeared.
    search_query: Option<(String, bool)>,
}

fn validate_block_search_pattern(pattern: &str) -> Result<(), String> {
    super::vte::compile_count_regex(pattern)?;
    vte4::Regex::for_search(
        pattern,
        pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
    )
    .map(|_| ())
    .map_err(crate::search::invalid_regex_message)
}

fn progress_status(progress: FindProgress, use_regex: bool) -> SearchStatus {
    if use_regex || progress.capped || progress.scan_limited {
        // Match enumeration uses Rust regex while painting/cursor movement is
        // native PCRE2. Even when both compile their semantics can differ, so
        // never present a regex total as exact.
        SearchStatus::partial_results(progress.current, progress.total)
    } else {
        SearchStatus::results(progress.current, progress.total)
    }
}

fn block_result_status(result: FindSearchResult, use_regex: bool) -> SearchStatus {
    match result {
        FindSearchResult::NoMatches => SearchStatus::results(0, 0),
        FindSearchResult::InvalidRegex => SearchStatus::Error("Invalid regex".to_string()),
        FindSearchResult::ScanLimit => SearchStatus::partial_results(0, 0),
        FindSearchResult::Matches(progress) => progress_status(progress, use_regex),
    }
}

fn block_navigation_status(
    previous: &SearchStatus,
    result: FindNavigationResult,
    use_regex: bool,
) -> SearchStatus {
    match result {
        FindNavigationResult::Progress(progress) => progress_status(progress, use_regex),
        FindNavigationResult::Invalidated => SearchStatus::results(0, 0),
        FindNavigationResult::Inactive => previous.clone(),
    }
}

impl BlockTerminal {
    pub(crate) fn mode(&self) -> crate::config::TerminalMode {
        self.mode
    }

    pub(crate) fn term_view(&self) -> Option<Rc<TermView>> {
        self.view.clone()
    }

    pub(crate) fn launch_error(&self) -> Option<&str> {
        self.launch_error.as_deref()
    }

    fn terminate_once(&self) {
        if !self.terminal_done.replace(true) {
            if let Some(view) = self.view.as_ref() {
                view.kill();
            }
        }
    }

    pub(crate) fn can_accept_agent_command(&self) -> bool {
        self.view
            .as_ref()
            .is_some_and(|view| view.can_accept_agent_command())
    }

    pub(crate) fn command_prompt_status(&self) -> crate::block_view::CommandPromptStatus {
        self.view.as_ref().map_or(
            crate::block_view::CommandPromptStatus::ShellIntegrationUnavailable,
            |view| view.command_prompt_status(),
        )
    }

    pub(crate) fn agent_command_prompt_status(&self) -> crate::block_view::CommandPromptStatus {
        self.view.as_ref().map_or(
            crate::block_view::CommandPromptStatus::ShellIntegrationUnavailable,
            |view| view.agent_command_prompt_status(),
        )
    }

    pub(crate) fn selected_block_context(
        &self,
        max_output_lines: usize,
    ) -> Option<crate::ai::BlockContext> {
        self.view
            .as_ref()
            .and_then(|view| view.selected_block_context(max_output_lines))
    }

    pub(crate) fn insert_inline_notice(&self, widget: &gtk::Widget) -> bool {
        let Some(view) = self.view.as_ref() else {
            return false;
        };
        view.insert_inline_notice(widget)
    }

    pub(crate) fn supports_inline_notices(&self) -> bool {
        self.view
            .as_ref()
            .is_some_and(|view| view.supports_inline_notices())
    }

    pub(crate) fn remove_inline_notice(&self, widget: &gtk::Widget) {
        if let Some(view) = self.view.as_ref() {
            view.remove_inline_notice(widget);
        }
    }

    pub(crate) fn try_insert_agent_command(&self, command: &str) -> bool {
        self.view
            .as_ref()
            .is_some_and(|view| view.try_insert_agent_command(command))
    }

    pub(crate) fn try_run_review_command(&self, command: &str) -> bool {
        self.view
            .as_ref()
            .is_some_and(|view| view.try_run_review_command(command))
    }

    /// Grid size (cols, rows) of the live VTE; `None` when the PTY failed to
    /// start and there is no view.
    pub(crate) fn grid_size(&self) -> Option<(i64, i64)> {
        self.view.as_ref().map(|view| view.grid_size())
    }

    pub(crate) fn debug_info(&self) -> crate::block_view::DebugInfo {
        self.view.as_ref().map_or_else(
            || {
                vec![(
                    "PTY",
                    vec![("Status".to_string(), "failed to start".to_string())],
                )]
            },
            |view| view.debug_info(),
        )
    }
}

fn launch_error_widget(error: &std::io::Error) -> gtk::Widget {
    let status = gtk::Box::new(gtk::Orientation::Vertical, 8);
    status.set_hexpand(true);
    status.set_vexpand(true);
    status.set_halign(gtk::Align::Center);
    status.set_valign(gtk::Align::Center);
    status.set_margin_start(24);
    status.set_margin_end(24);
    status.set_margin_top(24);
    status.set_margin_bottom(24);

    let title = gtk::Label::new(Some("Terminal failed to start"));
    title.add_css_class("title-2");
    let detail = gtk::Label::new(Some(&error.to_string()));
    detail.set_wrap(true);
    detail.set_selectable(true);
    detail.add_css_class("dim-label");
    let hint = gtk::Label::new(Some(
        "Check the configured shell, remote command, or host bridge; then close this pane and try again.",
    ));
    hint.set_wrap(true);

    status.append(&title);
    status.append(&detail);
    status.append(&hint);
    status.upcast()
}

fn connect_root_focus(root: &gtk::Widget, sender: &ComponentSender<BlockTerminal>) {
    // Track focus at the component root so both the live terminal and a launch
    // error page update the owning split before pane-scoped actions.
    let focus = gtk::EventControllerFocus::new();
    focus.connect_enter({
        let sender = sender.clone();
        move |_| {
            let _ = sender.output(VteOutput::Focused);
        }
    });
    root.add_controller(focus);
    let click = gtk::GestureClick::new();
    click.set_button(0);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed({
        let sender = sender.clone();
        move |_, _, _, _| {
            let _ = sender.output(VteOutput::Focused);
        }
    });
    root.add_controller(click);
}

fn connect_view_outputs(
    view: &Rc<TermView>,
    sender: &ComponentSender<BlockTerminal>,
    terminal_done: &Rc<Cell<bool>>,
) {
    view.connect_cwd_changed({
        let sender = sender.clone();
        move |cwd, external| {
            let _ = sender.output(VteOutput::CwdChanged {
                path: cwd.to_string(),
                external,
            });
        }
    });
    view.connect_remote_session_id({
        let sender = sender.clone();
        move |id| {
            let _ = sender.output(VteOutput::RemoteSessionId(id.to_string()));
        }
    });
    view.connect_exited({
        let sender = sender.clone();
        let terminal_done = terminal_done.clone();
        move |code| {
            terminal_done.set(true);
            let _ = sender.output(VteOutput::Exited(code));
        }
    });
    view.connect_bell({
        let sender = sender.clone();
        move || {
            let _ = sender.output(VteOutput::Bell);
        }
    });
    view.connect_title_changed({
        let sender = sender.clone();
        move |title| {
            let _ = sender.output(VteOutput::TitleChanged(title.to_string()));
        }
    });
    // A live block can repaint many times a second (spinners/progress bars).
    // Coalesce those repaints before they enter Relm4's application queue.
    let activity_pending = Rc::new(Cell::new(false));
    view.connect_activity({
        let sender = sender.clone();
        let activity_pending = activity_pending.clone();
        move || {
            if activity_pending.replace(true) {
                return;
            }
            let _ = sender.output(VteOutput::Activity);
            let activity_pending = activity_pending.clone();
            gtk::glib::timeout_add_local_once(Duration::from_millis(100), move || {
                activity_pending.set(false);
            });
        }
    });
    view.connect_agent_execution_lost({
        let sender = sender.clone();
        move |execution, reason| {
            log::warn!("Agent execution lost terminal correlation: {reason}");
            let _ = sender.output(VteOutput::AgentExecutionStartFailed { execution });
        }
    });
    let supports_correction_output = view.supports_inline_notices();
    view.connect_block_finished_with_output_if(
        move |agent_execution| supports_correction_output || agent_execution.is_some(),
        {
            let sender = sender.clone();
            move |command, exit_code, output_sample, agent_execution, duration_ms| {
                let _ = sender.output(command_finished_output(exit_code));
                let (exit_code, unknown_note) =
                    crate::block_view::exit_code_for_shared_surface(exit_code);
                let output_sample = match (unknown_note, output_sample) {
                    (Some(note), Some(output)) => Some(format!("{note}\n{output}")),
                    (Some(note), None) => Some(note.to_string()),
                    (None, output) => output,
                };
                let _ = sender.output(VteOutput::BlockFinished {
                    command,
                    // The agent transcript still speaks one i32; -1 plus the
                    // note above is explicitly unknown, never fabricated 0.
                    exit_code,
                    output_sample: output_sample.unwrap_or_default(),
                    agent_execution,
                    duration_ms,
                });
            }
        },
    );
    view.connect_ask_ai_about_block({
        let sender = sender.clone();
        move |context| {
            let _ = sender.output(VteOutput::AskAiAboutBlock(context));
        }
    });
}

impl Component for BlockTerminal {
    type Init = VteInit;
    type Input = VteInput;
    type Output = VteOutput;
    type CommandOutput = ();
    type Root = gtk::Widget;
    type Widgets = ();

    fn init_root() -> Self::Root {
        gtk::Box::new(gtk::Orientation::Vertical, 0).upcast()
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let config = init.config.clone();
        let terminal_done = Rc::new(Cell::new(false));
        connect_root_focus(&root, &sender);
        let view = {
            let config = config.borrow();
            TermView::new(
                &config,
                &init.mode,
                init.shell_argv.as_ref().as_slice(),
                init.working_directory.as_deref(),
                init.working_directory_external,
                init.session_id.as_deref(),
                &init.cwd_token,
                init.initial_commands.as_slice(),
            )
        }
        .map(Rc::new);

        let mut launch_error = None;
        let view = match view {
            Ok(view) => {
                init.probe.shell_pid.set(view.pid_i32());
                init.probe.pty_fd.set(view.pty_fd_i32());
                connect_view_outputs(&view, &sender, &terminal_done);
                // Needs the `Rc` the component holds: the menu's items call
                // back into the view, and the gesture lives on a widget the
                // view itself owns.
                TermView::install_canvas_context_menu(&view);
                TermView::arm_shell_integration_notice(&view, init.shell_argv.as_ref());
                if let Some(container) = root.downcast_ref::<gtk::Box>() {
                    container.append(&view.widget());
                }
                Some(view)
            }
            Err(error) => {
                terminal_done.set(true);
                log::error!("Block terminal failed to start: {error}");
                launch_error = Some(error.to_string());
                root.set_focusable(true);
                root.set_focus_on_click(true);
                if let Some(container) = root.downcast_ref::<gtk::Box>() {
                    container.append(&launch_error_widget(&error));
                }
                let sender = sender.clone();
                gtk::glib::idle_add_local_once(move || {
                    let _ = sender.output(VteOutput::LaunchFailed(
                        "Terminal failed to start; check the shell, remote command, or host bridge."
                            .to_string(),
                    ));
                });
                None
            }
        };

        let model = BlockTerminal {
            view,
            mode: init.mode,
            launch_error,
            terminal_done,
            config,
            cross_block_search_dialog: Rc::new(RefCell::new(None)),
            cross_block_search_memory: Rc::new(RefCell::new(Default::default())),
            record_snapshot_dialog: Rc::new(RefCell::new(None)),
            search_status: SearchStatus::Idle,
            search_query: None,
        };
        ComponentParts { model, widgets: () }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, root: &Self::Root) {
        if self.view.is_none() {
            if matches!(&msg, VteInput::GrabFocus) {
                root.grab_focus();
            }
            if let VteInput::RunAgentCommand { execution, .. } = &msg {
                let _ = sender.output(VteOutput::AgentExecutionStartFailed {
                    execution: *execution,
                });
            }
            match &msg {
                VteInput::SearchSet(..) | VteInput::SearchNext | VteInput::SearchPrev => {
                    self.search_status =
                        SearchStatus::Error("Terminal is unavailable.".to_string());
                    let _ = sender.output(VteOutput::SearchStatus(self.search_status.clone()));
                }
                VteInput::SearchClear => {
                    self.search_status = SearchStatus::Idle;
                    self.search_query = None;
                    let _ = sender.output(VteOutput::SearchStatus(self.search_status.clone()));
                }
                _ => {}
            }
            return;
        }
        let snapshot_dialog = self.record_snapshot_dialog.clone();
        let Some(view) = self.view.as_ref() else {
            return;
        };
        match msg {
            VteInput::WriteInput(data) => view.write_input(&data),
            VteInput::RunAgentCommand { execution, command } => {
                if !view.try_run_agent_command(execution, &command) {
                    let _ = sender.output(VteOutput::AgentExecutionStartFailed { execution });
                }
            }
            VteInput::Resize(cols, rows) => view.resize(cols, rows),
            VteInput::GrabFocus => view.grab_focus(),
            VteInput::Copy => view.copy_to_clipboard(),
            VteInput::CopyOutputOnly => view.copy_to_clipboard_with_modifier(true),
            VteInput::Paste => view.paste_from_clipboard(),
            VteInput::SetFontScale(scale) => view.set_font_scale(scale),
            VteInput::SetFont(desc) => {
                let font = FontDescription::from_string(&desc);
                view.set_font(&font);
            }
            VteInput::SetScrollback(lines) => view.vte().set_scrollback_lines(lines),
            VteInput::ScrollLines(lines) => view.scroll_lines(lines),
            VteInput::ApplyTheme => view.apply_theme(),
            VteInput::SyncConfig => view.reload_config(&self.config.borrow()),
            VteInput::Kill => self.terminate_once(),
            VteInput::FilterFailedBlocks => report_record_navigation(
                &sender,
                view,
                &snapshot_dialog,
                view.apply_failed_filter(),
            ),
            VteInput::FilterSlowBlocks => {
                report_record_navigation(&sender, view, &snapshot_dialog, view.apply_slow_filter())
            }
            VteInput::FilterPinnedBlocks => view.apply_pinned_filter(),
            VteInput::ClearBlockFilter => view.clear_block_filter(),
            VteInput::SelectAllBlocks => view.select_all_blocks(),
            VteInput::ClearBlocks => {
                let cleared = view.clear_blocks();
                if cleared > 0 {
                    let plural = if cleared == 1 { "" } else { "s" };
                    // The recovery is one button, not a palette search: an
                    // accidental Ctrl+Shift+K is exactly the moment nobody wants
                    // to go looking for the name of the action that undoes it.
                    let _ = sender.output(VteOutput::NoticeWithUndo {
                        message: format!("Cleared {cleared} block{plural}."),
                        button: "Undo".to_string(),
                        undo: crate::terminal::NoticeUndo::ClearBlocks,
                    });
                } else if view.is_unified() {
                    let _ = sender.output(VteOutput::Notice(
                        "Unified mode keeps no blocks to clear — use the shell's own clear."
                            .to_string(),
                    ));
                }
            }
            VteInput::UndoClearBlocks => {
                let restored = view.undo_clear_blocks();
                let message = if restored > 0 {
                    let plural = if restored == 1 { "" } else { "s" };
                    format!("Restored {restored} cleared block{plural}.")
                } else {
                    "No cleared blocks to restore.".to_string()
                };
                let _ = sender.output(VteOutput::Notice(message));
            }
            VteInput::ReinputSelectedCommands => view.reinput_selected_commands(),
            VteInput::JumpToPrevPinned => view.jump_to_pinned(-1),
            VteInput::JumpToNextPinned => view.jump_to_pinned(1),
            VteInput::JumpToPrevFailed => {
                report_record_navigation(&sender, view, &snapshot_dialog, view.jump_to_failed(-1))
            }
            VteInput::JumpToNextFailed => {
                report_record_navigation(&sender, view, &snapshot_dialog, view.jump_to_failed(1))
            }
            VteInput::ExportSessionMarkdown => {
                let message = export_notice(
                    view.export_session_to_file(crate::block_view::SessionExportFormat::Markdown),
                );
                let _ = sender.output(VteOutput::Notice(message));
            }
            VteInput::ExportSessionJson => {
                let message = export_notice(
                    view.export_session_to_file(crate::block_view::SessionExportFormat::Json),
                );
                let _ = sender.output(VteOutput::Notice(message));
            }
            VteInput::SearchSet(query, use_regex) => {
                self.search_query = Some((query.clone(), use_regex));
                let pattern = super::vte::search_pattern(&query, use_regex);
                self.search_status = match validate_block_search_pattern(&pattern) {
                    Ok(_) => block_result_status(view.find_in_blocks(&query, use_regex), use_regex),
                    Err(error) => {
                        view.clear_find();
                        SearchStatus::Error(error)
                    }
                };
                let _ = sender.output(VteOutput::SearchStatus(self.search_status.clone()));
            }
            VteInput::SearchNext => {
                let active = matches!(
                    self.search_status,
                    SearchStatus::Results { total, .. } if total > 0
                );
                let partial = matches!(
                    self.search_status,
                    SearchStatus::Results {
                        truncated: true,
                        ..
                    }
                );
                let result = if active {
                    view.find_next()
                } else {
                    FindNavigationResult::Inactive
                };
                self.search_status = if result == FindNavigationResult::Invalidated {
                    self.search_query.as_ref().map_or_else(
                        || SearchStatus::results(0, 0),
                        |(query, use_regex)| {
                            block_result_status(view.find_in_blocks(query, *use_regex), *use_regex)
                        },
                    )
                } else {
                    block_navigation_status(&self.search_status, result, partial)
                };
                let _ = sender.output(VteOutput::SearchStatus(self.search_status.clone()));
            }
            VteInput::SearchPrev => {
                let active = matches!(
                    self.search_status,
                    SearchStatus::Results { total, .. } if total > 0
                );
                let partial = matches!(
                    self.search_status,
                    SearchStatus::Results {
                        truncated: true,
                        ..
                    }
                );
                let result = if active {
                    view.find_prev()
                } else {
                    FindNavigationResult::Inactive
                };
                self.search_status = if result == FindNavigationResult::Invalidated {
                    self.search_query.as_ref().map_or_else(
                        || SearchStatus::results(0, 0),
                        |(query, use_regex)| {
                            block_result_status(view.find_in_blocks(query, *use_regex), *use_regex)
                        },
                    )
                } else {
                    block_navigation_status(&self.search_status, result, partial)
                };
                let _ = sender.output(VteOutput::SearchStatus(self.search_status.clone()));
            }
            VteInput::SearchClear => {
                view.clear_find();
                self.search_status = SearchStatus::Idle;
                self.search_query = None;
                let _ = sender.output(VteOutput::SearchStatus(self.search_status.clone()));
            }
            VteInput::CrossBlockSearch => cross_block_search::toggle(
                view.clone(),
                self.cross_block_search_dialog.clone(),
                snapshot_dialog,
                self.cross_block_search_memory.clone(),
            ),
            VteInput::AskAiAboutSelectedBlock => {
                if let Some(context) = view.selected_block_context(80) {
                    let _ = sender.output(VteOutput::AskAiAboutBlock(context));
                }
            }
        }
    }

    fn shutdown(&mut self, _widgets: &mut Self::Widgets, _output: relm4::Sender<Self::Output>) {
        self.terminate_once();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        block_navigation_status, block_result_status, command_finished_output,
        record_navigation_notice, validate_block_search_pattern, FindNavigationResult,
        FindProgress, FindSearchResult, RecordNavigationResult, VteOutput,
    };
    use crate::search::SearchStatus;

    #[test]
    fn block_exit_status_maps_to_command_finished_success() {
        assert!(matches!(
            command_finished_output(Some(0)),
            VteOutput::CommandFinished(true)
        ));
        assert!(matches!(
            command_finished_output(Some(1)),
            VteOutput::CommandFinished(false)
        ));
        assert!(matches!(
            command_finished_output(Some(-1)),
            VteOutput::CommandFinished(false)
        ));
        // New case: the shell reported no status. Styling the tab as a failure
        // would claim an outcome nothing observed, so it reads as activity.
        assert!(matches!(
            command_finished_output(None),
            VteOutput::CommandFinished(true)
        ));
    }

    #[test]
    fn invalid_search_stays_an_error_when_navigation_is_requested() {
        let error = SearchStatus::Error("Invalid regex: missing closing parenthesis".into());
        assert_eq!(
            block_navigation_status(&error, FindNavigationResult::Inactive, false),
            error
        );
    }

    #[test]
    fn unavailable_unified_record_location_has_explicit_feedback() {
        assert!(
            record_navigation_notice(RecordNavigationResult::LocationUnavailable)
                .is_some_and(|message| message.contains("no retained output snapshot"))
        );
        assert_eq!(
            record_navigation_notice(RecordNavigationResult::Navigated),
            None
        );
        assert_eq!(
            record_navigation_notice(RecordNavigationResult::NoMatchingRecord),
            None
        );
        assert_eq!(
            record_navigation_notice(RecordNavigationResult::SnapshotView { record_id: 7 }),
            None,
            "the snapshot view, not a notice, answers this result"
        );
    }

    #[test]
    fn block_regex_counts_remain_explicitly_approximate_while_literals_are_exact() {
        assert_eq!(
            block_result_status(
                FindSearchResult::Matches(FindProgress {
                    current: 1,
                    total: 3,
                    capped: false,
                    scan_limited: false,
                }),
                true,
            ),
            SearchStatus::partial_results(1, 3)
        );
        assert_eq!(
            block_result_status(
                FindSearchResult::Matches(FindProgress {
                    current: 1,
                    total: 3,
                    capped: false,
                    scan_limited: false,
                }),
                false,
            ),
            SearchStatus::results(1, 3)
        );
        assert_eq!(
            block_result_status(FindSearchResult::NoMatches, true),
            SearchStatus::results(0, 0)
        );
    }

    #[test]
    fn block_search_validates_the_native_pcre_pattern_too() {
        // Rust regex permits locally disabling Unicode for an ASCII-only
        // subexpression; PCRE2 does not recognize that inline `u` flag.
        assert!(super::super::vte::compile_count_regex("(?-u:a)").is_ok());
        let error = validate_block_search_pattern("(?-u:a)").unwrap_err();
        assert!(error.starts_with("Invalid regex:"));
    }
}
