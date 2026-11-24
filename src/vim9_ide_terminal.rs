use super::gtk;
use gtk::{pango, prelude::*};
use std::{
    ops::Deref,
    rc::{Rc, Weak},
};
#[cfg(not(target_os = "linux"))]
use vte4::prelude::*;

const EDITOR_NAME: &str = "vim9-editor-terminal";

#[derive(Clone)]
pub(super) struct EditorTerminal(Rc<Backend>);
pub(super) struct WeakEditorTerminal(Weak<Backend>);

#[cfg(target_os = "linux")]
struct Backend {
    root: gtk::Widget,
    pane: Rc<jterm_gtk_view::TerminalPane<crate::pty::OwnedPty>>,
}

#[cfg(not(target_os = "linux"))]
type Backend = vte4::Terminal;

impl EditorTerminal {
    pub(super) fn start(launch: &jterm_core::vim9::Vim9Launch) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        let backend = {
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
                cursor: None,
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
            Backend { root, pane }
        };
        #[cfg(not(target_os = "linux"))]
        let backend = {
            let _ = launch;
            let editor = vte4::Terminal::new();
            editor.set_mouse_autohide(true);
            editor.set_scroll_on_keystroke(true);
            editor.set_scrollback_lines(5000);
            editor.set_font(Some(&pango::FontDescription::from_string("Monospace 12")));
            editor.set_colors(Some(&foreground()), Some(&background()), &[]);
            editor
        };
        let editor = Self(Rc::new(backend));
        configure_widget(editor.widget());
        #[cfg(test)]
        editor.bind_test_widget();
        Ok(editor)
    }

    pub(super) fn owns_focus_widget(widget: &gtk::Widget) -> bool {
        widget.widget_name() == EDITOR_NAME
    }

    pub(super) fn widget(&self) -> &gtk::Widget {
        #[cfg(target_os = "linux")]
        {
            self.0.pane.screen().upcast_ref()
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.0.upcast_ref()
        }
    }

    pub(super) fn root(&self) -> &gtk::Widget {
        #[cfg(target_os = "linux")]
        {
            &self.0.root
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.widget()
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn vte(&self) -> &vte4::Terminal {
        &self.0
    }

    pub(super) fn downgrade(&self) -> WeakEditorTerminal {
        WeakEditorTerminal(Rc::downgrade(&self.0))
    }

    pub(super) fn connect_exit(&self, on_exit: impl Fn(i32) + 'static) {
        #[cfg(target_os = "linux")]
        self.0.pane.set_event_handler(move |event| {
            if let jterm_gtk_view::PaneEvent::Exited(status) = event {
                on_exit(status.code);
            }
        });
        #[cfg(not(target_os = "linux"))]
        self.0.connect_child_exited(move |_, status| {
            if let Some(code) = jterm_core::process::exit_code_from_wait_status(status) {
                on_exit(code);
            }
        });
    }

    pub(super) fn commit_text(&self, text: &str) {
        #[cfg(target_os = "linux")]
        if let Err(error) = self.0.pane.write_text(text) {
            log::warn!("Vim input rejected: {error}");
        }
        #[cfg(not(target_os = "linux"))]
        self.0.feed_child(text.as_bytes());
    }

    pub(super) fn delegate_composition(&self, delegated: bool) {
        #[cfg(target_os = "linux")]
        self.0.pane.delegate_composition(delegated);
        #[cfg(not(target_os = "linux"))]
        let _ = delegated;
    }

    pub(super) fn set_font(&self, font: &pango::FontDescription) {
        #[cfg(target_os = "linux")]
        self.0.pane.set_font(font);
        #[cfg(not(target_os = "linux"))]
        self.0.set_font(Some(font));
    }

    pub(super) fn set_line_height_scale(&self, scale: f64) {
        #[cfg(target_os = "linux")]
        self.0.pane.set_line_height_scale(scale);
        #[cfg(not(target_os = "linux"))]
        self.0.set_cell_height_scale(scale);
    }

    pub(super) fn grid_size(&self) -> (usize, usize) {
        #[cfg(target_os = "linux")]
        {
            let (columns, rows) = self.0.pane.grid_size();
            (usize::from(rows), usize::from(columns))
        }
        #[cfg(not(target_os = "linux"))]
        {
            (
                self.0.row_count().max(0) as usize,
                self.0.column_count().max(0) as usize,
            )
        }
    }

    pub(super) fn has_selection(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.0.pane.has_selection()
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.0.has_selection()
        }
    }

    pub(super) fn cell_metrics(&self) -> (f32, f32, f32, f32) {
        #[cfg(target_os = "linux")]
        {
            let (left, top, width, height) = self.0.pane.cell_metrics();
            (left as f32, top as f32, width as f32, height as f32)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let border = gtk::prelude::ScrollableExt::border(self.0.as_ref());
            let (left, top) = border.as_ref().map_or((0.0, 0.0), |border| {
                (f32::from(border.left()), f32::from(border.top()))
            });
            (
                left,
                top,
                self.0.char_width().max(1) as f32,
                self.0.char_height().max(1) as f32,
            )
        }
    }

    pub(super) fn grid_matches_allocation(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.0.pane.grid_matches_allocation()
        }
        #[cfg(not(target_os = "linux"))]
        {
            let vte = self.0.as_ref();
            let border = gtk::prelude::ScrollableExt::border(vte);
            let (horizontal, vertical) = border.as_ref().map_or((0, 0), |border| {
                (
                    i32::from(border.left()) + i32::from(border.right()),
                    i32::from(border.top()) + i32::from(border.bottom()),
                )
            });
            let columns = i64::from((vte.width() - horizontal).max(0)) / vte.char_width().max(1);
            let rows = i64::from((vte.height() - vertical).max(0)) / vte.char_height().max(1);
            rows > 0 && columns > 0 && rows == vte.row_count() && columns == vte.column_count()
        }
    }
}

#[cfg(test)]
impl EditorTerminal {
    const TEST_WIDGET_KEY: &str = "jterm-vim9-native-test-editor";

    fn bind_test_widget(&self) {
        // This key is private to the fixture. Store a weak reference so the
        // widget cannot keep the editor (and its PTY) alive.
        unsafe {
            self.widget()
                .set_data(Self::TEST_WIDGET_KEY, self.downgrade());
        }
    }

    pub(super) fn from_test_widget(widget: &gtk::Widget) -> Option<Self> {
        // bind_test_widget is the only writer and GTK runs these tests on
        // the owning thread, so the pointer is valid until this call returns.
        let editor = unsafe { widget.data::<WeakEditorTerminal>(Self::TEST_WIDGET_KEY)? };
        unsafe { editor.as_ref().upgrade() }
    }

    pub(super) fn write_test_input(&self, bytes: &[u8]) {
        #[cfg(target_os = "linux")]
        self.0
            .pane
            .write_input(bytes.to_vec())
            .expect("Vim PTY accepted input");
        #[cfg(not(target_os = "linux"))]
        self.0.feed_child(bytes);
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
