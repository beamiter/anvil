#[cfg(target_os = "linux")]
use jterm_gtk_view::RuntimeShell;
use jterm_gtk_view::{LinkActions, Pty, TerminalPane, Theme};
use relm4::gtk::{self, prelude::*};
use std::{cell::RefCell, rc::Rc};

use crate::{config::Config, pty::OwnedPty, terminal::VteInput};

#[cfg(target_os = "linux")]
const PANE_DATA: &str = "anvil-shared-panes-vim-source";

impl Pty for OwnedPty {
    fn write(&self, bytes: &[u8]) -> Result<(), String> {
        self.try_write_bytes(bytes)
            .map_err(|error| error.to_string())
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        self.try_resize(cols, rows)
            .map_err(|error| error.to_string())
    }

    fn stop(&self) {
        self.kill();
    }

    fn start_reader(
        &self,
        output: impl FnMut(Vec<u8>) + 'static,
        exited: impl FnOnce(jterm_core::exit_status::PtyExit) + 'static,
    ) -> Result<(), String> {
        OwnedPty::start_reader(self, output, exited).map_err(|error| error.to_string())
    }
}

pub(crate) struct SharedTerminal {
    pub(crate) widget: gtk::Widget,
    pane: Option<Rc<TerminalPane<OwnedPty>>>,
    config: Rc<RefCell<Config>>,
    standalone: bool,
    launch_error: Option<String>,
    #[cfg(target_os = "linux")]
    runtime_guard: Option<std::sync::Arc<crate::gtk_process::Guard>>,
}

impl Drop for SharedTerminal {
    fn drop(&mut self) {
        // The window retains the runtime for reaping after the pane is gone.
        if let Some(pane) = &self.pane {
            pane.stop();
        }
    }
}

impl SharedTerminal {
    pub(crate) fn open(
        config: Rc<RefCell<Config>>,
        argv: &[String],
        cwd: Option<&str>,
        close: impl Fn(&gtk::Widget) + 'static,
    ) -> Result<Self, String> {
        let theme = Self::theme(&config.borrow());
        let links = Self::links();
        #[cfg(target_os = "linux")]
        let mut runtime_guard = None;
        let status = jterm_core::review_input::safe_inline_display;
        let legacy = |theme, links, close| {
            let argv: Vec<_> = argv.iter().map(String::as_str).collect();
            let pty = OwnedPty::spawn(&argv, cwd, &jterm_gtk_view::CHILD_ENV)
                .map_err(|error| error.to_string())?;
            TerminalPane::new(pty, theme, status, links, close)
        };
        let (widget, pane) = {
            #[cfg(target_os = "linux")]
            {
                let host_bridge = crate::host::is_flatpak();
                let runtime = match RuntimeShell::spawn_anvil_bash_candidate(
                    argv,
                    cwd,
                    host_bridge,
                    crate::host::working_directory_available,
                    include_bytes!("../scripts/shell-integration/anvil.bash"),
                    || {
                        Self::runtime_environment(
                            argv,
                            config.borrow().vim9.auto_gui,
                            &jterm_gtk_view::CHILD_ENV,
                        )
                    },
                )? {
                    Some(runtime) => Some(runtime),
                    None => RuntimeShell::spawn_candidate(
                        argv,
                        cwd,
                        host_bridge,
                        crate::host::working_directory_available,
                        || {
                            Self::runtime_environment(
                                argv,
                                config.borrow().vim9.auto_gui,
                                &jterm_gtk_view::CHILD_ENV,
                            )
                        },
                    )?,
                };
                if let Some(runtime) = runtime {
                    let pid = runtime
                        .child_pid()
                        .ok_or("shared runtime child has no process id")?;
                    runtime_guard = Some(crate::gtk_process::Guard::capture(
                        pid,
                        RuntimeShell::candidate_program(argv),
                    ));
                    TerminalPane::with_runtime(runtime, theme, status, links, close)?
                } else {
                    legacy(theme, links, close)?
                }
            }
            #[cfg(not(target_os = "linux"))]
            legacy(theme, links, close)?
        };
        #[cfg(target_os = "linux")]
        if pane.runtime().is_some() {
            pane.set_history_row_limit(config.borrow().terminal_scrollback_lines as usize);
            Self::register_vim_source(&widget, &pane);
        }
        Ok(Self {
            widget,
            pane: Some(pane),
            config,
            standalone: true,
            launch_error: None,
            #[cfg(target_os = "linux")]
            runtime_guard,
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn open_for_pane(
        config: Rc<RefCell<Config>>,
        argv: &[String],
        cwd: Option<&str>,
        session_id: &str,
    ) -> Option<Self> {
        let home = std::env::var("HOME").ok();
        let cwd = cwd.or(home.as_deref());
        let result = RuntimeShell::spawn_session_candidate(
            argv,
            cwd,
            session_id,
            crate::host::is_flatpak(),
            crate::host::working_directory_available,
            || {
                Self::runtime_environment(
                    argv,
                    config.borrow().vim9.auto_gui,
                    &jterm_gtk_view::INTERACTIVE_CHILD_ENV,
                )
            },
        );
        Self::from_runtime(config, argv, result, false)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn open_hosted_attachment(
        config: Rc<RefCell<Config>>,
        argv: &[String],
        cwd: &str,
    ) -> Option<Self> {
        let result = RuntimeShell::spawn_hosted_attachment(
            argv,
            std::path::Path::new(cwd),
            crate::host::is_flatpak(),
            || {
                Self::runtime_environment(
                    argv,
                    config.borrow().vim9.auto_gui,
                    &jterm_gtk_view::INTERACTIVE_CHILD_ENV,
                )
            },
        );
        Self::from_runtime(config, argv, result, true)
    }

    #[cfg(target_os = "linux")]
    fn from_runtime(
        config: Rc<RefCell<Config>>,
        argv: &[String],
        result: Result<Option<Rc<RuntimeShell>>, String>,
        hosted_attachment: bool,
    ) -> Option<Self> {
        let build = |runtime: Rc<RuntimeShell>| {
            let pid = runtime
                .child_pid()
                .ok_or("shared shell has no live process id")?;
            let guard =
                crate::gtk_process::Guard::capture(pid, RuntimeShell::candidate_program(argv));
            let mount = if hosted_attachment {
                TerminalPane::with_hosted_attachment
            } else {
                TerminalPane::with_runtime_embedded
            };
            let (widget, pane) = mount(
                runtime,
                Self::theme(&config.borrow()),
                jterm_core::review_input::safe_inline_display,
                Self::links(),
            )?;
            pane.set_history_row_limit(config.borrow().terminal_scrollback_lines as usize);
            if !hosted_attachment {
                Self::register_vim_source(&widget, &pane);
            }
            Ok::<_, String>((widget, pane, guard))
        };
        match result.and_then(|runtime| runtime.map(build).transpose()) {
            Ok(None) => None,
            Ok(Some((widget, pane, guard))) => Some(Self {
                widget,
                pane: Some(pane),
                config,
                standalone: false,
                launch_error: None,
                runtime_guard: Some(guard),
            }),
            Err(error) => {
                let safe = jterm_core::review_input::safe_inline_display(&error, 768);
                let message = format!("Cannot start terminal: {safe}");
                let label = gtk::Label::new(Some(&message));
                label.set_wrap(true);
                label.set_focusable(true);
                Some(Self {
                    widget: label.upcast(),
                    pane: None,
                    config,
                    standalone: false,
                    launch_error: Some(message),
                    runtime_guard: None,
                })
            }
        }
    }

    pub(crate) fn links() -> LinkActions {
        LinkActions {
            admit: jterm_core::link::is_openable_osc8,
            find_plain: Some(jterm_core::link::plain_url_at),
            attach_opener: |widget| {
                let opener = crate::terminal::url::LinkOpener::new(widget);
                Box::new(move |uri| opener.open(uri))
            },
        }
    }

    fn theme(config: &Config) -> Theme {
        let mut font = crate::font::terminal_font_description(&config.font_desc, config);
        let scale = config.default_font_scale;
        if font.size() > 0 && scale.is_finite() && scale > 0.0 {
            let size = (f64::from(font.size()) * scale).round();
            if font.is_size_absolute() {
                font.set_absolute_size(size);
            } else {
                font.set_size(size as i32);
            }
        }
        Theme {
            font,
            foreground: config.foreground,
            background: config.background,
            palette: config.palette,
        }
    }

    #[cfg(target_os = "linux")]
    fn runtime_environment(
        argv: &[String],
        auto_gui: bool,
        terminal_env: &[(&str, &str)],
    ) -> Result<Vec<std::ffi::CString>, String> {
        let policy = jterm_core::child_env::ChildEnv {
            less_default: Some("R"),
            ..jterm_core::child_env::ChildEnv::from_identity()
        };
        let vim = crate::vim9_ide::shell_environment(argv, auto_gui);
        let extra = terminal_env
            .iter()
            .copied()
            .chain(
                vim.iter()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            )
            .collect::<Vec<_>>();
        let env = jterm_core::child_env::envp_from_captured(&policy, &extra)
            .map_err(|error| error.to_string())?;
        let mut env = crate::agent_cli::environment(env);
        env.retain(|item| {
            let bytes = item.as_bytes();
            !bytes.starts_with(b"ANVIL_SHELL_INTEGRATION_FD=")
                && !bytes.starts_with(b"ANVIL_SHELL_INTEGRATION_TOKEN=")
        });
        Ok(env)
    }

    #[cfg(target_os = "linux")]
    fn register_vim_source(widget: &gtk::Widget, pane: &Rc<TerminalPane<OwnedPty>>) {
        // SAFETY: the matching reader below uses this key with the same
        // weak pane type. The widget never retains the runtime itself.
        unsafe {
            widget.set_data(PANE_DATA, Rc::downgrade(pane));
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn vim_source(
        widget: &gtk::Widget,
        invocation: &jterm_core::vim9::Vim9ShellInvocation,
    ) -> Option<(gtk::Box, gtk::Widget)> {
        let pane = unsafe {
            widget
                .data::<std::rc::Weak<TerminalPane<OwnedPty>>>(PANE_DATA)?
                .as_ref()
                .upgrade()?
        };
        pane.runtime()?
            .inspect_child(|pid| invocation.matches_pid(pid).then_some(()))?;
        Some((
            widget.clone().downcast().ok()?,
            pane.screen().clone().upcast(),
        ))
    }

    pub(crate) fn toggle_history_search(&self) {
        if let Some(pane) = &self.pane {
            pane.toggle_history_search();
        }
    }

    pub(crate) fn emit(&self, action: VteInput) {
        let Some(pane) = &self.pane else {
            if matches!(action, VteInput::GrabFocus) {
                self.widget.grab_focus();
            }
            return;
        };
        let result = match action {
            VteInput::WriteInput(data) => pane.write_input(data),
            VteInput::InsertReviewText(data) => {
                crate::enter_ownership::claim_held();
                pane.write_input(data)
            }
            VteInput::PasteText(text) => {
                crate::enter_ownership::claim_held();
                pane.write_text(&text)
            }
            action => {
                match action {
                    VteInput::GrabFocus => pane.focus(),
                    VteInput::Copy | VteInput::CopyOutputOnly => pane.copy(),
                    VteInput::Paste => pane.paste(),
                    VteInput::ScrollLines(lines) => pane.scroll(i64::from(lines)),
                    VteInput::ReturnToLive => pane.follow_output(),
                    VteInput::SetScrollback(lines) => {
                        pane.set_history_row_limit(usize::try_from(lines).unwrap_or(usize::MAX));
                    }
                    VteInput::ApplyTheme | VteInput::SetFont(_) | VteInput::SetFontScale(_) => {
                        pane.set_theme(Self::theme(&self.config.borrow()));
                    }
                    VteInput::Kill => pane.stop(),
                    _ => {}
                }
                Ok(())
            }
        };
        if let Err(error) = result {
            log::warn!("Shared terminal input rejected: {error}");
        }
    }

    pub(crate) fn standalone(&self) -> bool {
        self.standalone
    }

    pub(crate) fn launch_error(&self) -> Option<&str> {
        self.launch_error.as_deref()
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn pane(&self) -> Option<&Rc<TerminalPane<OwnedPty>>> {
        self.pane.as_ref()
    }

    pub(crate) fn grid_size(&self) -> (u16, u16) {
        self.pane.as_ref().map_or((0, 0), |pane| pane.grid_size())
    }

    pub(crate) fn close_blocker(&self) -> Option<String> {
        let pane = self.pane.as_ref()?;
        #[cfg(target_os = "linux")]
        if let (Some(runtime), Some(guard)) = (pane.runtime(), &self.runtime_guard) {
            // Shutdown stops the runtime before its reader can publish an exit code.
            runtime.child_pid()?;
            return guard.blocker(true, None);
        }
        pane.pty().and_then(|pty| pty.close_blocker(true))
    }

    pub(crate) fn child_cleanup(&self) -> Option<crate::gtk_process::ChildDrain> {
        let pane = self.pane.as_ref()?;
        #[cfg(target_os = "linux")]
        if let Some(runtime) = pane.runtime() {
            return Some(crate::gtk_process::ChildDrain::SharedRuntime(Rc::clone(
                runtime,
            )));
        }
        pane.pty().map(|pty| pty.lifecycle().into())
    }

    pub(crate) fn withdrawal_work_probe(&self) -> Box<dyn Fn() -> bool> {
        let Some(pane) = &self.pane else {
            return Box::new(|| false);
        };
        #[cfg(target_os = "linux")]
        if let (Some(runtime), Some(guard)) = (pane.runtime(), &self.runtime_guard) {
            let runtime = Rc::clone(runtime);
            let probe = guard.withdrawal_work_probe();
            return Box::new(move || runtime.child_pid().is_some() && probe());
        }
        pane.pty()
            .expect("legacy pane owns a PTY")
            .withdrawal_work_probe()
    }
}
