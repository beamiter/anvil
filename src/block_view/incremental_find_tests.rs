//! Native incremental find must reveal its output without taking the find entry's focus.
use super::*;
use std::time::{Duration, Instant};

const CARD_COUNT: u64 = 12;

fn settle() {
    let context = gtk::glib::MainContext::default();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(250) {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_until(mut ready: impl FnMut() -> bool, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready() {
        assert!(Instant::now() < deadline, "{message}");
        settle();
    }
    settle();
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
        crate::child_env::capture_inherited_environment().expect("test child environment");
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
        self.query.set_text(query);
        self.focus_query();
        let result = self
            .view
            .find_in_blocks(self.query.text().as_str(), regex, false);
        eprintln!("FIND_FIXTURE search query={query:?} regex={regex} result={result:?}");
        for (id, output) in [(1, false), (2, true), (3, true)] {
            self.trace_native_selection("search immediate", id, output);
        }
        self.assert_ownership();
        result
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
