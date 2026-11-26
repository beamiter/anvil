#[cfg(target_os = "linux")]
use jterm_gtk_view::RuntimeShell;
use jterm_gtk_view::{CursorColors, LinkActions, TerminalPane, Theme};
use relm4::gtk::{self, prelude::*};
use std::{cell::RefCell, rc::Rc};

use crate::{config::Config, pty::OwnedPty, terminal::VteInput};

#[cfg(target_os = "linux")]
const PANE_DATA: &str = "anvil-shared-panes-vim-source";

#[cfg(target_os = "linux")]
type VimSource = (
    std::rc::Weak<TerminalPane<OwnedPty>>,
    gtk::glib::WeakRef<gtk::Widget>,
);

#[cfg(target_os = "linux")]
struct ViewControls {
    reattach: gtk::Button,
    style: gtk::CssProvider,
}

pub(crate) struct SharedTerminal {
    pub(crate) widget: gtk::Widget,
    pane: Option<Rc<TerminalPane<OwnedPty>>>,
    find: Option<Rc<jterm_gtk_view::TerminalFind<OwnedPty>>>,
    config: Rc<RefCell<Config>>,
    standalone: bool,
    launch_error: Option<String>,
    #[cfg(target_os = "linux")]
    runtime_guard: Option<std::sync::Arc<crate::gtk_process::Guard>>,
    #[cfg(target_os = "linux")]
    view_controls: Option<ViewControls>,
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
        #[cfg(all(target_os = "linux", test))]
        let mut runtime_guard = None;
        #[cfg(all(target_os = "linux", not(test)))]
        let runtime_guard;
        let status = jterm_core::review_input::safe_inline_display;
        #[cfg(any(test, not(target_os = "linux")))]
        let legacy = |theme, links, close| {
            let argv: Vec<_> = argv.iter().map(String::as_str).collect();
            let pty = OwnedPty::spawn(&argv, cwd, &jterm_gtk_view::CHILD_ENV)
                .map_err(|error| error.to_string())?;
            TerminalPane::new(pty, theme, status, links, close)
        };
        let (widget, pane) = {
            #[cfg(all(target_os = "linux", not(test)))]
            {
                let mut launch = argv.to_vec();
                let environment = || {
                    Self::runtime_environment(
                        argv,
                        config.borrow().vim9.auto_gui,
                        &jterm_gtk_view::CHILD_ENV,
                    )
                };
                let runtime = if crate::host::is_flatpak() {
                    launch = crate::host::wrap_argv(argv, cwd, &jterm_gtk_view::CHILD_ENV);
                    RuntimeShell::spawn_pane_command(&launch, Some("/"), None, environment()?)?
                } else if matches!(argv, [program] if matches!(program.as_str(), "/bin/bash" | "/usr/bin/bash"))
                {
                    RuntimeShell::spawn_bundled_bash(
                        std::path::Path::new(&argv[0]),
                        cwd,
                        crate::host::working_directory_available,
                        include_bytes!("../scripts/shell-integration/anvil.bash"),
                        environment,
                    )?
                } else {
                    RuntimeShell::spawn_configured_shell(
                        argv,
                        cwd,
                        None,
                        crate::host::working_directory_available,
                        environment,
                    )?
                };
                let pid = runtime
                    .child_pid()
                    .ok_or("shared runtime child has no process id")?;
                runtime_guard = Some(crate::gtk_process::Guard::capture(
                    pid,
                    RuntimeShell::candidate_program(&launch),
                ));
                TerminalPane::with_runtime(runtime, theme, status, links, close)?
            }
            #[cfg(all(target_os = "linux", test))]
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
                // Configured shells may have flags outside the standalone
                // bash/jsh probe profile. They still belong to the same shared
                // session owner, with their exact argument list preserved.
                let runtime = match runtime {
                    Some(runtime) => Some(runtime),
                    None => RuntimeShell::spawn_explicit_command(argv, cwd, host_bridge, || {
                        Self::runtime_environment(
                            argv,
                            config.borrow().vim9.auto_gui,
                            &jterm_gtk_view::CHILD_ENV,
                        )
                    })?,
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
        let (widget, view_controls) = if pane.runtime().is_some() {
            let settings = config.borrow();
            pane.set_history_row_limit(settings.terminal_scrollback_lines as usize);
            let (mount, controls) = Self::mount_view_controls(&widget, &pane, &settings);
            Self::register_vim_source(&mount, &widget, &pane);
            (mount, controls)
        } else {
            (widget, None)
        };
        let find = Some(pane.find_controller());
        Ok(Self {
            widget,
            find,
            pane: Some(pane),
            config,
            standalone: true,
            launch_error: None,
            #[cfg(target_os = "linux")]
            runtime_guard,
            #[cfg(target_os = "linux")]
            view_controls,
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn open_for_pane(
        config: Rc<RefCell<Config>>,
        argv: &[String],
        cwd: Option<&str>,
        session_id: &str,
        env_extra: &[(String, String)],
    ) -> Option<Self> {
        if !env_extra.is_empty() {
            let (argv, _) = crate::config::shell_argv_with_session(argv, Some(session_id));
            return Self::open_task(config, &argv, cwd, None, env_extra, false);
        }
        let home = std::env::var("HOME").ok();
        let cwd = cwd.or(home.as_deref());
        if crate::host::is_flatpak() {
            let argv = RuntimeShell::configured_shell_argv(argv, session_id)?;
            return Self::open_host_command(config, &argv, cwd);
        }
        #[cfg(not(anvil_vte_oracle))]
        let result = RuntimeShell::spawn_configured_shell(
            argv,
            cwd,
            Some(session_id),
            crate::host::working_directory_available,
            || {
                Self::runtime_environment(
                    argv,
                    config.borrow().vim9.auto_gui,
                    &jterm_gtk_view::INTERACTIVE_CHILD_ENV,
                )
            },
        )
        .map(Some);
        #[cfg(anvil_vte_oracle)]
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
        Self::from_runtime(config, argv, result, false, false)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn open_hosted_attachment(
        config: Rc<RefCell<Config>>,
        argv: &[String],
        cwd: &str,
    ) -> Option<Self> {
        let (launch, local_directory) =
            crate::host::hosted_attachment_pty(argv, cwd, &jterm_gtk_view::INTERACTIVE_CHILD_ENV);
        #[cfg(not(anvil_vte_oracle))]
        let result = Self::runtime_environment(
            argv,
            config.borrow().vim9.auto_gui,
            &jterm_gtk_view::INTERACTIVE_CHILD_ENV,
        )
        .and_then(|environment| RuntimeShell::spawn_program(&launch, local_directory, environment))
        .map(Some);
        #[cfg(anvil_vte_oracle)]
        let result = RuntimeShell::spawn_hosted_attachment(&launch, local_directory, || {
            Self::runtime_environment(
                argv,
                config.borrow().vim9.auto_gui,
                &jterm_gtk_view::INTERACTIVE_CHILD_ENV,
            )
        });
        Self::from_runtime(config, &launch, result, true, false)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn open_explicit(
        config: Rc<RefCell<Config>>,
        argv: &[String],
        cwd: Option<&str>,
        env_extra: &[(String, String)],
    ) -> Option<Self> {
        if !env_extra.is_empty() {
            return Self::open_task(config, argv, cwd, None, env_extra, false);
        }
        let home = std::env::var("HOME").ok();
        let launch = crate::agent_cli::inline_argv(argv);
        let cwd = cwd.or(home.as_deref());
        if crate::host::is_flatpak() {
            return Self::open_host_command(config, &launch, cwd);
        }
        #[cfg(not(anvil_vte_oracle))]
        let result = Self::runtime_environment(
            argv,
            config.borrow().vim9.auto_gui,
            &jterm_gtk_view::INTERACTIVE_CHILD_ENV,
        )
        .and_then(|environment| RuntimeShell::spawn_pane_command(&launch, cwd, None, environment))
        .map(Some);
        #[cfg(anvil_vte_oracle)]
        let result =
            RuntimeShell::spawn_explicit_command(&launch, cwd, crate::host::is_flatpak(), || {
                Self::runtime_environment(
                    argv,
                    config.borrow().vim9.auto_gui,
                    &jterm_gtk_view::INTERACTIVE_CHILD_ENV,
                )
            });
        Self::from_runtime(config, &launch, result, false, false)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn open_task(
        config: Rc<RefCell<Config>>,
        argv: &[String],
        cwd: Option<&str>,
        pinned_directory: Option<std::os::fd::BorrowedFd<'_>>,
        env_extra: &[(String, String)],
        inline_agent: bool,
    ) -> Option<Self> {
        let host_bridge = crate::host::is_flatpak();
        let host_pin = pinned_directory.filter(|_| host_bridge);
        let home = std::env::var("HOME").ok();
        let cwd = cwd.or(home.as_deref());
        let vim = if env_extra.is_empty() {
            crate::vim9_ide::shell_environment(argv, config.borrow().vim9.auto_gui)
        } else {
            Vec::new()
        };
        let extra: Vec<_> = vim
            .iter()
            .chain(env_extra)
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let argv = crate::agent_cli::inline_argv(argv);
        let launch = if host_pin.is_some() {
            crate::host::wrap_pinned_validation_argv(&argv, &extra)
        } else {
            crate::host::wrap_argv(&argv, cwd, &extra)
        };
        let local_cwd = if host_bridge { Some("/") } else { cwd };
        let result =
            Self::captured_runtime_environment(&extra).and_then(|environment| match host_pin {
                Some(directory) => RuntimeShell::spawn_forwarded_pane_command(
                    &launch,
                    directory,
                    crate::host::FLATPAK_VALIDATION_DIRECTORY_FD,
                    environment,
                ),
                None => RuntimeShell::spawn_pane_command(
                    &launch,
                    local_cwd,
                    pinned_directory,
                    environment,
                ),
            });
        Self::from_runtime(config, &launch, result.map(Some), false, inline_agent)
    }

    #[cfg(target_os = "linux")]
    fn open_host_command(
        config: Rc<RefCell<Config>>,
        argv: &[String],
        cwd: Option<&str>,
    ) -> Option<Self> {
        let environment = &jterm_gtk_view::INTERACTIVE_CHILD_ENV;
        let launch = crate::host::wrap_argv(argv, cwd, environment);
        #[cfg(not(anvil_vte_oracle))]
        let result = Self::runtime_environment(argv, config.borrow().vim9.auto_gui, environment)
            .and_then(|environment| {
                RuntimeShell::spawn_pane_command(&launch, Some("/"), None, environment)
            })
            .map(Some);
        #[cfg(anvil_vte_oracle)]
        let result = RuntimeShell::spawn_explicit_command(&launch, Some("/"), false, || {
            Self::runtime_environment(argv, config.borrow().vim9.auto_gui, environment)
        });
        Self::from_runtime(config, &launch, result, false, false)
    }

    #[cfg(target_os = "linux")]
    fn from_runtime(
        config: Rc<RefCell<Config>>,
        argv: &[String],
        result: Result<Option<Rc<RuntimeShell>>, String>,
        hosted_attachment: bool,
        inline_task: bool,
    ) -> Option<Self> {
        let build = |runtime: Rc<RuntimeShell>| {
            let pid = runtime
                .child_pid()
                .ok_or("shared shell has no live process id")?;
            let guard =
                crate::gtk_process::Guard::capture(pid, RuntimeShell::candidate_program(argv));
            let mount = if hosted_attachment {
                TerminalPane::with_hosted_attachment
            } else if inline_task {
                TerminalPane::with_inline_task
            } else {
                TerminalPane::with_runtime_embedded
            };
            let (widget, pane) = mount(
                runtime,
                Self::theme(&config.borrow()),
                jterm_core::review_input::safe_inline_display,
                Self::links(),
            )?;
            let settings = config.borrow();
            pane.set_history_row_limit(settings.terminal_scrollback_lines as usize);
            let (mount, controls) = Self::mount_view_controls(&widget, &pane, &settings);
            if !hosted_attachment {
                Self::register_vim_source(&mount, &widget, &pane);
            }
            Ok::<_, String>((mount, pane, guard, controls))
        };
        match result.and_then(|runtime| runtime.map(build).transpose()) {
            Ok(None) => None,
            Ok(Some((widget, pane, guard, view_controls))) => Some(Self {
                widget,
                find: Some(pane.find_controller()),
                pane: Some(pane),
                config,
                standalone: false,
                launch_error: None,
                runtime_guard: Some(guard),
                view_controls,
            }),
            Err(error) => {
                let safe = jterm_core::review_input::safe_inline_display(&error, 768);
                let message = format!("Cannot start terminal: {safe}");
                let label = gtk::Label::new(Some(&message));
                label.set_wrap(true);
                label.set_focusable(true);
                Some(Self {
                    widget: label.upcast(),
                    find: None,
                    pane: None,
                    config,
                    standalone: false,
                    launch_error: Some(message),
                    runtime_guard: None,
                    view_controls: None,
                })
            }
        }
    }

    #[cfg(all(target_os = "linux", not(test)))]
    pub(crate) fn unavailable(config: Rc<RefCell<Config>>, message: &str) -> Self {
        Self::from_runtime(config, &[], Err(message.to_owned()), false, false)
            .expect("a launch failure always produces an error view")
    }

    #[cfg(anvil_shared_terminal)]
    pub(crate) fn document_links() -> LinkActions {
        LinkActions {
            find_plain: Some(jterm_core::link::document_url_at),
            ..Self::links()
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

    pub(crate) fn theme(config: &Config) -> Theme {
        Self::theme_at_scale(config, config.default_font_scale)
    }

    pub(crate) fn theme_at_scale(config: &Config, scale: f64) -> Theme {
        let mut font = crate::font::terminal_font_description(&config.font_desc, config);
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
            cursor: Some(CursorColors {
                foreground: config.cursor_foreground,
                background: config.cursor,
            }),
        }
    }

    #[cfg(target_os = "linux")]
    fn runtime_environment(
        argv: &[String],
        auto_gui: bool,
        terminal_env: &[(&str, &str)],
    ) -> Result<Vec<std::ffi::CString>, String> {
        let vim = crate::vim9_ide::shell_environment(argv, auto_gui);
        let extra = terminal_env
            .iter()
            .copied()
            .chain(
                vim.iter()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            )
            .collect::<Vec<_>>();
        Self::captured_runtime_environment(&extra)
    }

    #[cfg(target_os = "linux")]
    fn captured_runtime_environment(
        extra: &[(&str, &str)],
    ) -> Result<Vec<std::ffi::CString>, String> {
        let policy = jterm_core::child_env::ChildEnv {
            less_default: Some("R"),
            ..jterm_core::child_env::ChildEnv::from_identity()
        };
        let env = jterm_core::child_env::envp_from_captured(&policy, extra)
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
    fn register_vim_source(
        mount: &gtk::Widget,
        terminal: &gtk::Widget,
        pane: &Rc<TerminalPane<OwnedPty>>,
    ) {
        // SAFETY: the matching reader below uses this key with the same
        // weak types. The mount never retains the runtime itself.
        unsafe {
            mount.set_data(PANE_DATA, (Rc::downgrade(pane), terminal.downgrade()));
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn vim_source(
        widget: &gtk::Widget,
        invocation: &jterm_core::vim9::Vim9ShellInvocation,
    ) -> Option<(gtk::Box, gtk::Widget)> {
        let (pane, terminal) = unsafe { widget.data::<VimSource>(PANE_DATA)?.as_ref() };
        let pane = pane.upgrade()?;
        if !pane.view_is_attached() {
            return None;
        }
        pane.runtime()?
            .inspect_child(|pid| invocation.matches_pid(pid).then_some(()))?;
        Some((
            terminal.upgrade()?.downcast().ok()?,
            pane.screen().clone().upcast(),
        ))
    }

    #[cfg(target_os = "linux")]
    fn style_view_controls(style: &gtk::CssProvider, config: &Config) {
        style.load_from_string(&format!(
            "* {{ background-color: {}; background-image: none; color: {}; }}",
            config.background, config.foreground
        ));
    }

    #[cfg(target_os = "linux")]
    fn describe_view_button(button: &gtk::Button, ticket: &jterm_gtk_view::Attachment) {
        button.update_property(&[gtk::accessible::Property::Description(&format!(
            "session={} epoch={} generation={}",
            ticket.session, ticket.epoch, ticket.generation
        ))]);
    }

    #[cfg(target_os = "linux")]
    fn mount_view_controls(
        terminal: &gtk::Widget,
        pane: &Rc<TerminalPane<OwnedPty>>,
        config: &Config,
    ) -> (gtk::Widget, Option<ViewControls>) {
        let Ok(ticket) = pane.view_attachment() else {
            return (terminal.clone(), None);
        };
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        let detach = gtk::Button::with_label("Detach terminal view");
        detach.set_halign(gtk::Align::End);
        detach.set_tooltip_text(Some("Keep the terminal running while its view is detached"));
        Self::describe_view_button(&detach, &ticket);
        root.append(&detach);

        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.set_vexpand(true);
        stack.add_named(terminal, Some("attached"));
        let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        placeholder.set_vexpand(true);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_vexpand(true);
        content.set_valign(gtk::Align::Center);
        content.set_halign(gtk::Align::Center);
        content.append(&gtk::Label::new(Some("Terminal view detached")));
        let reattach = gtk::Button::with_label("Reattach terminal view");
        content.append(&reattach);
        placeholder.append(&content);
        stack.add_named(&placeholder, Some("detached"));
        stack.set_visible_child_name("attached");
        root.append(&stack);

        let style = gtk::CssProvider::new();
        Self::style_view_controls(&style, config);
        for widget in [
            placeholder.upcast::<gtk::Widget>(),
            detach.clone().upcast(),
            reattach.clone().upcast(),
        ] {
            #[allow(deprecated)]
            widget
                .style_context()
                .add_provider(&style, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }

        let ticket = Rc::new(RefCell::new(ticket));
        let weak_pane = Rc::downgrade(pane);
        let weak_stack = stack.downgrade();
        let peer = reattach.downgrade();
        let expected = ticket.clone();
        detach.connect_clicked(move |button| {
            let (Some(pane), Some(stack), Some(reattach)) =
                (weak_pane.upgrade(), weak_stack.upgrade(), peer.upgrade())
            else {
                return;
            };
            let attachment = expected.borrow().clone();
            match pane.switch_view_attachment(&attachment, false) {
                Ok(next) => {
                    Self::describe_view_button(&reattach, &next);
                    expected.replace(next);
                    button.set_sensitive(false);
                    stack.set_visible_child_name("detached");
                    reattach.grab_focus();
                }
                Err(error) => button.set_tooltip_text(Some(&error)),
            }
        });
        let weak_pane = Rc::downgrade(pane);
        let weak_stack = stack.downgrade();
        let peer = detach.downgrade();
        reattach.connect_clicked(move |button| {
            let (Some(pane), Some(stack), Some(detach)) =
                (weak_pane.upgrade(), weak_stack.upgrade(), peer.upgrade())
            else {
                return;
            };
            let attachment = ticket.borrow().clone();
            match pane.switch_view_attachment(&attachment, true) {
                Ok(next) => {
                    Self::describe_view_button(&detach, &next);
                    ticket.replace(next);
                    stack.set_visible_child_name("attached");
                    detach.set_sensitive(true);
                    pane.focus();
                }
                Err(error) => button.set_tooltip_text(Some(&error)),
            }
        });
        (root.upcast(), Some(ViewControls { reattach, style }))
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn connect_detached_focus(&self, focused: impl Fn() + 'static) {
        if let Some(controls) = &self.view_controls {
            controls.reattach.connect_has_focus_notify(move |button| {
                if button.has_focus() {
                    focused();
                }
            });
        }
    }

    pub(crate) fn set_find_status_handler(
        &self,
        handler: impl Fn(crate::search::SearchStatus) + 'static,
    ) {
        if let Some(find) = &self.find {
            find.set_status_handler(move |status| {
                handler(match status {
                    jterm_gtk_view::TerminalFindStatus::Idle => crate::search::SearchStatus::Idle,
                    jterm_gtk_view::TerminalFindStatus::Results {
                        current,
                        total,
                        truncated,
                    } => crate::search::SearchStatus::Results {
                        current,
                        total,
                        truncated,
                    },
                    jterm_gtk_view::TerminalFindStatus::Error(error) => {
                        crate::search::SearchStatus::Error(error)
                    }
                })
            });
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
            VteInput::SearchSet(query, use_regex, case_sensitive) => {
                if let Some(find) = &self.find {
                    find.set_query(&query, use_regex, case_sensitive);
                }
                Ok(())
            }
            VteInput::SearchNext => {
                if let Some(find) = &self.find {
                    find.next();
                }
                Ok(())
            }
            VteInput::SearchPrev => {
                if let Some(find) = &self.find {
                    find.previous();
                }
                Ok(())
            }
            VteInput::SearchClear => {
                if let Some(find) = &self.find {
                    find.clear();
                }
                Ok(())
            }
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
                    VteInput::GrabFocus => {
                        #[cfg(target_os = "linux")]
                        if !pane.view_is_attached() {
                            if let Some(controls) = &self.view_controls {
                                controls.reattach.grab_focus();
                            }
                            return;
                        }
                        pane.focus();
                    }
                    VteInput::Copy | VteInput::CopyOutputOnly => pane.copy(),
                    VteInput::Paste => pane.paste(),
                    VteInput::ScrollLines(lines) => pane.scroll(i64::from(lines)),
                    VteInput::ReturnToLive => pane.follow_output(),
                    VteInput::SetScrollback(lines) => {
                        pane.set_history_row_limit(usize::try_from(lines).unwrap_or(usize::MAX));
                    }
                    VteInput::ApplyTheme | VteInput::SetFont(_) | VteInput::SetFontScale(_) => {
                        let settings = self.config.borrow();
                        pane.set_theme(Self::theme(&settings));
                        #[cfg(target_os = "linux")]
                        if let Some(controls) = &self.view_controls {
                            Self::style_view_controls(&controls.style, &settings);
                        }
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "requires private GTK display and a real PTY; run alone"]
    fn configured_standalone_shell_uses_runtime_beyond_probe_profile() {
        jterm_core::child_env::capture_inherited_environment().unwrap();
        gtk::init().expect("private GTK display");
        let context = gtk::glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"printf 'configured-ready\n'; IFS= read -r reply; printf 'configured-reply:%s\n' "$reply""#.into(),
        ];
        assert!(!RuntimeShell::admits_candidate(&argv, false));
        let terminal = SharedTerminal::open(
            Rc::new(RefCell::new(Config::safe_defaults())),
            &argv,
            Some("/tmp"),
            |_| {},
        )
        .unwrap();
        let pane = terminal.pane().unwrap();
        let runtime = pane
            .runtime()
            .expect("configured shell must use the shared owner");
        let pid = runtime.child_pid().unwrap();
        assert!(pane.pty().is_none(), "no frontend-owned PTY fallback");
        let window = gtk::Window::new();
        window.set_child(Some(&terminal.widget));
        window.present();
        let wait = |predicate: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !predicate() {
                assert!(
                    Instant::now() < deadline,
                    "configured shell did not publish or drain"
                );
                context.iteration(false);
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        let visible = |needle: &str| {
            let buffer = pane.screen().buffer();
            buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .contains(needle)
        };
        wait(&|| visible("configured-ready"));
        pane.write_input(b"same-owner\n".to_vec()).unwrap();
        wait(&|| visible("configured-reply:same-owner"));
        wait(&|| runtime.ready_to_discard());
        assert_eq!(runtime.exit_code(), Some(0));
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        window.destroy();
    }
}
