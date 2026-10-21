    #[test]
    #[ignore = "requires GTK display; exercised by the native pixel integration CI"]
    fn native_pixel_snapshot_and_source_hit_round_trip() {
        gtk::init().expect("GTK display required");
        use jterm_core::vim9::{Vim9RenderWindow, Vim9RenderRun, Vim9RenderCursor, Vim9RenderMarginRow};
        let surface = BufferSurface::new();
        let metrics = Metrics { origin_x: 0.0, origin_y: 0.0, cell_width: 8.0, line_height: 20.0 };
        let cursor = Vim9RenderCursor { row: 1, column: 0, width: 1, visible: true, line: 11, byte_column: 0 };
        let region = Vim9RenderRect { row: 0, column: 0, rows: 4, columns: 8 };
        let run = |row, line, text: &str| Vim9RenderRun { window: 10, row, column: 0,
            cells: text.len(), text: text.into(), line, byte_start: 0, byte_end: text.len(), ..Default::default() };
        let frame = Arc::new(Vim9RenderFrame {
            revision: 1, supported: true, rows: 6, columns: 8, window: 10, buffer: 1,
            changedtick: 3, mode: "n".into(), view: region.clone(), cursor: cursor.clone(),
            windows: vec![Vim9RenderWindow {
                window: 10, buffer: 1, changedtick: 3, view: region, cursor,
                topline: 10, botline: 13, line_count: 100, native_pointer: true,
                line_lengths: vec![2, 2, 2, 0], overscan: vec![
                    Vim9RenderMarginRow { row: -1, line: 9, byte_length: 3, runs: vec![run(0, 9, "pre")] },
                    Vim9RenderMarginRow { row: 4, line: 14, byte_length: 4, runs: vec![run(0, 14, "post")] },
                ], ..Default::default()
            }], runs: vec![run(0, 10, "AB"), run(1, 11, "CD"), run(2, 12, "EF")], ..Default::default()
        });
        { let mut state = surface.imp().state.borrow_mut(); state.metrics = metrics; state.frame = Some(frame.clone()); }
        surface.imp().pixel_enabled.set(true);
        assert!(surface.enqueue_scroll(8.0, 10.0, 6.25, gdk::ScrollUnit::Surface));
        assert_eq!(surface.presentation_offset(&surface.imp().state.borrow(), &frame, 10), -6.25);
        assert_eq!(surface.presented_cursor().unwrap().y, 13.75);
        // Execute the actual GTK snapshot/Pango path, including a partly
        // exposed semantic margin. No GPU/hardware latency is asserted here.
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_some());
        assert_eq!(surface.source_at(9.0, 75.0).unwrap().line, 14);
        assert_eq!(surface.source_at(9.0, 75.0).unwrap().byte_column, 1);
        assert_eq!(surface.source_at(45.0, 65.0).unwrap().line, 13);
        assert!(surface.source_at(65.0, 75.0).is_none());
        // Input must still hit the submitted image when a new frame is
        // received before GTK has submitted its replacement snapshot.
        let mut received = (*frame).clone();
        received.revision += 1;
        received.windows[0].topline += 1;
        received.windows[0].botline += 1;
        surface.imp().state.borrow_mut().frame = Some(Arc::new(received));
        assert_eq!(surface.source_at(9.0, 75.0).unwrap().line, 14);
        surface.imp().state.borrow_mut().frame = Some(frame.clone());
        surface.cancel_scroll();
        assert!(surface.imp().pixels.borrow().is_empty());
        assert!(surface.imp().source_gesture.borrow().is_none());
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_some());
        assert_eq!(surface.source_at(9.0, 75.0).unwrap().line, 13);
        assert_eq!(surface.presented_cursor().unwrap().y, 20.0);
        let mut edited = (*frame).clone(); edited.windows[0].changedtick += 1;
        surface.imp().state.borrow_mut().frame = Some(Arc::new(edited));
        assert!(surface.source_at(9.0, 75.0).is_none());
    }
