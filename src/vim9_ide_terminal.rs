use super::gtk;
use gtk::pango;
use std::{
    ops::Deref,
    rc::{Rc, Weak},
};
use vte4::prelude::*;

const EDITOR_NAME: &str = "vim9-editor-terminal";

#[derive(Clone)]
pub(super) struct EditorTerminal(Rc<Backend>);
pub(super) struct WeakEditorTerminal(Weak<Backend>);

enum Backend {
    Vte(vte4::Terminal),
    #[cfg(all(anvil_shared_terminal, target_os = "linux"))]
    Shared {
        root: gtk::Widget,
        pane: Rc<jterm_gtk_view::TerminalPane<crate::pty::OwnedPty>>,
    },
}

impl EditorTerminal {
    pub(super) fn start(launch: &jterm_core::vim9::Vim9Launch) -> Result<Self, String> {
        #[cfg(all(anvil_shared_terminal, target_os = "linux"))]
        if !crate::host::is_flatpak()
            && std::env::var_os("JTERM_SHARED_RUNTIME").is_none_or(|value| value == "1")
        {
            let mut argv = vec![launch.program.clone()];
            argv.extend(launch.args.iter().cloned());
            let mut extra: Vec<_> = launch
                .env
                .iter()
                .filter_map(|item| item.split_once('='))
                .collect();
            extra.extend(jterm_gtk_view::INTERACTIVE_CHILD_ENV);
            let env = jterm_core::child_env::envp_from_captured(
                &jterm_core::child_env::ChildEnv::from_identity(),
                &extra,
            )
            .map_err(|error| error.to_string())?;
            let runtime = jterm_gtk_view::RuntimeShell::spawn_program(&argv, &launch.cwd, env)?;
            let theme = jterm_gtk_view::Theme {
                font: pango::FontDescription::from_string("Monospace 12"),
                foreground: foreground(),
                background: background(),
                palette: jterm_gtk_view::default_ansi_palette(),
            };
            let (root, pane) = jterm_gtk_view::TerminalPane::with_runtime_embedded(
                runtime,
                theme,
                jterm_core::review_input::safe_inline_display,
                jterm_gtk_view::LinkActions {
                    admit: |_, _| false,
                    find_plain: None,
                    attach_opener: |_| Box::new(|_| {}),
                },
            )?;
            pane.set_history_row_limit(5000);
            configure_widget(pane.screen().upcast_ref());
            return Ok(Self(Rc::new(Backend::Shared { root, pane })));
        }
        let _ = launch;
        Ok(Self::legacy())
    }

    pub(super) fn legacy() -> Self {
        let editor = vte4::Terminal::new();
        editor.set_mouse_autohide(true);
        editor.set_scroll_on_keystroke(true);
        editor.set_scrollback_lines(5000);
        editor.set_font(Some(&pango::FontDescription::from_string("Monospace 12")));
        editor.set_colors(Some(&foreground()), Some(&background()), &[]);
        configure_widget(editor.upcast_ref());
        Self(Rc::new(Backend::Vte(editor)))
    }

    pub(super) fn owns_focus_widget(widget: &gtk::Widget) -> bool {
        widget.widget_name() == EDITOR_NAME
    }

    pub(super) fn widget(&self) -> &gtk::Widget {
        match self.0.as_ref() {
            Backend::Vte(editor) => editor.upcast_ref(),
            #[cfg(all(anvil_shared_terminal, target_os = "linux"))]
            Backend::Shared { pane, .. } => pane.screen().upcast_ref(),
        }
    }

    pub(super) fn root(&self) -> &gtk::Widget {
        match self.0.as_ref() {
            Backend::Vte(editor) => editor.upcast_ref(),
            #[cfg(all(anvil_shared_terminal, target_os = "linux"))]
            Backend::Shared { root, .. } => root,
        }
    }

    pub(super) fn vte(&self) -> Option<&vte4::Terminal> {
        match self.0.as_ref() {
            Backend::Vte(editor) => Some(editor),
            #[cfg(all(anvil_shared_terminal, target_os = "linux"))]
            Backend::Shared { .. } => None,
        }
    }

    pub(super) fn downgrade(&self) -> WeakEditorTerminal {
        WeakEditorTerminal(Rc::downgrade(&self.0))
    }

    pub(super) fn connect_exit(&self, on_exit: impl Fn(i32) + 'static) {
        match self.0.as_ref() {
            Backend::Vte(editor) => {
                editor.connect_child_exited(move |_, status| {
                    if let Some(code) = jterm_core::process::exit_code_from_wait_status(status) {
                        on_exit(code);
                    }
                });
            }
            #[cfg(all(anvil_shared_terminal, target_os = "linux"))]
            Backend::Shared { pane, .. } => {
                pane.set_event_handler(move |event| {
                    if let jterm_gtk_view::PaneEvent::Exited(status) = event {
                        on_exit(status.code);
                    }
                });
            }
        }
    }

    pub(super) fn commit_text(&self, text: &str) {
        match self.0.as_ref() {
            Backend::Vte(editor) => editor.feed_child(text.as_bytes()),
            #[cfg(all(anvil_shared_terminal, target_os = "linux"))]
            Backend::Shared { pane, .. } => {
                if let Err(error) = pane.write_text(text) {
                    log::warn!("Vim input rejected: {error}");
                }
            }
        }
    }

    pub(super) fn delegate_composition(&self, delegated: bool) {
        #[cfg(all(anvil_shared_terminal, target_os = "linux"))]
        if let Backend::Shared { pane, .. } = self.0.as_ref() {
            pane.delegate_composition(delegated);
        }
        let _ = delegated;
    }

    pub(super) fn set_font(&self, font: &pango::FontDescription) {
        match self.0.as_ref() {
            Backend::Vte(editor) => editor.set_font(Some(font)),
            #[cfg(all(anvil_shared_terminal, target_os = "linux"))]
            Backend::Shared { pane, .. } => pane.set_font(font),
        }
    }

    pub(super) fn set_line_height_scale(&self, scale: f64) {
        match self.0.as_ref() {
            Backend::Vte(editor) => editor.set_cell_height_scale(scale),
            #[cfg(all(anvil_shared_terminal, target_os = "linux"))]
            Backend::Shared { pane, .. } => pane.set_line_height_scale(scale),
        }
    }

    pub(super) fn grid_size(&self) -> (usize, usize) {
        match self.0.as_ref() {
            Backend::Vte(editor) => (
                editor.row_count().max(0) as usize,
                editor.column_count().max(0) as usize,
            ),
            #[cfg(all(anvil_shared_terminal, target_os = "linux"))]
            Backend::Shared { pane, .. } => {
                let (columns, rows) = pane.grid_size();
                (usize::from(rows), usize::from(columns))
            }
        }
    }

    pub(super) fn has_selection(&self) -> bool {
        match self.0.as_ref() {
            Backend::Vte(editor) => editor.has_selection(),
            #[cfg(all(anvil_shared_terminal, target_os = "linux"))]
            Backend::Shared { pane, .. } => pane.has_selection(),
        }
    }

    pub(super) fn cell_metrics(&self) -> (f32, f32, f32, f32) {
        match self.0.as_ref() {
            Backend::Vte(editor) => {
                let border = gtk::prelude::ScrollableExt::border(editor);
                let (left, top) = border.as_ref().map_or((0.0, 0.0), |border| {
                    (f32::from(border.left()), f32::from(border.top()))
                });
                (
                    left,
                    top,
                    editor.char_width().max(1) as f32,
                    editor.char_height().max(1) as f32,
                )
            }
            #[cfg(all(anvil_shared_terminal, target_os = "linux"))]
            Backend::Shared { pane, .. } => {
                let (left, top, width, height) = pane.cell_metrics();
                (left as f32, top as f32, width as f32, height as f32)
            }
        }
    }

    pub(super) fn grid_matches_allocation(&self) -> bool {
        match self.0.as_ref() {
            Backend::Vte(editor) => {
                let border = gtk::prelude::ScrollableExt::border(editor);
                let (horizontal, vertical) = border.as_ref().map_or((0, 0), |border| {
                    (
                        i32::from(border.left()) + i32::from(border.right()),
                        i32::from(border.top()) + i32::from(border.bottom()),
                    )
                });
                let columns =
                    i64::from((editor.width() - horizontal).max(0)) / editor.char_width().max(1);
                let rows =
                    i64::from((editor.height() - vertical).max(0)) / editor.char_height().max(1);
                rows > 0
                    && columns > 0
                    && rows == editor.row_count()
                    && columns == editor.column_count()
            }
            #[cfg(all(anvil_shared_terminal, target_os = "linux"))]
            Backend::Shared { pane, .. } => pane.grid_matches_allocation(),
        }
    }
}

impl WeakEditorTerminal {
    pub(super) fn upgrade(&self) -> Option<EditorTerminal> {
        self.0.upgrade().map(EditorTerminal)
    }
}

impl Deref for EditorTerminal {
    type Target = gtk::Widget;

    fn deref(&self) -> &Self::Target {
        self.widget()
    }
}

fn configure_widget(widget: &gtk::Widget) {
    widget.set_widget_name(EDITOR_NAME);
    widget.set_hexpand(true);
    widget.set_vexpand(true);
    widget.update_property(&[gtk::accessible::Property::Label("Vim9 editor")]);
}

fn foreground() -> gtk::gdk::RGBA {
    gtk::gdk::RGBA::new(0.85, 0.88, 0.93, 1.0)
}

fn background() -> gtk::gdk::RGBA {
    gtk::gdk::RGBA::new(0.07, 0.09, 0.12, 1.0)
}
