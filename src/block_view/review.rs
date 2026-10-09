//! A read-only, identity-bound review of finished cards. Output stays in its
//! native VTE (including images); opening review never copies a transcript.
use super::*;
use relm4::adw;
use relm4::adw::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReviewRecord {
    id: u64,
    command: String,
    // Raw folder remains part of identity even if two escaped displays collide.
    cwd: Option<String>,
    context: String,
    recallable: bool,
}

impl ReviewRecord {
    fn from_record(record: &BlockData) -> Self {
        let outcome = match block_status(Some(&record.cmd), record.exit_code) {
            BlockStatus::Background => "Background output; no command result".to_string(),
            BlockStatus::Succeeded => "Succeeded (exit 0)".to_string(),
            BlockStatus::Interrupted(code) if jterm_core::exit_status::is_job_stop(code) => {
                format!("Suspended (exit {code})")
            }
            BlockStatus::Interrupted(code) => format!("Interrupted (exit {code})"),
            BlockStatus::Failed(code) => format!("Failed (exit {code})"),
            BlockStatus::Unreported => "Exit status unavailable".to_string(),
        };
        let duration = if record.is_background() {
            Some("Not applicable (no command)".to_string())
        } else if record.timing_is_authoritative() {
            record
                .duration_ms
                .map(|ms| format!("{} ({ms} ms)", format_block_duration(ms)))
        } else {
            None
        }
        .unwrap_or_else(|| "Unavailable".to_string());
        let source = if record.command_truncated {
            "Shortened command; insertion disabled"
        } else if record.command_exact {
            "Exact shell-reported command"
        } else {
            "Captured command; shell did not verify exact text"
        };
        let output = if record.output_head_dropped {
            "Earlier output not retained; card contains only the retained tail"
        } else {
            "Retained output is on the card; images remain in the native terminal"
        };
        let lifecycle = record.lifecycle_notice().unwrap_or_else(|| {
            if record.is_background() {
                "Background output (no command)".to_string()
            } else {
                "Shell-reported completion".to_string()
            }
        });
        Self {
            id: record.id,
            command: record.cmd.clone(),
            cwd: record.cwd.clone(),
            context: format!("Folder: {}\nOutcome: {outcome}\nDuration: {duration}\nCommand: {source}\nLifecycle: {lifecycle}\nOutput: {output}", review_display(record.cwd.as_deref().unwrap_or("Unavailable"), false)),
            recallable: !record.command_truncated,
        }
    }
}

/// Render full reviewed text without invisible terminal/bidi effects. The review
/// admission budget bounds input; escapes expand each scalar to at most 10 bytes.
/// This is presentation only: action payloads and identity retain raw text.
fn review_display(text: &str, multiline: bool) -> String {
    let mut display = String::with_capacity(text.len());
    for ch in text.chars() {
        if multiline && matches!(ch, '\n' | '\t') {
            display.push(ch);
        } else if ch.is_control() || crate::review_input::is_terminal_visual_spoofing_character(ch)
        {
            match ch {
                '\n' => display.push_str(r"\n"),
                '\r' => display.push_str(r"\r"),
                '\t' => display.push_str(r"\t"),
                _ => {
                    use std::fmt::Write;
                    write!(display, "\\u{{{:X}}}", ch as u32).expect("write string");
                }
            }
        } else {
            display.push(ch);
        }
    }
    display
}

// Keep a multi-selection from constructing thousands of GTK text views or
// duplicating unbounded command/path strings. Refuse the whole review, never a
// silently shortened insertion. Single-card commands retain their full text.
fn review_fits(records: &VecDeque<BlockData>, ids: &HashSet<u64>) -> bool {
    let mut count = 0usize;
    let mut bytes = 0usize;
    for record in records.iter().filter(|record| ids.contains(&record.id)) {
        count += 1;
        bytes = bytes
            .saturating_add(record.cmd.len())
            .saturating_add(record.cwd.as_deref().map_or(0, str::len));
        if count > 128 || bytes > 2 * 1024 * 1024 {
            return false;
        }
    }
    true
}

fn records_for_review(records: &VecDeque<BlockData>, ids: &HashSet<u64>) -> Vec<ReviewRecord> {
    records
        .iter()
        .filter(|record| ids.contains(&record.id))
        .map(ReviewRecord::from_record)
        .collect()
}

fn complete_reviewed_command(records: &[ReviewRecord], ids: &HashSet<u64>) -> String {
    if records.len() != ids.len() || records.iter().any(|record| !ids.contains(&record.id)) {
        return String::new();
    }
    reviewed_command(records)
}

fn reviewed_command(records: &[ReviewRecord]) -> String {
    if records.iter().any(|record| !record.recallable) {
        return String::new();
    }
    selected_command_text(
        records
            .iter()
            .map(|record| (record.id, record.command.as_str())),
        &records.iter().map(|record| record.id).collect(),
    )
}

fn review_recall_is_lossless(command: &str, bracketed: bool) -> bool {
    if command.chars().any(|ch| {
        !matches!(ch, '\n' | '\t')
            && (ch.is_control() || crate::review_input::is_terminal_visual_spoofing_character(ch))
    }) {
        return false;
    }
    let recall = build_command_recall(command, bracketed);
    !recall.is_empty()
        && !recall.risk.truncated_to_first_line
        && !recall.risk.had_controls
        && !recall.risk.had_embedded_paste_marker
}

fn review_is_current(snapshot: &[ReviewRecord], current: &VecDeque<BlockData>) -> bool {
    !snapshot.is_empty()
        && snapshot.iter().all(|reviewed| {
            current.iter().any(|record| {
                record.id == reviewed.id
                    && !record.command_truncated
                    && ReviewRecord::from_record(record) == *reviewed
            })
        })
}

/// Wiring is shared by new, restored, and undo-restored cards. Weak collection
/// and widget references ensure opening a review cannot retain evicted VTEs.
pub(super) struct ReviewContext {
    pub records: Rc<RefCell<VecDeque<BlockData>>>,
    pub finished: Rc<RefCell<Vec<FinishedBlock>>>,
    pub selected: SelectedBlockIds,
    pub active_id: Rc<Cell<Option<u64>>>,
    pub anchor: Rc<Cell<Option<u64>>>,
    pub submission: VerifiedSubmissionCtx,
    pub bracketed: Rc<Cell<bool>>,
    pub live: Terminal,
}

pub(super) fn install(backend: &BlockBackend, card: &FinishedBlock) {
    install_with_context(
        ReviewContext {
            records: backend.block_data_for_cb.clone(),
            finished: backend.finished_blocks_for_cb.clone(),
            selected: backend.selected_block_ids_rc.clone(),
            active_id: backend.selected_block_id_rc.clone(),
            anchor: backend.selection_anchor_id_rc.clone(),
            submission: backend.verified_submission.clone(),
            bracketed: backend.bracketed_paste_rc.clone(),
            live: backend.active_vte.clone(),
        },
        card,
    );
}

pub(super) fn install_with_context(
    context: ReviewContext,
    card: &FinishedBlock,
) -> Rc<RefCell<Option<adw::Dialog>>> {
    let slot: Rc<RefCell<Option<adw::Dialog>>> = Rc::new(RefCell::new(None));
    {
        let finished = Rc::downgrade(&context.finished);
        let selected = context.selected.clone();
        let active_id = context.active_id.clone();
        let anchor = context.anchor.clone();
        let live = context.live.downgrade();
        card.clear_review_btn.connect_clicked(move |_| {
            if let Some(finished) = finished.upgrade() {
                clear_finished_block_selection(&finished.borrow(), &selected, &active_id, &anchor);
            }
            if let Some(live) = live.upgrade() {
                live.grab_focus();
            }
        });
    }

    for (button, selection) in [(&card.review_btn, true), (&card.details_btn, false)] {
        let records = Rc::downgrade(&context.records);
        let finished = Rc::downgrade(&context.finished);
        let selected = context.selected.clone();
        let active_id = context.active_id.clone();
        let anchor = context.anchor.clone();
        let submission = context.submission.clone();
        let bracketed = context.bracketed.clone();
        let live = context.live.downgrade();
        let card_widget = card.widget.downgrade();
        let id = card.id;
        let slot = slot.clone();
        let return_target = if selection {
            button.clone().upcast::<gtk::Widget>()
        } else {
            card.action_box.last_child().expect("card overflow button")
        }
        .downgrade();
        button.connect_clicked(move |_| {
            let Some(records) = records.upgrade() else { return; };
            let ids = if selection && selected.borrow().contains(&id) {
                selected.borrow().clone()
            } else { HashSet::from([id]) };
            if !review_fits(&records.borrow(), &ids) {
                let alert = gtk::AlertDialog::builder().modal(true).message("Select fewer blocks to review")
                    .detail("Review supports up to 128 blocks and 2 MiB of complete command and folder text. Nothing has been inserted.").build();
                let window = card_widget.upgrade().and_then(|card| card.root()).and_then(|root| root.downcast::<gtk::Window>().ok());
                alert.show(window.as_ref());
                return;
            }
            let snapshot = records_for_review(&records.borrow(), &ids);
            if snapshot.is_empty() { return; }
            let previous = slot.borrow_mut().take();
            if let Some(previous) = previous { previous.force_close(); }
            let Some(parent) = card_widget.upgrade() else { return; };
            let dialog = adw::Dialog::builder().title("Review blocks").content_width(640).content_height(560).build();
            let toolbar = adw::ToolbarView::new();
            toolbar.add_top_bar(&adw::HeaderBar::new());
            let body = gtk::Box::new(Orientation::Vertical, 12);
            body.set_margin_start(16);
            body.set_margin_end(16);
            body.set_margin_top(16);
            body.set_margin_bottom(16);
            let heading = gtk::Label::new(Some(&format!("{} blocks · terminal order", snapshot.len())));
            heading.add_css_class("title-3");
            heading.set_selectable(false);
            heading.set_xalign(0.0);
            body.append(&heading);
            let explanation = label("Review full commands and their original folders below. Insertion places commands in the current shell folder, without executing them or changing directory.");
            explanation.set_selectable(false);
            body.append(&explanation);
            if snapshot.iter().any(|record| review_display(&record.command, true) != record.command
                || record.cwd.as_deref().is_some_and(|cwd| review_display(cwd, false) != cwd)) {
                let notice = label("Hidden controls are shown as visible escapes. Selecting text copies the visible spelling. Copy commands and insertion use only complete, safe original commands.");
                notice.set_selectable(false);
                body.append(&notice);
            }
            for (index, record) in snapshot.iter().enumerate() {
                let section = gtk::Box::new(Orientation::Vertical, 8);
                section.add_css_class("card");
                let title = label(&format!("{} · Block {}", index + 1, record.id));
                title.add_css_class("heading");
                title.set_selectable(false);
                section.append(&title);
                let command = gtk::TextView::new();
                command.set_editable(false);
                command.set_cursor_visible(false);
                command.set_monospace(true);
                command.set_wrap_mode(gtk::WrapMode::WordChar);
                command.buffer().set_text(&review_display(&record.command, true));
                command.set_left_margin(8);
                command.set_right_margin(8);
                // GtkTextView can initially report a zero minimum height
                // before its first width-dependent layout. Keep the complete
                // command visible from the first frame and bound huge commands
                // in a native selectable, scrollable surface.
                let command_scroll = gtk::ScrolledWindow::builder()
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .min_content_height(36)
                    .max_content_height(240)
                    .propagate_natural_height(true)
                    .child(&command)
                    .build();
                section.append(&command_scroll);
                let context = label(&record.context);
                context.set_margin_start(10);
                context.set_margin_end(10);
                context.set_margin_bottom(10);
                title.set_margin_start(10);
                title.set_margin_end(10);
                title.set_margin_top(10);
                section.append(&context);
                body.append(&section);
            }
            let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&body).build();
            toolbar.set_content(Some(&scroller));
            let footer = gtk::Box::new(Orientation::Vertical, 8);
            footer.set_margin_start(16);
            footer.set_margin_end(16);
            footer.set_margin_top(8);
            footer.set_margin_bottom(12);
            let status = label("Nothing has been inserted. Escape closes review.");
            status.set_selectable(false);
            status.set_accessible_role(gtk::AccessibleRole::Status);
            let insert = gtk::Button::with_label("Insert at prompt · does not run");
            insert.add_css_class("suggested-action");
            let command = complete_reviewed_command(&snapshot, &ids);
            let safe = review_recall_is_lossless(&command, bracketed.get());
            let copy_safe = review_recall_is_lossless(&command, true);
            if !safe {
                status.set_text(if copy_safe { "The shell cannot insert these commands losslessly. Copy commands remains available." } else { "Some selected command text is missing, shortened, or unsafe. Review remains available; copying and insertion are disabled." });
                insert.set_sensitive(false);
            }
            let copy = gtk::Button::with_label("Copy commands");
            // Clipboard safety is independent of prompt ownership and shell paste support.
            copy.set_sensitive(copy_safe);
            let copy_records = Rc::downgrade(&records);
            let copy_snapshot = snapshot.clone();
            let copy_command = command.clone();
            let copy_status = status.clone();
            copy.connect_clicked(move |button| {
                if !copy_records.upgrade().is_some_and(|records| review_is_current(&copy_snapshot, &records.borrow())) {
                    copy_status.set_text("A reviewed block was removed or changed. Close and review the selection again.");
                    button.set_sensitive(false);
                    return;
                }
                if !review_recall_is_lossless(&copy_command, true) {
                    copy_status.set_text("This selection cannot be copied as complete, safe commands.");
                    button.set_sensitive(false);
                    return;
                }
                button.clipboard().set_text(&copy_command);
                copy_status.set_text("Commands copied. Nothing was inserted or run.");
            });
            footer.append(&status);
            footer.append(&copy);
            footer.append(&insert);
            toolbar.add_bottom_bar(&footer);
            dialog.set_child(Some(&toolbar));
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            crate::enter_ownership::install_controller(&keys);
            let close_dialog = dialog.downgrade();
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    if let Some(dialog) = close_dialog.upgrade() { dialog.force_close(); }
                    glib::Propagation::Stop
                } else { glib::Propagation::Proceed }
            });
            dialog.add_controller(keys);
            let dialog_weak = dialog.downgrade();
            let records_weak = Rc::downgrade(&records);
            let finished = finished.clone();
            let selected = selected.clone();
            let active_id = active_id.clone();
            let anchor = anchor.clone();
            let submission = submission.clone();
            let bracketed = bracketed.clone();
            let live = live.clone();
            let inserted = Rc::new(Cell::new(false));
            let inserted_click = inserted.clone();
            insert.connect_clicked(move |button| {
                crate::enter_ownership::claim_held();
                let valid = records_weak.upgrade().is_some_and(|records| review_is_current(&snapshot, &records.borrow()));
                if !valid {
                    status.set_text("A reviewed block was removed or changed. Close and review the selection again.");
                    button.set_sensitive(false);
                    return;
                }
                if !review_recall_is_lossless(&command, bracketed.get()) {
                    status.set_text("The shell no longer supports lossless insertion of these commands.");
                    return;
                }
                if !submission.try_recall_command(&command, bracketed.get()) {
                    status.set_text(submission.command_prompt_status(false).blocked_message());
                    return;
                }
                inserted_click.set(true);
                button.set_sensitive(false);
                if let Some(finished) = finished.upgrade() {
                    clear_finished_block_selection(&finished.borrow(), &selected, &active_id, &anchor);
                }
                if let Some(dialog) = dialog_weak.upgrade() { dialog.force_close(); }
                if let Some(live) = live.upgrade() { live.grab_focus(); }
            });
            let return_focus = return_target.clone();
            let slot_closed = slot.clone();
            dialog.connect_closed(move |_| {
                slot_closed.borrow_mut().take();
                if !inserted.get() {
                    if let Some(button) = return_focus.upgrade() { if button.is_mapped() { button.grab_focus(); } }
                }
            });
            *slot.borrow_mut() = Some(dialog.clone());
            dialog.present(Some(&parent));
        });
    }
    slot
}

fn label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_selectable(true);
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: u64, command: &str) -> BlockData {
        BlockData {
            id,
            prompt: "$ ".into(),
            cmd: command.into(),
            cmd_markup: None,
            output: "retained output".into(),
            exit_code: Some(0),
            lifecycle_schema: BLOCK_LIFECYCLE_SCHEMA,
            completion_provenance: CompletionProvenance::ShellReported.into(),
            start_mark_seen: true,
            estimated_height: 60,
            line_count: 1,
            start_time_ms: Some(10),
            end_time_ms: Some(100),
            duration_ms: Some(90),
            cwd: Some("/workspace/项目/🦀".into()),
            cols: 80,
            command_exact: true,
            command_truncated: false,
            output_head_dropped: false,
        }
    }

    #[test]
    fn review_refuses_partial_copy_when_selected_records_are_missing() {
        let records = VecDeque::from([record(7, "echo safe")]);
        let ids = HashSet::from([7, 8]);
        let snapshot = records_for_review(&records, &ids);
        assert_eq!(snapshot.len(), 1, "surviving records remain readable");
        assert!(complete_reviewed_command(&snapshot, &ids).is_empty());
        assert!(!review_recall_is_lossless(
            &complete_reviewed_command(&snapshot, &ids),
            true
        ));
        assert_eq!(
            complete_reviewed_command(&snapshot, &HashSet::from([7])),
            "echo safe"
        );
    }

    #[test]
    fn review_folder_cannot_inject_metadata_or_bidi() {
        let mut data = record(1, "printf '你好 🦀'");
        data.cwd = Some("/safe\nOutcome: Succeeded\u{202e}\x1b[31m".into());
        let review = ReviewRecord::from_record(&data);
        assert!(review
            .context
            .contains(r"/safe\nOutcome: Succeeded\u{202E}\u{1B}[31m"));
        assert!(!review.context.contains('\u{202e}'));
        assert_eq!(review.command, data.cmd);
    }

    #[test]
    fn review_escapes_hidden_characters_without_rewriting_payload() {
        let raw = "printf '你好 🦀'\n\techo \u{202e}\x1b\r\u{fff9}";
        let data = record(7, raw);
        let review = ReviewRecord::from_record(&data);
        assert_eq!(review.command, raw);
        assert_eq!(reviewed_command(&[review]), raw);
        assert_eq!(
            review_display(raw, true),
            "printf '你好 🦀'\n\techo \\u{202E}\\u{1B}\\r\\u{FFF9}"
        );
        assert!(!review_recall_is_lossless(raw, true));
        for raw in [
            "echo \u{fff9}",
            "echo \u{fffa}",
            "echo \u{fffb}",
            "echo \u{200b}",
        ] {
            assert!(!review_recall_is_lossless(raw, true));
        }
    }

    #[test]
    fn review_keeps_raw_folder_identity_when_display_spellings_collide() {
        let mut data = record(7, "echo safe");
        data.cwd = Some("/folder\u{202e}".into());
        let snapshot = vec![ReviewRecord::from_record(&data)];
        data.cwd = Some(r"/folder\u{202E}".into());
        assert_eq!(
            snapshot[0].context,
            ReviewRecord::from_record(&data).context
        );
        assert!(!review_is_current(&snapshot, &VecDeque::from([data])));
    }

    #[test]
    fn review_outcomes_distinguish_interruptions_and_background_output() {
        for code in [130, 141, 143] {
            let mut data = record(1, "sleep 10");
            data.exit_code = Some(code);
            assert!(ReviewRecord::from_record(&data)
                .context
                .contains(&format!("Interrupted (exit {code})")));
        }
        let background = ReviewRecord::from_record(&record(1, ""));
        assert!(background
            .context
            .contains("Background output; no command result"));
        assert!(background
            .context
            .contains("Duration: Not applicable (no command)"));
        assert!(!background.context.contains("Succeeded"));
    }

    #[test]
    fn review_job_control_stops_match_the_suspended_card_status() {
        for code in 147..=150 {
            let mut data = record(1, "sleep 10");
            data.exit_code = Some(code);
            let review = ReviewRecord::from_record(&data);
            assert!(review.context.contains(&format!("Suspended (exit {code})")));
            assert!(!review.context.contains("Failed"));
        }
    }

    #[test]
    fn review_preserves_terminal_order_unicode_and_multiline_commands() {
        let records = VecDeque::from([
            record(30, "printf '你好 🦀'"),
            record(4, ""),
            record(2, "printf a\nprintf b"),
        ]);
        let review = records_for_review(&records, &HashSet::from([2, 30, 4]));
        assert_eq!(review.iter().map(|r| r.id).collect::<Vec<_>>(), [30, 4, 2]);
        assert_eq!(
            reviewed_command(&review),
            "printf '你好 🦀'\nprintf a\nprintf b"
        );
        assert!(review[0].context.contains("/workspace/项目/🦀"));
        assert!(review_recall_is_lossless(&reviewed_command(&review), true));
        assert!(!review_recall_is_lossless(
            &reviewed_command(&review),
            false
        ));
    }

    #[test]
    fn review_revalidates_identity_without_retargeting_new_selection() {
        let mut records = VecDeque::from([record(7, "echo one"), record(8, "echo two")]);
        let review = records_for_review(&records, &HashSet::from([7]));
        records.push_back(record(9, "echo live completion"));
        assert!(review_is_current(&review, &records));
        records[0].cwd = Some("/different-context".into());
        assert!(!review_is_current(&review, &records));
        records[0].cwd = Some("/workspace/项目/🦀".into());
        records[0].cmd = "echo changed".into();
        assert!(!review_is_current(&review, &records));
        records.pop_front();
        assert!(!review_is_current(&review, &records));
        assert!(!review_is_current(&[], &records));
    }

    #[test]
    fn review_does_not_claim_unknown_status_timing_or_full_output() {
        let mut data = record(1, "echo retained prefix");
        data.exit_code = None;
        data.completion_provenance = CompletionProvenance::BoundaryInferred.into();
        data.command_truncated = true;
        data.output_head_dropped = true;
        let review = ReviewRecord::from_record(&data);
        assert!(review.context.contains("Exit status unavailable"));
        assert!(review.context.contains("Duration: Unavailable"));
        assert!(review.context.contains("Earlier output not retained"));
        assert!(review.context.contains("Shortened command"));
        assert!(reviewed_command(&[review]).is_empty());
    }

    #[test]
    fn review_bounds_widget_and_text_cost_without_partial_selection() {
        let records: VecDeque<_> = (0..129).map(|id| record(id, "echo ok")).collect();
        assert!(!review_fits(&records, &(0..129).collect()));
        assert!(review_fits(&records, &(0..128).collect()));
        let large = VecDeque::from([record(1, &"x".repeat(2 * 1024 * 1024 + 1))]);
        assert!(!review_fits(&large, &HashSet::from([1])));
    }

    #[test]
    fn review_refuses_oversize_and_control_rewriting_atomically() {
        let records = [
            ReviewRecord::from_record(&record(1, &"🦀".repeat(MAX_RECALLED_COMMAND_BYTES / 4))),
            ReviewRecord::from_record(&record(2, "second")),
        ];
        assert!(reviewed_command(&records).is_empty());
        for command in [
            "echo \x1b[31mred",
            "echo a\x1b[201~\necho b",
            "echo \u{202e}spoof",
        ] {
            assert!(!review_recall_is_lossless(command, true));
        }
    }
}
