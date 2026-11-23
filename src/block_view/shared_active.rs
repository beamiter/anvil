use super::{OwnedPty, SelectionFeedHold, SubmissionSurface};
use crate::{config::Config, shared_terminal::SharedTerminal};
use gtk::prelude::*;
use jterm_gtk_view::{EmbeddedTerminal, HistoryAnchor, PaneEvent, Pty, PtyExit, TranscriptView};
use relm4::gtk;
use std::{cell::OnceCell, rc::Rc};

type Input = Box<dyn Fn(&[u8]) -> Result<(), String>>;

pub(super) struct SharedActiveTerminal {
    terminal: Rc<EmbeddedTerminal<Transport>>,
    transcript: Option<TranscriptView<Transport>>,
    pub(super) widget: gtk::Widget,
    input: Rc<OnceCell<InputBinding>>,
}

pub(super) struct Transport {
    pty: Rc<OwnedPty>,
    input: Rc<OnceCell<InputBinding>>,
}

struct InputBinding {
    write: Input,
    hold: Rc<SelectionFeedHold>,
}

impl Pty for Transport {
    fn before_input(&self) {
        if let Some(input) = self.input.get() {
            input.hold.flush_now();
        }
    }

    fn write(&self, bytes: &[u8]) -> Result<(), String> {
        if super::classify_terminal_report(bytes, false).is_some() {
            return self.write_reply(bytes);
        }
        let input = self.input.get().ok_or("terminal input is not connected")?;
        (input.write)(bytes)
    }

    fn write_reply(&self, bytes: &[u8]) -> Result<(), String> {
        self.pty
            .try_write_bytes(bytes)
            .map_err(|error| error.to_string())
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        self.pty
            .try_resize(cols, rows)
            .map_err(|error| error.to_string())
    }

    fn stop(&self) {
        self.pty.kill();
    }

    fn start_reader(
        &self,
        _output: impl FnMut(Vec<u8>) + 'static,
        _exit: impl FnOnce(PtyExit) + 'static,
    ) -> Result<(), String> {
        Err("the pane's existing reader owns PTY output".into())
    }
}

impl SharedActiveTerminal {
    pub(super) fn new(config: &Config, pty: Rc<OwnedPty>) -> Result<Rc<Self>, String> {
        let input = Rc::new(OnceCell::new());
        let (widget, terminal) = EmbeddedTerminal::new(
            Transport {
                pty,
                input: input.clone(),
            },
            80,
            24,
            SharedTerminal::theme(config),
            |message, _| message.to_owned(),
            SharedTerminal::links(),
        )?;
        terminal
            .pane()
            .set_history_row_limit(config.terminal_scrollback_lines as usize);
        terminal
            .pane()
            .screen()
            .update_property(&[gtk::accessible::Property::Description(
                "Shared active terminal",
            )]);
        let transcript = config
            .terminal_mode
            .is_unified()
            .then(|| TranscriptView::new(terminal.clone()));
        Ok(Rc::new(Self {
            terminal,
            transcript,
            widget,
            input,
        }))
    }

    pub(super) fn transcript(&self) -> Option<&TranscriptView<Transport>> {
        self.transcript.as_ref()
    }

    pub(super) fn toggle_transcript_search(&self) -> bool {
        if self.transcript.is_none() {
            return false;
        }
        self.terminal.pane().toggle_history_search();
        true
    }

    pub(super) fn bind_input(&self, write: Input, hold: Rc<SelectionFeedHold>) {
        assert!(self.input.set(InputBinding { write, hold }).is_ok());
    }

    pub(super) fn screen(&self) -> &gtk::TextView {
        self.terminal.pane().screen()
    }

    pub(super) fn scrollback(&self) -> &gtk::Adjustment {
        self.terminal.scrollback_adjustment()
    }

    pub(super) fn copy(&self) {
        self.terminal.pane().copy();
    }

    pub(super) fn feed(&self, bytes: &[u8]) {
        if let Err(error) = self.terminal.ingest(bytes) {
            log::warn!("Could not process active terminal output: {error}");
        }
    }

    pub(super) fn exited(&self, status: PtyExit) {
        self.terminal.exited(status);
    }

    pub(super) fn on_event(&self, handler: impl Fn(PaneEvent) + 'static) {
        assert!(self.terminal.pane().set_event_handler(handler));
    }

    pub(super) fn grid_size(&self) -> (u16, u16) {
        self.terminal.pane().grid_size()
    }

    pub(super) fn fit_grid(&self, width: i32, height: i32) -> (u16, u16) {
        self.terminal.fit_grid(width, height)
    }

    pub(super) fn cell_metrics(&self) -> (i32, i32, i32, i32) {
        self.terminal.pane().cell_metrics()
    }

    pub(super) fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        self.terminal.resize(cols, rows)
    }

    pub(super) fn update_theme(&self, config: &Config, scale: f64) {
        self.terminal
            .pane()
            .set_theme(SharedTerminal::theme_at_scale(config, scale));
        self.terminal
            .pane()
            .set_history_row_limit(config.terminal_scrollback_lines as usize);
    }

    pub(super) fn drawn_rows(&self) -> i64 {
        self.terminal.inspect(|terminal| {
            terminal.frame().map_or(1, |frame| {
                let drawn = frame.rows().iter().rposition(|row| {
                    row.cells
                        .iter()
                        .any(|cell| !cell.is_reflow_trimmable_blank())
                });
                (1 + drawn.unwrap_or(0).max(frame.cursor().row)) as i64
            })
        })
    }

    pub(super) fn capture(&self, start: (i64, i64), end: (i64, i64)) -> Option<String> {
        let point = |(column, row): (i64, i64)| {
            Some(HistoryAnchor {
                row: row.try_into().ok()?,
                column: column.try_into().ok()?,
            })
        };
        let (start, end) = (point(start)?, point(end)?);
        self.terminal
            .inspect(|terminal| terminal.primary_text_range(start, end).ok())
    }

    pub(super) fn kitty_cursor(&self) -> Option<(i64, i64)> {
        self.terminal.inline_image_cursor()
    }

    pub(super) fn prepare_inline_image(
        &self,
        texture: gtk::gdk::Texture,
        encoded_source_bytes: usize,
        requested: super::kitty_graphics::DisplayPlacement,
    ) -> Option<jterm_gtk_view::PendingInlineImage> {
        requested.geometry_valid.then_some(())?;
        self.terminal.prepare_inline_image(
            texture,
            encoded_source_bytes,
            jterm_gtk_view::InlineImageRequest {
                columns: requested.columns,
                rows: requested.rows?,
                move_cursor: requested.cursor_moves?,
                cursor: (requested.cursor_col, requested.cursor_row),
            },
        )
    }

    pub(super) fn admit_inline_image(&self, pending: jterm_gtk_view::PendingInlineImage) -> bool {
        self.terminal.admit_inline_image(pending)
    }

    pub(super) fn cursor(&self) -> Option<(i64, i64)> {
        self.terminal.inspect(|terminal| {
            let anchor = terminal.primary_cursor_anchor()?;
            Some((i64::from(anchor.column), anchor.row.try_into().ok()?))
        })
    }
}

impl SubmissionSurface for SharedActiveTerminal {
    fn cursor_position(&self) -> (i64, i64) {
        self.cursor().unwrap_or((-1, -1))
    }

    fn row_count(&self) -> i64 {
        i64::from(self.grid_size().1)
    }

    fn prompt_anchor(&self, provisional: (i64, i64), _recorded_rows: i64) -> (i64, i64) {
        provisional
    }

    fn visible_editor_text(&self, anchor: (i64, i64)) -> Option<String> {
        self.capture(anchor, self.cursor()?)
    }

    fn suffix_is_empty(&self) -> Option<bool> {
        self.terminal
            .inspect(|terminal| terminal.primary_suffix_is_empty())
    }
}

/// The block parser admits OSC 7 as a single ST-terminated packet. Its
/// boundary remains intact during coalescing.
pub(super) fn directory_uri(bytes: &[u8]) -> Option<&str> {
    let payload = bytes.strip_prefix(b"\x1b]7;")?.strip_suffix(b"\x1b\\")?;
    std::str::from_utf8(payload).ok()
}
