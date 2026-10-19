//! Relm4 adapter around the forge block view.
//!
//! anvil's application shell expects terminal backends to be Relm4
//! components that speak `VteInit`/`VteInput`/`VteOutput`. The block-mode
//! implementation itself is now the forge `block_view::TermView`; this file
//! only adapts that GTK view to the existing anvil component surface.

use gtk::prelude::*;
use relm4::gtk;
use relm4::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;
use vte4::prelude::TerminalExt;

use crate::block_view::{
    FindNavigationResult, FindProgress, FindScope, FindSearchResult, RecordNavigationResult,
    TermView,
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
///
/// An interrupt or a stop (130, 141, 143, 148 — Ctrl+C, a closed pipe, a
/// kill, Ctrl+Z) is not a failure either: the card reads it as interrupted
/// with a neutral stripe, and badging the tab like a crash would cry wolf.
fn command_finished_output(exit_code: Option<i32>) -> VteOutput {
    VteOutput::CommandFinished(
        exit_code
            .is_none_or(|code| code == 0 || crate::block_view::interrupt_signal(code).is_some()),
    )
}

fn fold_all_notice(collapse: bool, folded: usize) -> String {
    if folded > 0 {
        let plural = if folded == 1 { "" } else { "s" };
        let verb = if collapse { "Collapsed" } else { "Expanded" };
        format!("{verb} {folded} block{plural}.")
    } else if collapse {
        "No blocks to collapse.".to_string()
    } else {
        "No blocks to expand.".to_string()
    }
}

fn toggle_collapsed_notice(changed: bool) -> Option<&'static str> {
    (!changed).then_some("No finished block to fold.")
}

/// Toast text for a session-export attempt.
fn export_notice(result: std::io::Result<std::path::PathBuf>) -> String {
    match result {
        Ok(path) => format!("Session exported to {}", path.display()),
        Err(err) => format!("Session export failed: {err}"),
    }
}

/// A separate nonmodal window leaves terminal input and tab switching available.
/// Only this main-context callback touches GTK; the worker owns plain data.
#[cfg(test)]
type PreviewTestHook = Box<dyn FnOnce() + Send>;
#[cfg(test)]
thread_local! {
    static PREVIEW_TEST_HOOK: RefCell<Option<PreviewTestHook>> = const { RefCell::new(None) };
}

/// Configure once, then preview an immutable worker-prepared snapshot. Closing
/// either stage never starts the filesystem publisher.
fn preview_session_export(
    view: &Rc<TermView>,
    slot: &Rc<RefCell<Option<crate::block_view::export::SessionExportJob>>>,
    dialogs: &Rc<RefCell<Option<gtk::Window>>>,
    sender: &relm4::Sender<VteOutput>,
    format: crate::block_view::SessionExportFormat,
) {
    use crate::block_view::export::{ExportPrivacy, ExportSelection};
    if slot.borrow().is_some() {
        return;
    }
    if let Some(window) = dialogs.borrow().as_ref() {
        window.present();
        return;
    }
    let window = gtk::Window::builder()
        .title("Review session export")
        .default_width(700)
        .default_height(520)
        .build();
    if let Some(parent) = view.widget().root().and_downcast::<gtk::Window>() {
        window.set_transient_for(Some(&parent));
        window.set_destroy_with_parent(true);
    }
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    let explanation = gtk::Label::new(Some("Choose all records, block IDs, or inclusive command-start Unix milliseconds. Unknown times are omitted from time ranges. Preview is limited to 64 KiB; retained output may already be truncated or unavailable."));
    explanation.set_wrap(true);
    content.append(&explanation);
    let mode = gtk::DropDown::from_strings(&[
        "All completed records",
        "Block IDs",
        "Time range (Unix ms)",
    ]);
    content.append(&mode);
    let ids = gtk::Entry::builder()
        .placeholder_text("Block IDs, comma-separated (example: 1,4,9)")
        .max_length(25000)
        .build();
    let start = gtk::Entry::builder()
        .placeholder_text("Start Unix milliseconds, inclusive")
        .max_length(20)
        .build();
    let end = gtk::Entry::builder()
        .placeholder_text("End Unix milliseconds, inclusive")
        .max_length(20)
        .build();
    content.append(&ids);
    content.append(&start);
    content.append(&end);
    let original =
        gtk::CheckButton::with_label("Export ORIGINAL text, including secrets and private paths");
    content.append(&original);
    let warning = gtk::Label::new(Some(crate::block_view::export_policy::WARNING));
    warning.set_wrap(true);
    content.append(&warning);
    let status = gtk::Label::new(None);
    status.set_wrap(true);
    content.append(&status);
    let text = gtk::TextView::new();
    text.set_editable(false);
    text.set_monospace(true);
    text.set_wrap_mode(gtk::WrapMode::WordChar);
    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&text)
        .build();
    content.append(&scroll);
    let preview = gtk::Button::with_label("Prepare redacted/original preview");
    let save = gtk::Button::with_label("Save reviewed export");
    save.set_sensitive(false);
    let cancel = gtk::Button::with_label("Cancel (write nothing)");
    content.append(&preview);
    content.append(&save);
    content.append(&cancel);
    window.set_child(Some(&content));
    let prepared = Rc::new(RefCell::new(
        None::<crate::block_view::export::SessionExportSnapshot>,
    ));
    let weak_window = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = weak_window.upgrade() {
            window.close();
        }
    });
    let weak_dialogs = Rc::downgrade(dialogs);
    let cancelled_preview = prepared.clone();
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_worker = cancelled.clone();
    window.connect_close_request(move |_| {
        cancel_worker.store(true, std::sync::atomic::Ordering::Release);
        cancelled_preview.borrow_mut().take();
        if let Some(dialogs) = weak_dialogs.upgrade() {
            dialogs.borrow_mut().take();
        }
        gtk::glib::Propagation::Proceed
    });
    let weak_view = Rc::downgrade(view);
    let weak_slot = Rc::downgrade(slot);
    let weak_window = window.downgrade();
    let output = sender.clone();
    let saved = prepared.clone();
    save.connect_clicked(move |button| {
        let (Some(view), Some(slot), Some(window)) = (
            weak_view.upgrade(),
            weak_slot.upgrade(),
            weak_window.upgrade(),
        ) else {
            return;
        };
        if slot.borrow().is_some() || !window.is_visible() {
            return;
        }
        let snapshot = saved.borrow_mut().take();
        if let Some(snapshot) = snapshot {
            button.set_sensitive(false);
            window.close();
            start_snapshot_export(&view, &slot, &output, format, snapshot);
        }
    });
    let weak_view = Rc::downgrade(view);
    let weak_slot = Rc::downgrade(slot);
    let weak_window = window.downgrade();
    preview.connect_clicked(move |button| {
        let selection = match mode.selected() {
            1 => ids.text().split(',').map(|id| id.trim().parse::<u64>()).collect::<Result<Vec<_>, _>>().map(|ids| ExportSelection::Ids {ids}).map_err(|_| "Enter comma-separated numeric block IDs."),
            2 => start.text().parse::<u64>().and_then(|start_ms| end.text().parse::<u64>().map(|end_ms| ExportSelection::TimeRange {start_ms,end_ms})).map_err(|_| "Enter numeric Unix milliseconds."),
            _ => Ok(ExportSelection::All),
        };
        let selection = match selection { Ok(s) => s, Err(e) => {status.set_text(e);return;} };
        let Some(view) = weak_view.upgrade() else { return; };
        let Some(permit) = crate::block_view::export_policy::PreviewPermit::try_acquire() else {
            status.set_text("Another preview is still preparing or finishing cancellation. No new snapshot was copied. Retry Prepare when it finishes.");
            return;
        };
        let snapshot = match view.selected_export_snapshot(&selection) {Ok(s) => s, Err(e) => {status.set_text(&e.to_string());return;} };
        let privacy = if original.is_active() {ExportPrivacy::Original} else {ExportPrivacy::Redacted};
        // Lock range and choice. A different choice requires Cancel and a fresh
        // dialog, so a stale worker result can never authorize another range.
        for widget in [mode.clone().upcast::<gtk::Widget>(), ids.clone().upcast(), start.clone().upcast(), end.clone().upcast(), original.clone().upcast()] {widget.set_sensitive(false);}
        button.set_sensitive(false); status.set_text("Preparing preview… no file has been written.");
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        #[cfg(test)]
        let test_hook = PREVIEW_TEST_HOOK.with(|slot| slot.borrow_mut().take());
        let cancelled = cancelled.clone();
        if let Err(e) = std::thread::Builder::new().name("export-preview".into()).spawn(move || {
            let _permit = permit;
            #[cfg(test)]
            if let Some(hook) = test_hook { hook(); }
            let result = if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "Preview cancelled; no file written."))
            } else {
                snapshot.prepare(privacy, format)
            };
            let _ = tx.send(result);
            // The permit remains held through calculation and result delivery,
            // including when the preview window was closed long ago.
        }) {status.set_text(&e.to_string());return;}
        let weak_window = weak_window.clone(); let weak_slot = weak_slot.clone();
        let text = text.clone(); let status = status.clone(); let save = save.clone(); let prepared = prepared.clone();
        gtk::glib::timeout_add_local(Duration::from_millis(50), move || {
            let Some(window) = weak_window.upgrade() else {return gtk::glib::ControlFlow::Break;};
            if weak_slot.upgrade().is_none() || !window.is_visible() { window.destroy(); return gtk::glib::ControlFlow::Break; }
            match rx.try_recv() {
                Ok(Ok(result)) => {
                    text.buffer().set_text(&result.preview); *prepared.borrow_mut() = Some(result.snapshot);
                    status.set_text(if privacy == ExportPrivacy::Original {"ORIGINAL unfiltered snapshot. Review before Save. No file written yet."} else {"Redacted snapshot. Review metadata and content before Save; heuristics may miss secrets."});
                    save.set_sensitive(true); gtk::glib::ControlFlow::Break
                }
                Ok(Err(e)) => {status.set_text(&e.to_string());gtk::glib::ControlFlow::Break}
                Err(std::sync::mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
                Err(_) => {status.set_text("Preview worker stopped. Cancel and retry.");gtk::glib::ControlFlow::Break}
            }
        });
    });
    *dialogs.borrow_mut() = Some(window.clone());
    window.present();
    // Pane closure also dismisses an idle or completed preview.
    let weak_slot = Rc::downgrade(slot);
    let weak_window = window.downgrade();
    gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
        let Some(window) = weak_window.upgrade() else {
            return gtk::glib::ControlFlow::Break;
        };
        if weak_slot.upgrade().is_none() {
            window.destroy();
            return gtk::glib::ControlFlow::Break;
        }
        if !window.is_visible() {
            return gtk::glib::ControlFlow::Break;
        }
        gtk::glib::ControlFlow::Continue
    });
}

#[cfg(test)]
fn start_session_export(
    view: &Rc<TermView>,
    slot: &Rc<RefCell<Option<crate::block_view::export::SessionExportJob>>>,
    sender: &relm4::Sender<VteOutput>,
    format: crate::block_view::SessionExportFormat,
) {
    if slot.borrow().is_some() {
        let _ = sender.send(VteOutput::Notice(
            "A session export is already running.".into(),
        ));
        return;
    }
    match view.session_export_snapshot() {
        Ok(snapshot) => start_snapshot_export(view, slot, sender, format, snapshot),
        Err(error) => {
            let _ = sender.send(VteOutput::Notice(export_notice(Err(error))));
        }
    }
}
fn start_snapshot_export(
    view: &Rc<TermView>,
    slot: &Rc<RefCell<Option<crate::block_view::export::SessionExportJob>>>,
    sender: &relm4::Sender<VteOutput>,
    format: crate::block_view::SessionExportFormat,
    snapshot: crate::block_view::export::SessionExportSnapshot,
) {
    if slot.borrow().is_some() {
        return;
    }
    let job = match snapshot.start(format) {
        Ok(job) => job,
        Err(error) => {
            let _ = sender.send(VteOutput::Notice(export_notice(Err(error))));
            return;
        }
    };
    let window = gtk::Window::builder()
        .title("Exporting session")
        .default_width(340)
        .modal(false)
        .build();
    if let Some(parent) = view.widget().root().and_downcast::<gtk::Window>() {
        window.set_transient_for(Some(&parent));
        window.set_destroy_with_parent(true);
    }
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    let label = gtk::Label::new(Some("Preparing export…"));
    let progress = gtk::ProgressBar::new();
    let cancel = gtk::Button::with_label("Cancel");
    content.append(&label);
    content.append(&progress);
    content.append(&cancel);
    window.set_child(Some(&content));
    let token = job.cancellation.clone();
    cancel.connect_clicked(move |button| {
        let accepted = token.cancel();
        button.set_sensitive(false);
        button.set_label(if accepted {
            "Cancelling…"
        } else {
            "Finishing…"
        });
    });
    let token = job.cancellation.clone();
    window.connect_close_request(move |_| {
        token.cancel();
        gtk::glib::Propagation::Proceed
    });
    *slot.borrow_mut() = Some(job);
    window.present();
    let weak_slot = Rc::downgrade(slot);
    let output = sender.clone();
    // Strong window reference lasts only until the worker result or pane close.
    gtk::glib::timeout_add_local(Duration::from_millis(50), move || {
        let Some(slot) = weak_slot.upgrade() else {
            window.destroy();
            return gtk::glib::ControlFlow::Break;
        };
        let outcome = {
            let borrowed = slot.borrow();
            let Some(job) = borrowed.as_ref() else {
                window.destroy();
                return gtk::glib::ControlFlow::Break;
            };
            let completed = job.completed.load(std::sync::atomic::Ordering::Relaxed);
            progress.set_fraction(if job.total == 0 {
                1.0
            } else {
                completed as f64 / job.total as f64
            });
            label.set_text(&if completed == job.total {
                "Saving export…".to_string()
            } else {
                format!("Exporting {completed} / {} records…", job.total)
            });
            match job.result.try_recv() {
                Ok(result) => Some(result),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some(Err(std::io::Error::other("export worker stopped")))
                }
            }
        };
        if let Some(result) = outcome {
            slot.borrow_mut().take();
            window.destroy();
            let message = match result {
                Err(ref error) if error.kind() == std::io::ErrorKind::Interrupted => {
                    "Session export cancelled.".to_string()
                }
                result => export_notice(result),
            };
            let _ = output.send(VteOutput::Notice(message));
            gtk::glib::ControlFlow::Break
        } else {
            gtk::glib::ControlFlow::Continue
        }
    });
    // The callback owns a clone, not this handle.
}

fn record_navigation_notice(result: RecordNavigationResult) -> Option<&'static str> {
    match result {
        RecordNavigationResult::LocationUnavailable => {
            Some("This record has no exact terminal location and no retained output snapshot.")
        }
        RecordNavigationResult::NoMatchingRecord => Some("No matching block."),
        // SnapshotView opens the read-only view instead of reporting anything.
        RecordNavigationResult::Navigated | RecordNavigationResult::SnapshotView { .. } => None,
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
    session_export: Rc<RefCell<Option<crate::block_view::export::SessionExportJob>>>,
    session_export_dialog: Rc<RefCell<Option<gtk::Window>>>,
    /// Last result belongs to the regex currently installed in this pane.
    /// Keeping it backend-side prevents navigation after a compile error from
    /// turning that diagnostic into a misleading `(0, 0)` result.
    search_status: SearchStatus,
    /// Query that produced `search_status`. A card resize/filter/expand can
    /// invalidate VTE's native cursor between Next/Previous actions; retaining
    /// the query lets the adapter rebuild the pass instead of claiming the
    /// still-visible text disappeared.
    search_query: Option<(String, bool, bool)>,
    /// `search_status` came from the live terminal's own search, run while an
    /// alternate-screen app owned the pane, rather than from the blocks.
    search_live: bool,
}

/// Run a Find in the scope the pane has now: the finished blocks, or only the
/// live screen while an alternate-screen app owns the pane and the cards are
/// hidden (see [`FindScope`]). Returns the status and whether it searched the
/// live screen.
fn search_in_scope(
    view: &TermView,
    query: &str,
    use_regex: bool,
    case_sensitive: bool,
) -> (SearchStatus, bool) {
    view.clear_unified_navigation_cursor();
    if view.find_scope() == FindScope::LiveScreen {
        // Drops any block hits (and the live terminal's regex) first.
        view.clear_find();
        return (
            super::vte::search_terminal(view.vte(), query, use_regex, case_sensitive),
            true,
        );
    }
    let pattern = super::vte::search_pattern(query, use_regex);
    let status = match validate_block_search_pattern(&pattern, case_sensitive) {
        Ok(_) => block_result_status(
            view.find_in_blocks(query, use_regex, case_sensitive),
            use_regex,
        ),
        Err(error) => {
            view.clear_find();
            SearchStatus::Error(error)
        }
    };
    (status, false)
}

fn validate_block_search_pattern(pattern: &str, case_sensitive: bool) -> Result<(), String> {
    super::vte::compile_count_regex(pattern, case_sensitive)?;
    vte4::Regex::for_search(pattern, super::vte::search_pcre2_flags(case_sensitive))
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

/// What a Find Next/Previous does, given where the last search ran and the
/// pane's scope now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FindStep {
    /// Step the live screen's native search.
    Native,
    /// Search again in the pane's current scope: the scope changed under a
    /// Find left open, or it had nothing to step through when an
    /// alternate-screen app took the pane over.
    Rescope,
    /// Step the block hits.
    Blocks,
}

fn find_step(search_live: bool, live_now: bool, block_hits: bool, has_query: bool) -> FindStep {
    match (search_live, live_now) {
        (true, true) => FindStep::Native,
        (true, false) => FindStep::Rescope,
        // No block hits to step (none found, an error, idle): stepping them
        // would do nothing, so a query left open searches the app's screen.
        (false, true) if !block_hits && has_query => FindStep::Rescope,
        (false, _) => FindStep::Blocks,
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

    pub(crate) fn selected_block_agent_evidence(
        &self,
        max_output_lines: usize,
    ) -> Option<crate::block_view::BlockAgentEvidence> {
        self.view
            .as_ref()
            .and_then(|view| view.selected_block_agent_evidence(max_output_lines))
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
            move |command,
                  exit_code,
                  completion_provenance,
                  output_sample,
                  agent_execution,
                  duration_ms| {
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
                    completion_provenance,
                    output_sample: output_sample.unwrap_or_default(),
                    agent_execution,
                    duration_ms,
                });
            }
        },
    );
    view.connect_ask_ai_about_block({
        let sender = sender.clone();
        move |context, intent| {
            let _ = sender.output(VteOutput::AskAiAboutBlock(context, intent));
        }
    });
    view.connect_fix_block_with_agent({
        let sender = sender.clone();
        move || {
            let _ = sender.output(VteOutput::FixBlockWithAgent);
        }
    });
    view.connect_delete_selected_blocks({
        let sender = sender.clone();
        move || {
            sender.input(VteInput::DeleteSelectedBlocks);
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
                &init.env_extra,
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
                TermView::install_unified_navigation(&view, {
                    let sender = sender.clone();
                    move |action| match action {
                        crate::keybindings::Action::PreviousCommand => {
                            sender.input(VteInput::PreviousCommand)
                        }
                        crate::keybindings::Action::NextCommand => {
                            sender.input(VteInput::NextCommand)
                        }
                        crate::keybindings::Action::ReturnToLive => {
                            sender.input(VteInput::ReturnToLive)
                        }
                        crate::keybindings::Action::CopyCurrentCommand => {
                            sender.input(VteInput::CopyCurrentCommand)
                        }
                        crate::keybindings::Action::CrossBlockSearch => {
                            sender.input(VteInput::CrossBlockSearch)
                        }
                        crate::keybindings::Action::ToggleSearch => {
                            let _ = sender.output(VteOutput::FindRequested);
                        }
                        _ => {}
                    }
                });
                // The live card had no Ctrl+click at all: a running command's
                // URLs and claude's OSC 8 links underlined on hover and did
                // nothing. The handler denies every other click, so selection,
                // click-to-move and a program's mouse reporting are untouched.
                super::vte::setup_terminal_click_handler(view.vte());
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
            session_export: Rc::new(RefCell::new(None)),
            session_export_dialog: Rc::new(RefCell::new(None)),
            search_status: SearchStatus::Idle,
            search_query: None,
            search_live: false,
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
            VteInput::InsertReviewText(data) => {
                crate::enter_ownership::claim_held();
                view.write_input(&data);
            }
            VteInput::PasteText(text) => {
                crate::enter_ownership::claim_held();
                view.paste_text(&text);
            }
            VteInput::RunAgentCommand { execution, command } => {
                if !view.try_run_agent_command(execution, &command) {
                    let _ = sender.output(VteOutput::AgentExecutionStartFailed { execution });
                }
            }
            VteInput::Resize(cols, rows) => view.resize(cols, rows),
            VteInput::GrabFocus => view.grab_focus(),
            VteInput::Copy => {
                if let Some(message) = super::vte::copy_empty_notice(view.copy_to_clipboard()) {
                    let _ = sender.output(VteOutput::Notice(message.to_string()));
                }
            }
            VteInput::CopyOutputOnly => {
                if let Some(message) =
                    super::vte::copy_empty_notice(view.copy_to_clipboard_with_modifier(true))
                {
                    let _ = sender.output(VteOutput::Notice(message.to_string()));
                }
            }
            VteInput::Paste => view.paste_from_clipboard(),
            VteInput::SetFontScale(scale) => view.set_font_scale(scale),
            VteInput::SetFont(desc) => view.set_font(&desc),
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
            VteInput::DeleteSelectedBlocks => {
                let deleted = view.delete_selected_blocks();
                if deleted > 0 {
                    let plural = if deleted == 1 { "" } else { "s" };
                    let _ = sender.output(VteOutput::NoticeWithUndo {
                        message: format!("Deleted {deleted} block{plural}."),
                        button: "Undo".to_string(),
                        undo: crate::terminal::NoticeUndo::ClearBlocks,
                    });
                } else if view.is_unified() {
                    let _ = sender.output(VteOutput::Notice(
                        "Unified mode keeps no block cards to delete.".to_string(),
                    ));
                } else {
                    let _ = sender.output(VteOutput::Notice(
                        "Select one or more finished blocks first.".to_string(),
                    ));
                }
            }
            VteInput::CollapseAllBlocks | VteInput::ExpandAllBlocks => {
                let collapse = matches!(msg, VteInput::CollapseAllBlocks);
                let folded = view.set_all_blocks_collapsed(collapse);
                let _ = sender.output(VteOutput::Notice(fold_all_notice(collapse, folded)));
            }
            VteInput::CollapseSelectedBlocks | VteInput::ExpandSelectedBlocks => {
                let collapse = matches!(msg, VteInput::CollapseSelectedBlocks);
                let result = view.set_selected_blocks_collapsed(collapse);
                let _ = sender.output(VteOutput::Notice(result.notice(collapse)));
            }
            VteInput::ToggleBlockCollapsed => {
                if let Some(message) =
                    toggle_collapsed_notice(view.toggle_selected_block_collapsed())
                {
                    let _ = sender.output(VteOutput::Notice(message.to_string()));
                }
            }
            VteInput::ReinputSelectedCommands => {
                if let Some(message) = view.reinput_selected_commands() {
                    let _ = sender.output(VteOutput::Notice(message.to_string()));
                }
            }
            VteInput::JumpToPrevPinned => {
                report_record_navigation(&sender, view, &snapshot_dialog, view.jump_to_pinned(-1))
            }
            VteInput::JumpToNextPinned => {
                report_record_navigation(&sender, view, &snapshot_dialog, view.jump_to_pinned(1))
            }
            VteInput::JumpToPrevFailed => {
                report_record_navigation(&sender, view, &snapshot_dialog, view.jump_to_failed(-1))
            }
            VteInput::JumpToNextFailed => {
                report_record_navigation(&sender, view, &snapshot_dialog, view.jump_to_failed(1))
            }
            VteInput::PreviousCommand | VteInput::NextCommand => {
                match view.navigate_unified_command(matches!(msg, VteInput::PreviousCommand)) {
                    Ok(result) => report_record_navigation(&sender, view, &snapshot_dialog, result),
                    Err(message) => {
                        let _ = sender.output(VteOutput::Notice(message.into()));
                    }
                }
            }
            VteInput::ReturnToLive => {
                if let Err(message) = view.return_to_live() {
                    let _ = sender.output(VteOutput::Notice(message.into()));
                }
            }
            VteInput::CopyCurrentCommand => {
                let message = view
                    .copy_current_unified_command()
                    .map_or_else(|message| message, |_| "Copied the current command.");
                let _ = sender.output(VteOutput::Notice(message.into()));
            }
            VteInput::ExportSessionMarkdown | VteInput::ExportSessionJson => {
                let format = if matches!(msg, VteInput::ExportSessionJson) {
                    crate::block_view::SessionExportFormat::Json
                } else {
                    crate::block_view::SessionExportFormat::Markdown
                };
                preview_session_export(
                    view,
                    &self.session_export,
                    &self.session_export_dialog,
                    sender.output_sender(),
                    format,
                );
            }
            VteInput::SearchSet(query, use_regex, case_sensitive) => {
                self.search_status = if let Some(status) =
                    crate::search::oversize_query_status(&query)
                {
                    // Do not retain the oversized query: an invalidated Find
                    // rebuilds from `search_query` and must not re-scan with it.
                    self.search_query = None;
                    self.search_live = false;
                    view.clear_find();
                    status
                } else {
                    let (status, live) = search_in_scope(view, &query, use_regex, case_sensitive);
                    self.search_query = Some((query, use_regex, case_sensitive));
                    self.search_live = live;
                    status
                };
                let _ = sender.output(VteOutput::SearchStatus(self.search_status.clone()));
            }
            VteInput::SearchNext | VteInput::SearchPrev => {
                view.clear_unified_navigation_cursor();
                let step: isize = if matches!(msg, VteInput::SearchNext) {
                    1
                } else {
                    -1
                };
                let live_now = view.find_scope() == FindScope::LiveScreen;
                let active = matches!(
                    self.search_status,
                    SearchStatus::Results { total, .. } if total > 0
                );
                let plan = find_step(
                    self.search_live,
                    live_now,
                    active,
                    self.search_query.is_some(),
                );
                if plan == FindStep::Native {
                    // The alternate-screen app's screen, searched natively.
                    let found = if step > 0 {
                        view.vte().search_find_next()
                    } else {
                        view.vte().search_find_previous()
                    };
                    self.search_status = self.search_status.stepped(step, found);
                } else if plan == FindStep::Rescope {
                    // The app left and the blocks are back, or it arrived while
                    // the blocks had nothing to step: search the pane's scope.
                    self.search_status = self.search_query.as_ref().map_or_else(
                        || SearchStatus::results(0, 0),
                        |(query, use_regex, case_sensitive)| {
                            let (status, live) =
                                search_in_scope(view, query, *use_regex, *case_sensitive);
                            self.search_live = live;
                            status
                        },
                    );
                } else {
                    let partial = matches!(
                        self.search_status,
                        SearchStatus::Results {
                            truncated: true,
                            ..
                        }
                    );
                    // Even an old zero-hit result can be invalidated by new
                    // output. Ask the view before deciding to keep its count.
                    let result = if step > 0 {
                        view.find_next()
                    } else {
                        view.find_prev()
                    };
                    self.search_status = if result == FindNavigationResult::Invalidated {
                        // Rebuilt in whatever scope the pane has now, so a
                        // Find left open when an alternate-screen app took
                        // over moves to its screen.
                        self.search_query.as_ref().map_or_else(
                            || SearchStatus::results(0, 0),
                            |(query, use_regex, case_sensitive)| {
                                let (status, live) =
                                    search_in_scope(view, query, *use_regex, *case_sensitive);
                                self.search_live = live;
                                status
                            },
                        )
                    } else {
                        block_navigation_status(&self.search_status, result, partial)
                    };
                }
                let _ = sender.output(VteOutput::SearchStatus(self.search_status.clone()));
            }
            VteInput::SearchClear => {
                // Clears the live terminal's native search too.
                view.clear_find();
                self.search_status = SearchStatus::Idle;
                self.search_query = None;
                self.search_live = false;
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
                    let _ = sender.output(VteOutput::AskAiAboutBlock(
                        context,
                        crate::ai::BlockAiIntent::Ask,
                    ));
                } else {
                    let _ = sender.output(VteOutput::Notice(
                        super::vte::ask_ai_empty_notice().to_string(),
                    ));
                }
            }
        }
    }

    fn shutdown(&mut self, _widgets: &mut Self::Widgets, _output: relm4::Sender<Self::Output>) {
        self.session_export.borrow_mut().take();
        if let Some(window) = self.session_export_dialog.borrow_mut().take() {
            window.destroy();
        }
        self.terminate_once();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        block_navigation_status, block_result_status, command_finished_output, find_step,
        fold_all_notice, record_navigation_notice, toggle_collapsed_notice,
        validate_block_search_pattern, FindNavigationResult, FindProgress, FindSearchResult,
        FindStep, RecordNavigationResult, VteOutput,
    };
    use crate::search::SearchStatus;

    #[test]
    #[ignore = "requires GTK display; registered in test-gtk-display.sh"]
    fn session_export_preview_cancel_and_explicit_original_are_safe() {
        use crate::block_view::export_policy::PreviewPermit;
        use gtk::prelude::*;
        use relm4::gtk;
        use std::{
            cell::RefCell,
            rc::Rc,
            time::{Duration, Instant},
        };
        fn pump() {
            let context = gtk::glib::MainContext::default();
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        fn wait(check: impl Fn() -> bool) {
            let end = Instant::now() + Duration::from_secs(5);
            while !check() {
                assert!(Instant::now() < end, "A06 native condition timed out");
                pump();
            }
        }
        fn children(window: &gtk::Window) -> Vec<gtk::Widget> {
            let content = window.child().unwrap();
            let mut result = Vec::new();
            let mut current = content.first_child();
            while let Some(widget) = current {
                current = widget.next_sibling();
                result.push(widget);
            }
            result
        }
        fn button(window: &gtk::Window, label: &str) -> gtk::Button {
            children(window)
                .into_iter()
                .filter_map(|w| w.downcast::<gtk::Button>().ok())
                .find(|b| b.label().as_deref() == Some(label))
                .unwrap()
        }
        fn original(window: &gtk::Window) -> gtk::CheckButton {
            children(window)
                .into_iter()
                .find_map(|w| w.downcast::<gtk::CheckButton>().ok())
                .unwrap()
        }
        fn visible_preview(window: &gtk::Window) -> String {
            let scroll = children(window)
                .into_iter()
                .find_map(|w| w.downcast::<gtk::ScrolledWindow>().ok())
                .unwrap();
            let text = scroll.child().unwrap().downcast::<gtk::TextView>().unwrap();
            let buffer = text.buffer();
            buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .to_string()
        }
        fn assert_content(text: &str, unfiltered: bool) {
            assert!(text.contains("SAFE_NATIVE_EXPORT_MARKER"));
            for secret in [
                "fixture-native-prompt",
                "fixture-native-command",
                "fixture-native-markup",
                "fixture-native-output",
                "fixture-native-directory",
            ] {
                assert_eq!(
                    text.contains(secret),
                    unfiltered,
                    "unexpected privacy for {secret}: {text}"
                );
            }
        }
        crate::child_env::capture_inherited_environment().unwrap();
        gtk::init().unwrap();
        let view = crate::block_view::organism_settings_test_view();
        crate::block_view::export::seed_preview_fixture(&view);
        let source_before = view.export_session_json();
        assert_content(&source_before, true);
        let parent = gtk::Window::builder().child(&view.widget()).build();
        parent.present();
        let slot = Rc::new(RefCell::new(None));
        let dialogs = Rc::new(RefCell::new(None));
        let (sender, _received) = relm4::channel();
        let open = || {
            super::preview_session_export(
                &view,
                &slot,
                &dialogs,
                &sender,
                crate::block_view::SessionExportFormat::Json,
            );
            dialogs.borrow().as_ref().unwrap().clone()
        };
        let directory = gtk::glib::user_data_dir().join("anvil/exports");
        let files = || {
            std::fs::read_dir(&directory)
                .map(|entries| {
                    entries
                        .map(|e| e.unwrap().path())
                        .collect::<std::collections::BTreeSet<_>>()
                })
                .unwrap_or_default()
        };
        let before = files();
        // Cancel before preparation. Repeated open must reuse the same window.
        let window = open();
        assert_eq!(open(), window);
        assert!(!original(&window).is_active());
        let stale_save = button(&window, "Save reviewed export");
        assert!(!stale_save.is_sensitive());
        window.close();
        wait(|| dialogs.borrow().is_none());
        stale_save.emit_clicked();
        assert_eq!(files(), before);

        // Hold the actual worker after admission, before CPU preparation. Cancel
        // cannot free its permit or allow a second captured snapshot/worker.
        let (entered, entered_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        super::PREVIEW_TEST_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                entered.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }))
        });
        let window = open();
        let stale_save = button(&window, "Save reviewed export");
        button(&window, "Prepare redacted/original preview").emit_clicked();
        wait(|| entered_rx.try_recv().is_ok());
        assert!(PreviewPermit::is_running());
        assert!(!original(&window).is_sensitive());
        window.close();
        wait(|| dialogs.borrow().is_none());
        stale_save.emit_clicked();
        let busy = open();
        button(&busy, "Prepare redacted/original preview").emit_clicked();
        assert!(children(&busy)
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::Label>().ok())
            .any(|label| label.text().contains("finishing cancellation")));
        assert!(original(&busy).is_sensitive());
        assert!(!button(&busy, "Save reviewed export").is_sensitive());
        assert!(PreviewPermit::is_running());
        assert_eq!(files(), before);
        busy.close();
        wait(|| dialogs.borrow().is_none());
        release.send(()).unwrap();
        wait(|| !PreviewPermit::is_running());
        for _ in 0..25 {
            pump();
        }
        stale_save.emit_clicked();
        assert_eq!(files(), before);
        assert!(slot.borrow().is_none());
        assert_eq!(view.export_session_json(), source_before);

        // Cancel a completed, visibly redacted nonempty preview, then deliberately
        // emit a stale Save signal to test invalidation rather than visibility alone.
        let window = open();
        button(&window, "Prepare redacted/original preview").emit_clicked();
        let stale_save = button(&window, "Save reviewed export");
        wait(|| stale_save.is_sensitive());
        assert_content(&visible_preview(&window), false);
        window.close();
        wait(|| dialogs.borrow().is_none());
        stale_save.emit_clicked();
        assert_eq!(files(), before);
        assert_eq!(view.export_session_json(), source_before);

        // Exercise real default-redacted and explicit-Original Save paths, with
        // ID and timestamp selection respectively. Compare the visible preview
        // to the exact private file, not a separately reconstructed serializer.
        for unfiltered in [false, true] {
            wait(|| !PreviewPermit::is_running());
            let before_save = files();
            let window = open();
            assert!(!original(&window).is_active());
            original(&window).set_active(unfiltered);
            let mode = children(&window)
                .into_iter()
                .find_map(|w| w.downcast::<gtk::DropDown>().ok())
                .unwrap();
            let entries: Vec<_> = children(&window)
                .into_iter()
                .filter_map(|w| w.downcast::<gtk::Entry>().ok())
                .collect();
            if unfiltered {
                mode.set_selected(2);
                entries[1].set_text("100");
                entries[2].set_text("100");
            } else {
                mode.set_selected(1);
                entries[0].set_text("9001");
            }
            button(&window, "Prepare redacted/original preview").emit_clicked();
            let save = button(&window, "Save reviewed export");
            wait(|| save.is_sensitive());
            assert!(!original(&window).is_sensitive());
            assert!(!mode.is_sensitive());
            let preview = visible_preview(&window);
            assert_content(&preview, unfiltered);
            assert_eq!(files(), before_save);
            save.emit_clicked();
            wait(|| slot.borrow().is_none());
            let after = files();
            let created: Vec<_> = after.difference(&before_save).collect();
            assert_eq!(created.len(), 1);
            let bytes = std::fs::read(created[0]).unwrap();
            let saved = String::from_utf8(bytes).unwrap();
            assert_eq!(saved, preview);
            assert_content(&saved, unfiltered);
            let json: serde_json::Value = serde_json::from_str(&saved).unwrap();
            assert_eq!(
                json["export_metadata"]["privacy"],
                if unfiltered { "original" } else { "redacted" }
            );
            assert_eq!(json["export_metadata"]["omitted_records"], 1);
            assert_eq!(json["records"].as_array().unwrap().len(), 1);
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(created[0]).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(view.export_session_json(), source_before);
        }
        parent.destroy();
        view.kill();
    }

    #[test]
    #[ignore = "requires GTK display, session bus and xdotool; registered in test-gtk-display.sh"]
    fn session_export_slow_fsync_keeps_native_input_and_tabs_responsive() {
        use gtk::prelude::*;
        use relm4::gtk;
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::{Duration, Instant};
        fn pump(duration: Duration) {
            let end = Instant::now() + duration;
            let context = gtk::glib::MainContext::default();
            while Instant::now() < end {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        fn wait(condition: impl Fn() -> bool) {
            let end = Instant::now() + Duration::from_secs(5);
            while !condition() {
                assert!(Instant::now() < end, "GUI export test timed out");
                pump(Duration::from_millis(10));
            }
        }
        crate::child_env::capture_inherited_environment().unwrap();
        gtk::init().unwrap();
        let view = crate::block_view::organism_settings_test_view();
        let entry = gtk::Entry::new();
        let second = gtk::Entry::new();
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.append(&entry);
        page.append(&view.widget());
        let tabs = gtk::Notebook::new();
        tabs.append_page(&page, Some(&gtk::Label::new(Some("Terminal"))));
        tabs.append_page(&second, Some(&gtk::Label::new(Some("Second pane"))));
        let window = gtk::Window::builder()
            .title("A01 export responsiveness")
            .default_width(640)
            .default_height(480)
            .child(&tabs)
            .build();
        window.present();
        wait(|| entry.is_mapped());
        let slot = Rc::new(RefCell::new(None));
        let (output, received) = relm4::channel();
        let notices = Rc::new(RefCell::new(Vec::new()));
        let observed = notices.clone();
        gtk::glib::spawn_future_local(async move {
            while let Some(message) = received.recv().await {
                if let super::VteOutput::Notice(message) = message {
                    observed.borrow_mut().push(message);
                }
            }
        });
        let directory = gtk::glib::user_data_dir().join("anvil/exports");
        let before = std::fs::read_dir(&directory)
            .map(|d| d.count())
            .unwrap_or(0);
        for close_pane in [false, true] {
            let (entered, entered_rx) = std::sync::mpsc::channel();
            crate::block_view::export::set_test_sync(move |file| {
                entered.send(()).unwrap();
                std::thread::sleep(Duration::from_secs(2));
                file.sync_all()
            });
            super::start_session_export(
                &view,
                &slot,
                &output,
                crate::block_view::SessionExportFormat::Json,
            );
            wait(|| entered_rx.try_recv().is_ok());
            assert!(slot.borrow().is_some());
            let now = Instant::now();
            window.present();
            tabs.set_current_page(Some(0));
            entry.grab_focus();
            pump(Duration::from_millis(80));
            let result = std::process::Command::new("xdotool")
                .args(["search", "--name", "^A01 export responsiveness$"])
                .output()
                .unwrap();
            assert!(result.status.success());
            let id = String::from_utf8(result.stdout).unwrap();
            let id = id.lines().last().unwrap();
            assert!(std::process::Command::new("xdotool")
                .args([
                    "windowfocus",
                    "--sync",
                    id,
                    "key",
                    "ctrl+a",
                    "type",
                    "responsive"
                ])
                .status()
                .unwrap()
                .success());
            wait(|| entry.text() == "responsive");
            // Deliver real X11 key events through TermView/VTE into the kernel
            // PTY while fsync is sleeping. An auxiliary entry alone would only
            // establish GTK-loop responsiveness, not terminal input delivery.
            view.grab_focus();
            pump(Duration::from_millis(50));
            crate::block_view::organism_settings_test_pty_bytes(&view);
            assert!(std::process::Command::new("xdotool")
                .args(["type", "terminal-responsive"])
                .status()
                .unwrap()
                .success());
            let delivered = RefCell::new(Vec::new());
            wait(|| {
                delivered
                    .borrow_mut()
                    .extend(crate::block_view::organism_settings_test_pty_bytes(&view));
                delivered
                    .borrow()
                    .windows(b"terminal-responsive".len())
                    .any(|bytes| bytes == b"terminal-responsive")
            });
            entry.grab_focus();
            pump(Duration::from_millis(30));

            assert!(std::process::Command::new("xdotool")
                .args(["key", "ctrl+Page_Down"])
                .status()
                .unwrap()
                .success());
            wait(|| tabs.current_page() == Some(1));
            assert!(
                now.elapsed() < Duration::from_millis(1500),
                "input/tab interaction waited for 2-second fsync"
            );
            let notices_before = notices.borrow().len();
            if close_pane {
                // Exact shutdown path: dropping the job cancels; the next main
                // tick retires the UI callback without posting stale results.
                slot.borrow_mut().take();
            } else {
                // Closing the real progress window requests cancellation.
                let export_window = gtk::Window::list_toplevels()
                    .into_iter()
                    .filter_map(|w| w.downcast::<gtk::Window>().ok())
                    .find(|w| w.title().as_deref() == Some("Exporting session"))
                    .unwrap();
                export_window.close();
            }
            wait(|| slot.borrow().is_none());
            pump(Duration::from_millis(2200));
            if close_pane {
                assert_eq!(notices.borrow().len(), notices_before);
            } else {
                assert!(notices.borrow().last().unwrap().contains("cancelled"));
            }
            assert_eq!(
                std::fs::read_dir(&directory)
                    .map(|d| d.count())
                    .unwrap_or(0),
                before
            );
        }
        window.destroy();
        view.kill();
    }

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
        // Ctrl+C, a SIGTERM and Ctrl+Z read as activity, a SIGKILL as a failure.
        for code in [130, 143, 148] {
            assert!(
                matches!(
                    command_finished_output(Some(code)),
                    VteOutput::CommandFinished(true)
                ),
                "{code}"
            );
        }
        assert!(matches!(
            command_finished_output(Some(137)),
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
    fn a_find_with_nothing_to_step_moves_to_an_alternate_screen_app() {
        // Opened at the prompt with no block hits, then opencode took over:
        // Next searches its screen instead of stepping nothing.
        assert_eq!(find_step(false, true, false, true), FindStep::Rescope);
        // Block hits still step (their pass rebuilds itself once invalidated).
        assert_eq!(find_step(false, true, true, true), FindStep::Blocks);
        assert_eq!(find_step(false, false, false, true), FindStep::Blocks);
        // Without a query there is nothing to search again.
        assert_eq!(find_step(false, true, false, false), FindStep::Blocks);
        assert_eq!(find_step(true, true, false, true), FindStep::Native);
        assert_eq!(find_step(true, false, true, true), FindStep::Rescope);
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
            Some("No matching block.")
        );
        assert_eq!(
            record_navigation_notice(RecordNavigationResult::SnapshotView { record_id: 7 }),
            None,
            "the snapshot view, not a notice, answers this result"
        );
    }

    #[test]
    fn fold_shortcuts_explain_an_empty_or_unchanged_document() {
        assert_eq!(fold_all_notice(true, 3), "Collapsed 3 blocks.");
        assert_eq!(fold_all_notice(false, 1), "Expanded 1 block.");
        assert_eq!(fold_all_notice(true, 0), "No blocks to collapse.");
        assert_eq!(fold_all_notice(false, 0), "No blocks to expand.");
        assert_eq!(
            toggle_collapsed_notice(false),
            Some("No finished block to fold.")
        );
        assert_eq!(toggle_collapsed_notice(true), None);
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
        assert!(super::super::vte::compile_count_regex("(?-u:a)", false).is_ok());
        let error = validate_block_search_pattern("(?-u:a)", false).unwrap_err();
        assert!(error.starts_with("Invalid regex:"));
    }
}
