//! Relm4 adapter around the jterm4 block view.
//!
//! jterm1's application shell expects terminal backends to be Relm4
//! components that speak `VteInit`/`VteInput`/`VteOutput`. The block-mode
//! implementation itself is now the jterm4 `block_view::TermView`; this file
//! only adapts that GTK view to the existing jterm1 component surface.

use gtk::pango::FontDescription;
use gtk::prelude::*;
use relm4::gtk;
use relm4::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;
use vte4::prelude::TerminalExt;

use crate::block_view::TermView;

use super::cross_block_search;

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

pub struct BlockTerminal {
    view: Option<Rc<TermView>>,
    terminal_done: Rc<Cell<bool>>,
    config: Rc<RefCell<crate::config::Config>>,
    cross_block_search_dialog: Rc<RefCell<Option<relm4::adw::Dialog>>>,
}

impl BlockTerminal {
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
    view.connect_block_finished({
        let sender = sender.clone();
        move |command, exit_code, output_sample, agent_generation, duration_ms| {
            let _ = sender.output(command_finished_output(exit_code));
            let _ = sender.output(VteOutput::BlockFinished {
                command,
                // The agent transcript this feeds still speaks one i32.
                exit_code: crate::block_view::exit_code_for_i32_api(exit_code),
                output_sample,
                agent_generation,
                duration_ms,
            });
        }
    });
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
                init.shell_argv.as_ref().as_slice(),
                init.working_directory.as_deref(),
                init.working_directory_external,
                init.session_id.as_deref(),
                &init.cwd_token,
                init.initial_commands.as_slice(),
            )
        }
        .map(Rc::new);

        let view = match view {
            Ok(view) => {
                init.probe.shell_pid.set(view.pid_i32());
                init.probe.pty_fd.set(view.pty_fd_i32());
                connect_view_outputs(&view, &sender, &terminal_done);
                if let Some(container) = root.downcast_ref::<gtk::Box>() {
                    container.append(&view.widget());
                }
                Some(view)
            }
            Err(error) => {
                terminal_done.set(true);
                log::error!("Block terminal failed to start: {error}");
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
            terminal_done,
            config,
            cross_block_search_dialog: Rc::new(RefCell::new(None)),
        };
        ComponentParts { model, widgets: () }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, root: &Self::Root) {
        if self.view.is_none() {
            if matches!(&msg, VteInput::GrabFocus) {
                root.grab_focus();
            }
            if let VteInput::RunAgentCommand { generation, .. } = &msg {
                let _ = sender.output(VteOutput::AgentExecutionStartFailed {
                    generation: *generation,
                });
            }
            return;
        }
        let Some(view) = self.view.as_ref() else {
            return;
        };
        match msg {
            VteInput::WriteInput(data) => view.write_input(&data),
            VteInput::RunAgentCommand {
                generation,
                command,
            } => {
                if !view.try_run_agent_command(generation, &command) {
                    let _ = sender.output(VteOutput::AgentExecutionStartFailed { generation });
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
            VteInput::FilterFailedBlocks => view.apply_failed_filter(),
            VteInput::FilterSlowBlocks => view.apply_slow_filter(),
            VteInput::FilterPinnedBlocks => view.apply_pinned_filter(),
            VteInput::ClearBlockFilter => view.clear_block_filter(),
            VteInput::SelectAllBlocks => view.select_all_blocks(),
            VteInput::ClearBlocks => {
                let cleared = view.clear_blocks();
                if cleared > 0 {
                    let plural = if cleared == 1 { "" } else { "s" };
                    let _ = sender.output(VteOutput::Notice(format!(
                        "Cleared {cleared} block{plural} — \"Undo clear blocks\" restores them."
                    )));
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
            VteInput::JumpToPrevFailed => view.jump_to_failed(-1),
            VteInput::JumpToNextFailed => view.jump_to_failed(1),
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
                let _ = view.find_in_blocks(&query, use_regex);
            }
            VteInput::SearchNext => {
                let _ = view.find_next();
            }
            VteInput::SearchPrev => {
                let _ = view.find_prev();
            }
            VteInput::SearchClear => view.clear_find(),
            VteInput::CrossBlockSearch => {
                cross_block_search::toggle(view.clone(), self.cross_block_search_dialog.clone())
            }
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
    use super::{command_finished_output, VteOutput};

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
}
