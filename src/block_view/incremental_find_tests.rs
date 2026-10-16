//! Native incremental find must reveal its output without taking the find entry's focus.
use super::*;
use std::time::{Duration, Instant};

const CARD_COUNT: u64 = 12;

// A continuously ready GLib source must not hide the caller's deadline.
fn dispatch_until(mut before_deadline: impl FnMut() -> bool, mut dispatch: impl FnMut() -> bool) {
    while before_deadline() && dispatch() {}
}

fn wait_until_with(
    mut ready: impl FnMut() -> bool,
    mut before_deadline: impl FnMut() -> bool,
    mut settle: impl FnMut(),
    message: &str,
) {
    while !ready() {
        assert!(before_deadline(), "{message}");
        settle();
    }
    settle();
}

fn settle() {
    let context = gtk::glib::MainContext::default();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(250) {
        dispatch_until(
            || start.elapsed() < Duration::from_millis(250),
            || context.iteration(false),
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_until(ready: impl FnMut() -> bool, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    wait_until_with(ready, || Instant::now() < deadline, settle, message);
}

#[test]
fn always_ready_dispatch_returns_at_deadline_without_claiming_quiescence() {
    let tick = std::cell::Cell::new(0);
    dispatch_until(
        || tick.get() < 250,
        || {
            tick.set(tick.get() + 1);
            true
        },
    );
    assert_eq!(tick.get(), 250);
    // The source remains ready; a subsequent bounded drain still does work.
    dispatch_until(
        || tick.get() < 500,
        || {
            tick.set(tick.get() + 1);
            true
        },
    );
    assert_eq!(tick.get(), 500);
}

#[test]
#[should_panic(expected = "readiness deadline remains enforced")]
fn always_ready_dispatch_does_not_hide_readiness_timeout() {
    let tick = std::cell::Cell::new(0);
    wait_until_with(
        || false,
        || tick.get() < 3000,
        || {
            let end = tick.get() + 250;
            dispatch_until(
                || tick.get() < end,
                || {
                    tick.set(tick.get() + 1);
                    true
                },
            );
        },
        "readiness deadline remains enforced",
    );
}

#[test]
fn bounded_dispatch_stops_when_idle_and_readiness_still_settles() {
    let calls = std::cell::Cell::new(0);
    dispatch_until(
        || true,
        || {
            calls.set(calls.get() + 1);
            false
        },
    );
    assert_eq!(calls.get(), 1);
    let settles = std::cell::Cell::new(0);
    wait_until_with(
        || settles.get() == 1,
        || true,
        || settles.set(settles.get() + 1),
        "ready",
    );
    assert_eq!(
        settles.get(),
        2,
        "successful readiness keeps the final settle"
    );
}

fn find_search_entry(widget: &gtk::Widget) -> Option<gtk::SearchEntry> {
    if let Ok(entry) = widget.clone().downcast::<gtk::SearchEntry>() {
        return Some(entry);
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(entry) = find_search_entry(&current) {
            return Some(entry);
        }
        child = current.next_sibling();
    }
    None
}

fn find_toggle_button(widget: &gtk::Widget, label: &str) -> Option<gtk::ToggleButton> {
    if let Ok(button) = widget.clone().downcast::<gtk::ToggleButton>() {
        if button.label().as_deref() == Some(label) {
            return Some(button);
        }
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(button) = find_toggle_button(&current, label) {
            return Some(button);
        }
        child = current.next_sibling();
    }
    None
}

struct Fixture {
    view: TermView,
    window: gtk::Window,
    query: gtk::SearchEntry,
}

impl Fixture {
    fn new(mode: crate::config::TerminalMode) -> Self {
        Self::with_second_output(mode, "keep needle-B\r\nkeep needle-C\r\ndrop needle-D\r\n")
    }

    fn with_second_output(mode: crate::config::TerminalMode, second_output: &str) -> Self {
        super::alt_screen::set_find_render_trace(true);
        // A single native test can create several independent windows. Capture
        // before GTK once per process, never recapture toolkit-mutated state.
        static CHILD_ENV: std::sync::Once = std::sync::Once::new();
        CHILD_ENV.call_once(|| {
            crate::child_env::capture_inherited_environment().expect("test child environment");
        });
        gtk::init().expect("GTK display");
        let mut config = Config::safe_defaults();
        config.terminal_mode = mode;
        let view = TermView::new_with_spawner(
            &config,
            &mode,
            &["sh".into()],
            None,
            false,
            None,
            "incremental-find-test",
            &[],
            &[],
            |_, _, _, _| crate::pty::OwnedPty::from_openpty(Some(true)),
        )
        .expect("nonexecuting find PTY");
        for id in 1..=CARD_COUNT {
            let (command, output) = match id {
                1 => ("printf needle-A", "unrelated command output\r\n"),
                2 => ("cat mixed.log", second_output),
                3 => ("cat last.log", "last needle-E\r\n"),
                _ => ("printf filler", "unrelated retained output\r\n"),
            };
            let card = FinishedBlock::new(
                id,
                "$ ",
                command,
                None,
                output,
                Some(0),
                &config,
                None,
                None,
                None,
                80,
            );
            let height = card.set_virtualized(true);
            card.widget()
                .insert_before(&view.block_list, Some(view.active.borrow().widget()));
            view.block_data.borrow_mut().push_back(BlockData {
                id,
                prompt: "$ ".into(),
                cmd: command.into(),
                cmd_markup: None,
                output: output.into(),
                exit_code: Some(0),
                lifecycle_schema: blocks::BLOCK_LIFECYCLE_SCHEMA,
                completion_provenance: CompletionProvenance::ShellReported.into(),
                start_mark_seen: true,
                estimated_height: height,
                line_count: output.lines().count(),
                start_time_ms: None,
                end_time_ms: None,
                duration_ms: None,
                cwd: None,
                cols: 80,
                command_exact: true,
                command_truncated: false,
                output_head_dropped: false,
            });
            view.finished_blocks.borrow_mut().push(card);
        }

        // The real native search entry is outside the terminal's widget subtree,
        // exactly where the app's find bar owns keyboard input during navigation.
        let query = gtk::SearchEntry::new();
        let search_bar = gtk::SearchBar::new();
        search_bar.set_child(Some(&query));
        search_bar.connect_entry(&query);
        search_bar.set_search_mode(true);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&search_bar);
        root.append(&view.root);
        let window = gtk::Window::builder()
            .default_width(760)
            .default_height(480)
            .child(&root)
            .build();
        window.present();

        // Chat hides history until the real prompt lifecycle grants it a dock.
        view.pty.set_test_foreground_group(Some(101));
        view.pty.emit_test_output(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        wait_until(
            || view.bstate.get() == BlockState::AwaitingCommand && view.block_scroll.is_mapped(),
            "the OSC 133 prompt must expose the history transcript",
        );
        assert_eq!(view.finished_blocks.borrow().len(), CARD_COUNT as usize);
        view.scroll_debouncer.user_scrolled_up.set(true);
        view.block_scroll.vadjustment().set_value(0.0);
        view.update_viewport();
        view.update_block_visibility();
        settle();

        // Keep a genuine multi-card selection while find targets other cards.
        view.selected_block_ids.borrow_mut().extend([1, 4]);
        view.selected_block_id.set(Some(4));
        view.selection_anchor_id.set(Some(1));
        sync_finished_block_selection(
            &view.finished_blocks.borrow(),
            &view.selected_block_ids,
            &view.selected_block_id,
        );
        let fixture = Self {
            view,
            window,
            query,
        };
        fixture.collapse_all();
        fixture.focus_query();
        fixture
    }

    fn focus_query(&self) {
        assert!(self.query.grab_focus(), "the find entry must accept focus");
        settle();
        self.assert_ownership();
    }

    fn assert_ownership(&self) {
        let focused = gtk::prelude::RootExt::focus(&self.window).expect("native focus owner");
        assert!(
            focused == self.query.clone().upcast::<gtk::Widget>()
                || focused.is_ancestor(&self.query),
            "incremental find must leave keyboard focus in its search entry; got {focused:?}"
        );
        assert_eq!(
            *self.view.selected_block_ids.borrow(),
            HashSet::from([1, 4])
        );
        assert_eq!(self.view.selected_block_id.get(), Some(4));
        assert_eq!(self.view.selection_anchor_id.get(), Some(1));
        assert!(
            self.view.scroll_debouncer.user_scrolled_up.get(),
            "find must preserve the user's follow-bottom lock"
        );
    }

    fn collapse_all(&self) {
        self.view.set_all_blocks_collapsed(true);
        self.view.block_scroll.vadjustment().set_value(0.0);
        self.view.update_viewport();
        self.view.update_block_visibility();
        settle();
        self.assert_open_outputs(&[]);
        for card in &self.view.finished_blocks.borrow()[..3] {
            assert!(
                card.widget().is_mapped(),
                "target cards must already be mounted"
            );
            assert_eq!(
                card.widget().height_request(),
                -1,
                "not a virtual placeholder"
            );
            assert!(
                !card.output_vte.is_mapped(),
                "collapsed output must really be hidden"
            );
        }
    }

    fn assert_open_outputs(&self, ids: &[u64]) {
        for card in self.view.finished_blocks.borrow().iter() {
            assert_eq!(
                !card.is_output_collapsed(),
                ids.contains(&card.id),
                "find changed the wrong fold for record {}",
                card.id,
            );
        }
    }

    fn search(&self, query: &str, regex: bool) -> find::FindSearchResult {
        self.query.set_text(&if regex {
            format!("/{query}/")
        } else {
            query.to_string()
        });
        self.focus_query();
        let result = self.view.find_in_blocks(query, regex, false);
        eprintln!("FIND_FIXTURE search query={query:?} regex={regex} result={result:?}");
        for (id, output) in [(1, false), (2, true), (3, true)] {
            self.trace_native_selection("search immediate", id, output);
        }
        self.assert_ownership();
        result
    }

    /// Deliberately do not iterate the main loop: the caller is exercising the
    /// interval between a reveal's queued VTE feed and its completion signal.
    fn search_now(&self, query: &str, regex: bool) -> find::FindSearchResult {
        self.assert_ownership();
        self.query.set_text(&if regex {
            format!("/{query}/")
        } else {
            query.to_string()
        });
        let result = self.view.find_in_blocks(query, regex, false);
        eprintln!("FIND_FIXTURE search without settle query={query:?} result={result:?}");
        self.trace_native_selection("search without settle", 2, true);
        self.assert_ownership();
        result
    }

    fn step_now(&self, next: bool, current: usize, total: usize) {
        self.assert_ownership();
        let result = if next {
            self.view.find_next()
        } else {
            self.view.find_prev()
        };
        eprintln!("FIND_FIXTURE step without settle next={next} result={result:?}");
        self.trace_native_selection("step without settle", 2, true);
        assert_eq!(
            result,
            find::FindNavigationResult::Progress(progress(current, total)),
        );
        self.assert_ownership();
    }

    fn assert_output_selection_empty(&self, id: u64) {
        self.assert_ownership();
        let cards = self.view.finished_blocks.borrow();
        let card = cards.iter().find(|card| card.id == id).unwrap();
        assert!(card.output_vte.is_mapped());
        assert!(
            card.output_vte
                .text_selected(vte4::Format::Text)
                .as_deref()
                .unwrap_or_default()
                .is_empty(),
            "retired replay must not restore its old output selection",
        );
    }

    fn assert_output_viewport_contains(&self, id: u64, expected: &str) -> gtk::glib::GString {
        self.assert_ownership();
        let cards = self.view.finished_blocks.borrow();
        let card = cards.iter().find(|card| card.id == id).unwrap();
        let terminal = &card.output_vte;
        assert!(terminal.is_mapped());
        assert!(
            (1..=256).contains(&terminal.row_count()),
            "native displayed grid must stay bounded",
        );
        let adjustment = terminal.vadjustment().expect("native output adjustment");
        let lower = adjustment.lower();
        let upper = adjustment.upper();
        let value = adjustment.value();
        let page = adjustment.page_size();
        assert!([lower, upper, value, page]
            .iter()
            .all(|number| number.is_finite()));
        assert!(page > 0.0);
        // Bound the native viewport dimensions before extracting its text.
        // These adjustment rows are relative to the retained ring's start.
        let first_row = value.ceil().max(lower.ceil()) as i64;
        let after_last_row = (value + page).floor().min(upper.floor()) as i64;
        let rows = after_last_row - first_row;
        let columns = terminal.column_count();
        assert!(
            (1..=256).contains(&rows),
            "fixture viewport read must stay bounded"
        );
        assert!(
            (1..=512).contains(&columns),
            "fixture viewport columns must stay bounded"
        );
        // VTE 0.76's widget.cc normalizes GtkAdjustment to the ring's start,
        // but text_range_format uses absolute ring rows. A reset advances that
        // hidden start. text_format reads only VTE's actual displayed rows and
        // avoids mixing those coordinate spaces after a snapshot replay.
        let visible = terminal
            .text_format(vte4::Format::Text)
            .expect("native visible output text");
        eprintln!(
            "FIND_FIXTURE visible id={id} expected={expected:?} adjustment_rows={first_row}..{after_last_row} adjustment(lower,upper,value,page)={:?}",
            (lower, upper, value, page),
        );
        assert!(
            visible.contains(expected),
            "selected hit must remain in the actual native viewport after layout and deferred pins; visible={visible:?}",
        );
        visible
    }

    fn trace_native_selection(&self, stage: &str, id: u64, output: bool) {
        let cards = self.view.finished_blocks.borrow();
        let card = cards.iter().find(|card| card.id == id).unwrap();
        let terminal = if output {
            &card.output_vte
        } else {
            &card.command_vte
        };
        let adjustment = terminal.vadjustment().map(|adjustment| {
            (
                adjustment.lower(),
                adjustment.upper(),
                adjustment.value(),
                adjustment.page_size(),
            )
        });
        eprintln!(
            "FIND_FIXTURE {stage} id={id} output={output} selected={:?} stamp={:?} rows={} cols={} mapped={} allocation={}x{} adjustment(lower,upper,value,page)={adjustment:?}",
            terminal.text_selected(vte4::Format::Text),
            card.render_stamp(),
            terminal.row_count(),
            terminal.column_count(),
            terminal.is_mapped(),
            terminal.width(),
            terminal.height(),
        );
    }

    fn assert_match(&self, id: u64, output: bool, expected: &str) {
        eprintln!("FIND_FIXTURE assert_match id={id} output={output} expected={expected:?}");
        self.trace_native_selection("before settle", id, output);
        settle();
        self.trace_native_selection("after settle", id, output);
        self.assert_ownership();
        let cards = self.view.finished_blocks.borrow();
        let card = cards.iter().find(|card| card.id == id).unwrap();
        let terminal = if output {
            &card.output_vte
        } else {
            &card.command_vte
        };
        assert!(
            terminal.is_mapped(),
            "the selected VTE must be mapped after layout"
        );
        assert!(terminal.width() > 1 && terminal.height() > 1);
        assert_eq!(
            terminal.text_selected(vte4::Format::Text).as_deref(),
            Some(expected),
            "the native selection must name the exact occurrence after layout"
        );
    }

    fn step(&self, next: bool, current: usize, total: usize, id: u64, output: bool, text: &str) {
        self.focus_query();
        let result = if next {
            self.view.find_next()
        } else {
            self.view.find_prev()
        };
        eprintln!("FIND_FIXTURE step next={next} expected={current}/{total} result={result:?}");
        self.trace_native_selection("step immediate", id, output);
        assert_eq!(
            result,
            find::FindNavigationResult::Progress(progress(current, total))
        );
        self.assert_match(id, output, text);
    }

    fn searchable_output(&self, id: u64) -> String {
        let cards = self.view.finished_blocks.borrow();
        cards
            .iter()
            .find(|card| card.id == id)
            .unwrap()
            .with_searchable_output(str::to_string)
    }

    fn assert_scroll_stays_parked(&self) {
        let adjustment = self.view.block_scroll.vadjustment();
        let parked = adjustment.value();
        assert!(
            adjustment.upper() - adjustment.page_size() > parked + 1.0,
            "the fixture must leave room for an erroneous follow-bottom jump"
        );
        self.view
            .scroll_debouncer
            .mark_dirty(&self.view.block_scroll);
        self.view
            .scroll_debouncer
            .pin_to_bottom_deferred(&self.view.block_scroll);
        settle();
        assert!(
            (adjustment.value() - parked).abs() < 1.0,
            "streaming must not move away from the selected search hit"
        );
        self.assert_ownership();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.window.close();
        settle();
    }
}

fn progress(current: usize, total: usize) -> find::FindProgress {
    find::FindProgress {
        current,
        total,
        capped: false,
        scan_limited: false,
    }
}

fn navigation_reveals_exact_occurrence(mode: crate::config::TerminalMode) {
    let fixture = Fixture::new(mode);
    assert_eq!(
        fixture.search("needle-C", false),
        find::FindSearchResult::Matches(progress(1, 1))
    );
    fixture.assert_open_outputs(&[2]);
    fixture.assert_match(2, true, "needle-C");

    fixture.collapse_all();
    assert_eq!(
        fixture.search("needle-A", false),
        find::FindSearchResult::Matches(progress(1, 1))
    );
    fixture.assert_open_outputs(&[]);
    fixture.assert_match(1, false, "needle-A");

    // Different matched strings make an off-by-one native cursor observable;
    // a counter-only assertion could pass while selecting the wrong occurrence.
    assert_eq!(
        fixture.search("needle-[A-E]", true),
        find::FindSearchResult::Matches(progress(1, 5))
    );
    fixture.assert_open_outputs(&[]);
    fixture.assert_match(1, false, "needle-A");
    fixture.step(true, 2, 5, 2, true, "needle-B");
    fixture.assert_open_outputs(&[2]);
    fixture.step(true, 3, 5, 2, true, "needle-C");
    fixture.step(true, 4, 5, 2, true, "needle-D");
    fixture.step(true, 5, 5, 3, true, "needle-E");
    fixture.assert_open_outputs(&[2, 3]);
    fixture.step(true, 1, 5, 1, false, "needle-A");

    // Previous enters a never-selected folded surface at its last occurrence.
    fixture.collapse_all();
    assert_eq!(
        fixture.search("needle-[A-E]", true),
        find::FindSearchResult::Matches(progress(1, 5))
    );
    fixture.step(false, 5, 5, 3, true, "needle-E");
    fixture.assert_open_outputs(&[3]);
    fixture.step(false, 4, 5, 2, true, "needle-D");
    fixture.assert_open_outputs(&[2, 3]);
    fixture.step(false, 3, 5, 2, true, "needle-C");
    fixture.step(false, 2, 5, 2, true, "needle-B");
    fixture.step(false, 1, 5, 1, false, "needle-A");
    // A single-hit no-op must also restore a native selection lost to another
    // selection owner, not trust the logical occurrence cache.
    assert_eq!(
        fixture.search("needle-A", false),
        find::FindSearchResult::Matches(progress(1, 1)),
    );
    let command = fixture.view.finished_blocks.borrow()[0].command_vte.clone();
    for next in [true, false] {
        command.unselect_all();
        assert!(!command.has_selection());
        fixture.step(next, 1, 1, 1, false, "needle-A");
    }
    fixture.assert_scroll_stays_parked();
}

fn filtered_single_hit_reopens(mode: crate::config::TerminalMode) {
    let fixture = Fixture::new(mode);
    let card = fixture.view.finished_blocks.borrow()[1].clone();
    (card.toggle_filter)();
    let filter = find_search_entry(card.widget().upcast_ref()).expect("per-card output filter");
    filter.set_text("keep");
    wait_until(
        || {
            fixture.searchable_output(2).contains("needle-B")
                && !fixture.searchable_output(2).contains("needle-D")
        },
        "the real output filter must finish applying its keep/drop subset",
    );
    let filtered_before = fixture.searchable_output(2);
    fixture.collapse_all();
    assert_eq!(
        fixture.search("needle-D", false),
        find::FindSearchResult::NoMatches
    );
    fixture.assert_open_outputs(&[]);
    assert_eq!(
        fixture.search("needle-[BCD]", true),
        find::FindSearchResult::Matches(progress(1, 2))
    );
    fixture.assert_open_outputs(&[2]);
    fixture.assert_match(2, true, "needle-B");
    fixture.step(true, 2, 2, 2, true, "needle-C");
    fixture.step(false, 1, 2, 2, true, "needle-B");

    assert_eq!(
        fixture.search("needle-C", false),
        find::FindSearchResult::Matches(progress(1, 1))
    );
    fixture.assert_match(2, true, "needle-C");
    // The one-hit cursor does not advance, but it must still reveal the card
    // after an independent header collapse. Neither Next nor Previous may
    // return "already selected" while the selected output is hidden.
    for next in [true, false] {
        assert!(card.set_collapsed(true));
        settle();
        assert!(!card.output_vte.is_mapped());
        fixture.step(next, 1, 1, 2, true, "needle-C");
        fixture.assert_open_outputs(&[2]);
        assert_eq!(filter.text().as_str(), "keep");
        assert_eq!(
            fixture.searchable_output(2),
            filtered_before,
            "revealing a hit must not clear or broaden the active output filter"
        );
    }
    fixture.assert_scroll_stays_parked();
}

fn resized_reveal_preserves_last_filtered_occurrence(mode: crate::config::TerminalMode) {
    // Enough retained rows to make a real height change alter the output's
    // viewport stamp. Keep both matches near the top so this fixture tests
    // reveal/replay, rather than depending on offscreen-search policy.
    let output = format!(
        "keep needle-B\r\nkeep needle-C\r\n{}drop needle-D\r\n",
        "keep retained filler\r\n".repeat(60),
    );
    let fixture = Fixture::with_second_output(mode, &output);
    let card = fixture.view.finished_blocks.borrow()[1].clone();
    card.set_collapsed(false);
    (card.toggle_filter)();
    let filter = find_search_entry(card.widget().upcast_ref()).expect("per-card output filter");
    filter.set_text("keep");
    wait_until(
        || {
            card.output_vte.is_mapped()
                && fixture.searchable_output(2).contains("needle-C")
                && !fixture.searchable_output(2).contains("needle-D")
        },
        "the long filtered output must render at the original window height",
    );
    let filtered_before = fixture.searchable_output(2);
    fixture.collapse_all();
    let before_resize = card.render_stamp();
    assert!(before_resize.0 > 0 && before_resize.1 > 0);
    assert!(
        before_resize.1 < 60,
        "the original output must be viewport-capped"
    );
    let old_height = fixture.view.block_scroll.height();

    // Hidden output deliberately skips global refits. Its next reveal must
    // adopt the new ancestor viewport and invalidate its old native cursor.
    // Change height only: an unmapped VTE may retain its old width allocation.
    fixture.window.set_default_size(760, 720);
    fixture.window.queue_resize();
    wait_until(
        || {
            assert!(
                !card.output_vte.is_mapped(),
                "resize must leave output folded"
            );
            fixture.view.block_scroll.height() > old_height + 100
        },
        "the actual transcript viewport must grow while output stays unmapped",
    );
    assert_eq!(
        card.render_stamp(),
        before_resize,
        "hidden output must retain its old stamp"
    );
    assert!(card.widget().is_mapped());
    assert_eq!(card.widget().height_request(), -1);
    fixture.assert_open_outputs(&[]);

    // First select the command, recording the folded output's old stamp.
    // Previous wraps directly to its SECOND (last) filtered occurrence, so
    // restarting the query at occurrence zero cannot accidentally pass.
    assert_eq!(
        fixture.search("needle-[A-D]", true),
        find::FindSearchResult::Matches(progress(1, 3)),
    );
    fixture.assert_match(1, false, "needle-A");
    assert_eq!(card.render_stamp(), before_resize);
    assert!(!card.output_vte.is_mapped());
    fixture.focus_query();
    let result = fixture.view.find_prev();
    let during_reveal = card.render_stamp();
    assert_ne!(
        during_reveal, before_resize,
        "find's own reveal must change the stamp before returning"
    );
    assert!(
        during_reveal.1 > before_resize.1,
        "the grown viewport must re-window more output rows"
    );
    assert_eq!(
        during_reveal.3, before_resize.3,
        "geometry must not change the filtered-text generation"
    );
    assert_eq!(result, find::FindNavigationResult::Progress(progress(3, 3)));
    fixture.assert_open_outputs(&[2]);
    fixture.assert_match(2, true, "needle-C");
    assert_eq!(filter.text().as_str(), "keep");
    assert_eq!(fixture.searchable_output(2), filtered_before);
    fixture.step(false, 2, 3, 2, true, "needle-B");
    fixture.step(true, 3, 3, 2, true, "needle-C");
    fixture.assert_scroll_stays_parked();
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn block_resized_reveal_restores_last_filtered_occurrence() {
    resized_reveal_preserves_last_filtered_occurrence(crate::config::TerminalMode::Block);
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn chat_resized_reveal_restores_last_filtered_occurrence() {
    resized_reveal_preserves_last_filtered_occurrence(crate::config::TerminalMode::Chat);
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn block_incremental_find_reveals_exact_occurrence() {
    navigation_reveals_exact_occurrence(crate::config::TerminalMode::Block);
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn chat_incremental_find_reveals_exact_occurrence() {
    navigation_reveals_exact_occurrence(crate::config::TerminalMode::Chat);
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn block_filtered_single_hit_reopens_without_taking_focus() {
    filtered_single_hit_reopens(crate::config::TerminalMode::Block);
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn chat_filtered_single_hit_reopens_without_taking_focus() {
    filtered_single_hit_reopens(crate::config::TerminalMode::Chat);
}

fn pending_replay_tracks_latest_navigation(mode: crate::config::TerminalMode) {
    {
        let fixture = Fixture::new(mode);
        assert_eq!(
            fixture.search_now("needle-[BCD]", true),
            find::FindSearchResult::Matches(progress(1, 3)),
        );
        // Every step occurs before a main-loop iteration. The repair must use the
        // latest cursor (the second output hit), not the original reveal's first
        // hit or the third hit selected by an earlier queued Next.
        fixture.step_now(true, 2, 3);
        fixture.step_now(true, 3, 3);
        fixture.step_now(false, 2, 3);
        fixture.assert_open_outputs(&[2]);
        fixture.assert_match(2, true, "needle-C");
        fixture.step(false, 1, 3, 2, true, "needle-B");
        fixture.step(true, 2, 3, 2, true, "needle-C");
        fixture.assert_scroll_stays_parked();
    }

    // Keep the later output already open before this query, so entering it
    // cannot reveal/rebuild and accidentally discard the earlier surface's
    // cached cursor. That earlier output must finish parsing while a DIFFERENT
    // surface is current, then be selected again at its old last occurrence.
    let fixture = Fixture::new(mode);
    let later = fixture.view.finished_blocks.borrow()[2].clone();
    assert!(later.set_collapsed(false));
    fixture.focus_query();
    assert!(later.output_vte.is_mapped());
    let later_stamp = later.render_stamp();
    fixture.assert_open_outputs(&[3]);
    assert_eq!(
        fixture.search_now("needle-[A-E]", true),
        find::FindSearchResult::Matches(progress(1, 5)),
    );
    fixture.step_now(true, 2, 5);
    fixture.step_now(true, 3, 5);
    fixture.step_now(true, 4, 5);
    fixture.step_now(true, 5, 5);
    fixture.assert_open_outputs(&[2, 3]);
    fixture.assert_match(3, true, "needle-E");
    assert_eq!(later.render_stamp(), later_stamp);
    // The earlier output's native selection was reset while needle-E owned
    // the logical cursor. Its old vte_cursor must not yield AlreadySelected.
    fixture.step(false, 4, 5, 2, true, "needle-D");
    let output = fixture.view.finished_blocks.borrow()[1].output_vte.clone();
    output.unselect_all();
    assert!(!output.has_selection());
    // Restarting a lost native cursor must reach the requested second hit,
    // not merely find any matching string or reset the counter to the first.
    fixture.step(false, 3, 5, 2, true, "needle-C");
    fixture.step(true, 4, 5, 2, true, "needle-D");
    fixture.assert_scroll_stays_parked();
}

#[derive(Clone, Copy, Debug)]
enum ReplayInterruption {
    ReplaceQuery,
    ReplaceQueryOnSameOutput,
    ClearFind,
    EditBeforeDebounce,
    FocusLeaveAndReturn,
    OutputRevision,
}

fn pending_replay_retires_after_interruption(mode: crate::config::TerminalMode) {
    for interruption in [
        ReplayInterruption::ReplaceQuery,
        ReplayInterruption::ReplaceQueryOnSameOutput,
        ReplayInterruption::ClearFind,
        ReplayInterruption::EditBeforeDebounce,
        ReplayInterruption::FocusLeaveAndReturn,
        ReplayInterruption::OutputRevision,
    ] {
        let revision_output = format!(
            "keep needle-B\r\nkeep needle-C\r\ndrop needle-D\r\n{}",
            "keep revision filler\r\n".repeat(60),
        );
        let fixture = if matches!(interruption, ReplayInterruption::OutputRevision) {
            Fixture::with_second_output(mode, &revision_output)
        } else {
            Fixture::new(mode)
        };
        // Put the competing focus owner in the real window before the reveal,
        // so adding it cannot introduce a geometry change during the race.
        let other_entry = gtk::Entry::new();
        other_entry.set_text("independent focus owner");
        fixture
            .window
            .child()
            .unwrap()
            .downcast::<gtk::Box>()
            .unwrap()
            .append(&other_entry);
        let revision_controls = if matches!(interruption, ReplayInterruption::OutputRevision) {
            let card = fixture.view.finished_blocks.borrow()[1].clone();
            card.set_collapsed(false);
            (card.toggle_filter)();
            let filter = find_search_entry(card.widget().upcast_ref())
                .expect("prebuilt per-card output filter");
            let case = find_toggle_button(card.widget().upcast_ref(), "Aa")
                .expect("filter case-sensitive control");
            wait_until(
                || card.output_vte.is_mapped(),
                "prepare the original full output before revision-race setup",
            );
            fixture.collapse_all();
            let old_height = fixture.view.block_scroll.height();
            let old_stamp = card.render_stamp();
            fixture.window.set_default_size(760, 720);
            fixture.window.queue_resize();
            wait_until(
                || {
                    assert!(!card.output_vte.is_mapped());
                    fixture.view.block_scroll.height() > old_height + 100
                },
                "force a real own-reveal replay before the independent filter revision",
            );
            assert_eq!(card.render_stamp(), old_stamp);
            Some((filter, case, old_stamp))
        } else {
            None
        };
        fixture.focus_query();
        assert_eq!(
            fixture.search_now("needle-[BCD]", true),
            find::FindSearchResult::Matches(progress(1, 3)),
        );
        if let Some((_, _, old_stamp)) = revision_controls.as_ref() {
            assert_ne!(
                fixture.view.finished_blocks.borrow()[1].render_stamp(),
                *old_stamp,
                "revision interruption must begin with a genuine own-reveal render",
            );
        }
        fixture.step_now(true, 2, 3);
        let output = fixture.view.finished_blocks.borrow()[1].output_vte.clone();
        eprintln!("FIND_FIXTURE interrupt pending replay: {interruption:?}");
        match interruption {
            ReplayInterruption::ReplaceQuery => {
                // Applying another query retires the old callback even when
                // its output still has a contents-changed emission queued.
                assert_eq!(
                    fixture.search_now("needle-A", false),
                    find::FindSearchResult::Matches(progress(1, 1)),
                );
                fixture.assert_match(1, false, "needle-A");
            }
            ReplayInterruption::ReplaceQueryOnSameOutput => {
                // The new query owns the same still-parsing terminal. Retiring
                // the old repair must not also discard the replacement's hit.
                assert_eq!(
                    fixture.search_now("needle-D", false),
                    find::FindSearchResult::Matches(progress(1, 1)),
                );
            }
            ReplayInterruption::ClearFind => {
                fixture.query.set_text("");
                fixture.view.clear_find();
                assert_eq!(
                    fixture.view.find_next(),
                    find::FindNavigationResult::Inactive
                );
                settle();
                assert_eq!(
                    fixture.view.find_next(),
                    find::FindNavigationResult::Inactive
                );
            }
            ReplayInterruption::EditBeforeDebounce => {
                // The native entry changes before the debounced new query has
                // run. Explicitly clear the old native selection to make its
                // resurrection observable independently of VTE parser timing.
                fixture.query.set_text("replacement awaiting debounce");
                output.unselect_all();
                settle();
                assert_eq!(
                    fixture.query.text().as_str(),
                    "replacement awaiting debounce"
                );
            }
            ReplayInterruption::OutputRevision => {
                let card = fixture.view.finished_blocks.borrow()[1].clone();
                let before = card.render_stamp();
                let (filter, case, _) = revision_controls.as_ref().unwrap();
                filter.set_text("keep");
                // Option changes apply immediately, unlike search-changed's
                // debounce. Neither operation moves native focus away from
                // the find entry, so focus retirement cannot mask this case.
                case.set_active(true);
                fixture.assert_ownership();
                assert_ne!(card.render_stamp().3, before.3);
                let filtered = fixture.searchable_output(2);
                assert!(filtered.contains("needle-B") && filtered.contains("needle-C"));
                assert!(!filtered.contains("needle-D"));
                // The new revision still has the old second hit. A callback
                // which wrongly survives could reselect it successfully; an
                // assertion only about differing stamps would miss that bug.
                output.unselect_all();
                settle();
                assert_eq!(filter.text().as_str(), "keep");
                assert_eq!(fixture.query.text().as_str(), "/needle-[BCD]/");
                assert_eq!(fixture.searchable_output(2), filtered);
            }
            ReplayInterruption::FocusLeaveAndReturn => {
                assert!(other_entry.grab_focus());
                let focused = gtk::prelude::RootExt::focus(&fixture.window)
                    .expect("the competing entry must own native focus");
                assert!(
                    focused == other_entry.clone().upcast::<gtk::Widget>()
                        || focused.is_ancestor(&other_entry),
                );
                // A later owner's cleared selection must win even if the
                // original search entry has focus again when parsing ends.
                output.unselect_all();
                assert!(fixture.query.grab_focus());
                fixture.assert_ownership();
                settle();
            }
        }
        fixture.trace_native_selection("after replay interruption", 2, true);
        if matches!(interruption, ReplayInterruption::ReplaceQueryOnSameOutput) {
            fixture.assert_match(2, true, "needle-D");
        } else {
            fixture.assert_output_selection_empty(2);
        }
        fixture.assert_open_outputs(&[2]);
        fixture.assert_scroll_stays_parked();
    }
}

fn large_replay_preserves_narrowed_native_cursor(mode: crate::config::TerminalMode) {
    let repeated_tail =
        "repeat\t\x1b[36m界 wide retained replay payload across a narrowed terminal column boundary\x1b[0m\r\n";
    let output = format!(
        "head needle-B\r\nhead needle-C\r\nhead needle-D\r\n{}tail needle-F\r\n",
        repeated_tail.repeat(160),
    );
    assert!(
        output.len() > 8 * 1024,
        "the native replay must exceed 8 KiB"
    );
    let fixture = Fixture::with_second_output(mode, &output);
    let card = fixture.view.finished_blocks.borrow()[1].clone();
    card.set_collapsed(false);
    wait_until(
        || card.output_vte.is_mapped(),
        "the long decorated transcript must first be rendered",
    );
    let wide_stamp = card.render_stamp();
    assert!(wide_stamp.0 > 0);
    fixture.window.set_default_size(520, 480);
    fixture.window.queue_resize();
    wait_until(
        || card.render_stamp().0 < wide_stamp.0,
        "a real narrower allocation must rewrap the ANSI/tab/wide-character transcript",
    );
    assert_eq!(fixture.searchable_output(2), output);

    // Settle the new width while mapped, then change height while folded. The
    // reveal now genuinely replays the long, already-narrowed native document
    // without relying on a hidden widget receiving a fresh width allocation.
    fixture.collapse_all();
    let before_reveal = card.render_stamp();
    let old_height = fixture.view.block_scroll.height();
    fixture.window.set_default_size(520, 720);
    fixture.window.queue_resize();
    wait_until(
        || {
            assert!(!card.output_vte.is_mapped());
            fixture.view.block_scroll.height() > old_height + 100
        },
        "the larger viewport must settle with the narrowed output still folded",
    );
    assert_eq!(card.render_stamp(), before_reveal);
    fixture.focus_query();
    assert_eq!(
        fixture.search_now("needle-[A-DF]", true),
        find::FindSearchResult::Matches(progress(1, 5)),
    );
    fixture.step_now(false, 5, 5);
    assert_ne!(
        card.render_stamp(),
        before_reveal,
        "reveal must start a new native render"
    );
    fixture.step_now(false, 4, 5);
    fixture.step_now(true, 5, 5);
    fixture.step_now(false, 4, 5);
    fixture.step_now(true, 5, 5);
    fixture.assert_open_outputs(&[2]);
    // Finish at the sentinel beyond the >8 KiB repeated tail BEFORE the first
    // settle, so a repair which observes only a parsed prefix cannot pass.
    fixture.assert_match(2, true, "needle-F");
    fixture.assert_output_viewport_contains(2, "needle-F");
    assert!(card.render_stamp().0 < wide_stamp.0);
    assert_eq!(fixture.searchable_output(2), output);
    fixture.step(false, 4, 5, 2, true, "needle-D");
    let head_viewport = fixture.assert_output_viewport_contains(2, "needle-D");
    assert!(
        !head_viewport.contains("needle-F"),
        "the visible-text API must exclude the tail sentinel after navigating to the head",
    );
    fixture.step(true, 5, 5, 2, true, "needle-F");
    fixture.assert_output_viewport_contains(2, "needle-F");
    fixture.assert_scroll_stays_parked();
    fixture.assert_output_viewport_contains(2, "needle-F");
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn block_pending_replay_tracks_latest_navigation() {
    pending_replay_tracks_latest_navigation(crate::config::TerminalMode::Block);
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn chat_pending_replay_tracks_latest_navigation() {
    pending_replay_tracks_latest_navigation(crate::config::TerminalMode::Chat);
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn block_pending_replay_retires_after_interruption() {
    pending_replay_retires_after_interruption(crate::config::TerminalMode::Block);
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn chat_pending_replay_retires_after_interruption() {
    pending_replay_retires_after_interruption(crate::config::TerminalMode::Chat);
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn block_large_replay_preserves_narrowed_native_cursor() {
    large_replay_preserves_narrowed_native_cursor(crate::config::TerminalMode::Block);
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn chat_large_replay_preserves_narrowed_native_cursor() {
    large_replay_preserves_narrowed_native_cursor(crate::config::TerminalMode::Chat);
}

// Native controls around the real TermView backend. This deliberately does not
// call Fixture::step (which refocuses the query before each navigation), and it
// does not claim to exercise the application's top-level dispatcher.
fn native_find_controls_preserve_query(mode: crate::config::TerminalMode) {
    use std::process::Command;
    fn input(args: &[&str]) {
        let result = Command::new("xdotool").args(args).output().unwrap();
        assert!(result.status.success(), "xdotool {args:?}: {:?}", result);
        settle();
    }
    fn owns(window: &gtk::Window, widget: &impl IsA<gtk::Widget>) -> bool {
        gtk::prelude::RootExt::focus(window)
            .is_some_and(|focus| focus == *widget.as_ref() || focus.is_ancestor(widget))
    }
    fn click(window: &gtk::Window, id: &str, widget: &impl IsA<gtk::Widget>) {
        let widget = widget.as_ref();
        let point = widget
            .compute_point(
                window,
                &gtk::graphene::Point::new(
                    widget.width() as f32 / 2.0,
                    widget.height() as f32 / 2.0,
                ),
            )
            .expect("mapped native control coordinates");
        input(&[
            "mousemove",
            "--window",
            id,
            &point.x().round().to_string(),
            &point.y().round().to_string(),
        ]);
        input(&["mousedown", "1"]);
        // Separate native press/release across multiple frame/debounce turns.
        settle();
        input(&["mouseup", "1"]);
    }
    let fixture = Rc::new(Fixture::new(mode));
    let bar = fixture
        .query
        .ancestor(gtk::SearchBar::static_type())
        .unwrap()
        .downcast::<gtk::SearchBar>()
        .unwrap();
    bar.set_child(None::<&gtk::Widget>);
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    controls.append(&fixture.query);
    fixture.query.set_hexpand(true);
    let previous = gtk::Button::with_label("Previous");
    let next = gtk::Button::with_label("Next");
    previous.set_focus_on_click(false);
    next.set_focus_on_click(false);
    let case = gtk::ToggleButton::with_label("Aa");
    case.set_focus_on_click(false);
    controls.append(&case);
    controls.append(&previous);
    controls.append(&next);
    let weak = Rc::downgrade(&fixture);
    case.connect_toggled(move |button| {
        let fixture = weak.upgrade().unwrap();
        let result = fixture
            .view
            .find_in_blocks("needle", false, button.is_active());
        assert!(
            matches!(result, find::FindSearchResult::Matches(_)),
            "{result:?}"
        );
    });
    let outside = gtk::Entry::new();
    outside.set_placeholder_text(Some("Independent focus owner"));
    controls.append(&outside);
    bar.set_child(Some(&controls));
    bar.connect_entry(&fixture.query);
    let activations = Rc::new(Cell::new(0usize));
    let last_current = Rc::new(Cell::new(0usize));
    for (button, forward) in [(&next, true), (&previous, false)] {
        let weak = Rc::downgrade(&fixture);
        let activations = activations.clone();
        let last_current = last_current.clone();
        button.connect_clicked(move |_| {
            let fixture = weak.upgrade().unwrap();
            let result = if forward {
                fixture.view.find_next()
            } else {
                fixture.view.find_prev()
            };
            assert!(
                matches!(result, find::FindNavigationResult::Progress(_)),
                "{result:?}"
            );
            if let find::FindNavigationResult::Progress(value) = result {
                last_current.set(value.current);
            }
            activations.set(activations.get() + 1);
        });
    }
    let weak = Rc::downgrade(&fixture);
    let key_activations = activations.clone();
    let key_current = last_current.clone();
    fixture.query.connect_activate(move |_| {
        let fixture = weak.upgrade().unwrap();
        let result = fixture.view.find_next();
        assert!(
            matches!(result, find::FindNavigationResult::Progress(_)),
            "{result:?}"
        );
        if let find::FindNavigationResult::Progress(value) = result {
            key_current.set(value.current);
        }
        key_activations.set(key_activations.get() + 1);
    });
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(&fixture);
    let key_activations = activations.clone();
    let key_current = last_current.clone();
    controller.connect_key_pressed(move |_, key, _, state| {
        if matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter)
            && state.contains(gtk::gdk::ModifierType::SHIFT_MASK)
        {
            let fixture = weak.upgrade().unwrap();
            let result = fixture.view.find_prev();
            assert!(
                matches!(result, find::FindNavigationResult::Progress(_)),
                "{result:?}"
            );
            if let find::FindNavigationResult::Progress(value) = result {
                key_current.set(value.current);
            }
            key_activations.set(key_activations.get() + 1);
            gtk::glib::Propagation::Stop
        } else {
            gtk::glib::Propagation::Proceed
        }
    });
    fixture.query.add_controller(controller);
    let title = format!("native-find-controls-{}", std::process::id());
    fixture.window.set_title(Some(&title));
    settle();
    let result = Command::new("xdotool")
        .args(["search", "--name", &title])
        .output()
        .unwrap();
    assert!(result.status.success());
    let id = String::from_utf8(result.stdout)
        .unwrap()
        .lines()
        .last()
        .unwrap()
        .to_owned();
    input(&["windowfocus", "--sync", &id]);
    assert!(matches!(
        fixture.search("needle", false),
        find::FindSearchResult::Matches(_)
    ));
    settle();
    fixture.query.set_position(4);
    let selection = fixture.query.selection_bounds();
    let caret = fixture.query.position();
    assert_eq!(selection, None);
    assert_eq!(caret, 4);
    let unchanged = || {
        assert_eq!(fixture.query.text().as_str(), "needle");
        assert_eq!(
            fixture.query.selection_bounds(),
            selection,
            "query selection changed"
        );
        assert_eq!(fixture.query.position(), caret, "query caret changed");
    };
    let activated = |count, current| {
        wait_until(
            || activations.get() >= count,
            "native find activation must complete",
        );
        assert_eq!(
            activations.get(),
            count,
            "one action must navigate exactly once"
        );
        assert_eq!(
            last_current.get(),
            current,
            "native action must navigate in the requested direction"
        );
    };
    // Search starts at command A; Next must reveal the still-folded output B.
    assert!(fixture
        .view
        .finished_blocks
        .borrow()
        .iter()
        .find(|card| card.id == 2)
        .unwrap()
        .is_output_collapsed());
    click(&fixture.window, &id, &next);
    activated(1, 2);
    fixture.assert_match(2, true, "needle");
    unchanged();
    assert!(!fixture
        .view
        .finished_blocks
        .borrow()
        .iter()
        .find(|card| card.id == 2)
        .unwrap()
        .is_output_collapsed());
    click(&fixture.window, &id, &previous);
    activated(2, 1);
    fixture.assert_match(1, false, "needle");
    unchanged();
    click(&fixture.window, &id, &case);
    wait_until(|| case.is_active(), "pointer case toggle must complete");
    assert!(case.is_active());
    fixture.assert_ownership();
    unchanged();
    input(&["key", "Return"]);
    activated(3, 2);
    fixture.assert_match(2, true, "needle");
    unchanged();
    input(&["key", "shift+Return"]);
    activated(4, 1);
    fixture.assert_match(1, false, "needle");
    unchanged();
    input(&["key", "Tab"]);
    wait_until(
        || owns(&fixture.window, &case),
        "native focus must reach case",
    );
    assert!(owns(&fixture.window, &case));
    input(&["key", "space"]);
    wait_until(|| !case.is_active(), "keyboard case toggle must complete");
    assert!(!case.is_active());
    wait_until(
        || owns(&fixture.window, &case),
        "native focus must reach case",
    );
    assert!(owns(&fixture.window, &case));
    unchanged();
    // An ordinary Tab must leave the entry. Space activates the focused native
    // button and must not be redirected into the query or steal focus back.
    input(&["key", "Tab"]);
    wait_until(
        || owns(&fixture.window, &previous),
        "native focus must reach previous",
    );
    assert!(owns(&fixture.window, &previous), "Tab must focus Previous");
    input(&["key", "space"]);
    activated(5, 5);
    wait_until(
        || owns(&fixture.window, &previous),
        "native focus must reach previous",
    );
    assert!(owns(&fixture.window, &previous));
    unchanged();
    input(&["key", "Tab"]);
    wait_until(
        || owns(&fixture.window, &next),
        "native focus must reach next",
    );
    assert!(owns(&fixture.window, &next), "Tab must focus Next");
    input(&["key", "space"]);
    activated(6, 1);
    wait_until(
        || owns(&fixture.window, &next),
        "native focus must reach next",
    );
    assert!(owns(&fixture.window, &next));
    unchanged();
    input(&["key", "Tab"]);
    wait_until(
        || owns(&fixture.window, &outside),
        "native Tab must reach outside editor",
    );
    assert!(
        owns(&fixture.window, &outside),
        "Tab must reach outside editor"
    );
    input(&["type", "outside"]);
    wait_until(
        || outside.text().as_str() == "outside",
        "typing must reach outside editor",
    );
    assert_eq!(outside.text().as_str(), "outside");
    unchanged();
    // A later pointer navigation may update the match, but must not undo the
    // user's deliberate move to an unrelated editable widget.
    click(&fixture.window, &id, &next);
    wait_until(
        || owns(&fixture.window, &outside),
        "native focus must reach outside",
    );
    assert!(owns(&fixture.window, &outside));
    activated(7, 2);
    unchanged();
    // A selected query and a VTE match cannot both own PRIMARY. GTK's
    // GtkText content-detach collapses to the range's anchor (2), not its
    // directed caret (5). Measure this native distinction separately rather
    // than asserting impossible simultaneous selections.
    assert!(fixture.query.grab_focus());
    fixture.query.select_region(2, 5);
    assert_eq!(fixture.query.position(), 5);
    assert_eq!(fixture.query.selection_bounds(), Some((2, 5)));
    let point = next
        .compute_point(
            &fixture.window,
            &gtk::graphene::Point::new(next.width() as f32 / 2.0, next.height() as f32 / 2.0),
        )
        .unwrap();
    input(&[
        "mousemove",
        "--window",
        &id,
        &point.x().round().to_string(),
        &point.y().round().to_string(),
    ]);
    input(&["mousedown", "1"]);
    settle();
    fixture.assert_ownership();
    assert_eq!(
        fixture.query.position(),
        5,
        "pointer press must not move the query caret"
    );
    assert_eq!(
        fixture.query.selection_bounds(),
        Some((2, 5)),
        "pointer press must retain the selection until native navigation"
    );
    assert_eq!(activations.get(), 7, "press alone must not navigate");
    input(&["mouseup", "1"]);
    activated(8, 3);
    fixture.assert_match(2, true, "needle");
    assert_eq!(
        fixture.query.selection_bounds(),
        None,
        "native match must receive PRIMARY"
    );
    assert_eq!(
        fixture.query.position(),
        2,
        "GTK PRIMARY loss collapses to the original selection anchor"
    );
    input(&["type", "X"]);
    wait_until(
        || fixture.query.text().as_str() == "neXedle",
        "typing must continue at GTK's collapse endpoint",
    );
    assert_eq!(fixture.query.position(), 3);
    fixture.assert_ownership();

    // Start an independent collapsed-caret editing scenario. No focus request
    // or caret repair occurs between navigation and the ensuing native typing.
    fixture.query.set_text("needle");
    fixture.query.set_position(4);
    click(&fixture.window, &id, &next);
    activated(9, 4);
    fixture.assert_match(2, true, "needle");
    assert_eq!(fixture.query.selection_bounds(), None);
    assert_eq!(fixture.query.position(), 4);
    input(&["type", "X"]);
    wait_until(
        || fixture.query.text().as_str() == "needXle",
        "navigation must preserve the insertion caret",
    );
    assert_eq!(fixture.query.position(), 5);
    fixture.assert_ownership();
    fixture.window.close();
}

#[test]
#[ignore = "requires isolated X11 DISPLAY and xdotool"]
fn block_native_find_controls_keep_query_caret_and_tab_focus() {
    native_find_controls_preserve_query(crate::config::TerminalMode::Block);
}

#[test]
#[ignore = "requires isolated X11 DISPLAY and xdotool"]
fn chat_native_find_controls_keep_query_caret_and_tab_focus() {
    native_find_controls_preserve_query(crate::config::TerminalMode::Chat);
}
