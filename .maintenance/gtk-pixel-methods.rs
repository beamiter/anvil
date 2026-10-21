    fn presentation_offset(&self, state: &PaintState, frame: &Vim9RenderFrame, window: i64) -> f32 {
        if let Some(scroll) = self.imp().pixels.borrow().get(&window) {
            return scroll.offset(frame, state.metrics);
        }
        self.imp().motions.borrow().get(&window)
            .map_or(0.0, |motion| motion.animation.offset(state.time) as f32)
    }

    fn presented_cursor(&self) -> Option<Vim9PixelRect> {
        let state = self.imp().state.borrow();
        let frame = state.frame.as_ref()?;
        let offset = self.presentation_offset(&state, frame, frame.window);
        if self.imp().pixels.borrow().contains_key(&frame.window) {
            return Vim9Viewport::new(frame, frame.window, state.metrics, offset)?.cursor_rect();
        }
        let mut cursor = state.metrics.cursor_rect(&frame.cursor)?;
        cursor.y += offset;
        Some(cursor)
    }

    /// Hit the last submitted native image, not a newer frame received between
    /// presentation ticks. Source generation is checked again before dispatch.
    fn source_at(&self, x: f64, y: f64) -> Option<Vim9BufferPosition> {
        if !x.is_finite() || !y.is_finite() { return None; }
        let state = self.imp().state.borrow();
        if state.preedit.is_some() { return None; }
        let current = state.frame.as_ref()?;
        if !matches!(current.mode.as_str(), "n" | "v") { return None; }
        let presented = self.imp().presented.borrow();
        let presented = presented.as_ref()?;
        if presented.metrics != state.metrics { return None; }
        for window in &presented.frame.windows {
            let live = current.windows.iter().find(|w| w.window == window.window)?;
            if !window.native_pointer || !live.native_pointer || live.buffer != window.buffer
                || live.changedtick != window.changedtick || live.view != window.view
                || live.leftcol != window.leftcol || live.textoff != window.textoff {
                continue;
            }
            let offset = presented.offsets.get(&window.window).copied().unwrap_or(0.0);
            if let Some(view) = Vim9Viewport::new(&presented.frame, window.window, presented.metrics, offset) {
                if let Some(anchor) = view.anchor_at(x as f32, y as f32) { return Some(anchor.position); }
            }
        }
        None
    }

    fn pointer_command(position: Vim9BufferPosition, gesture: u64, phase: Vim9PointerPhase) -> Vim9Command {
        Vim9Command::Pointer { window: position.window, buffer: position.buffer,
            changedtick: position.changedtick, line: position.line,
            byte_column: position.byte_column, gesture, phase }
    }

    fn press_source(&self, session: &Vim9Session, x: f64, y: f64) -> bool {
        let Some(position) = self.source_at(x, y) else { return false; };
        let gesture = self.imp().gesture_sequence.get() % i32::MAX as u64 + 1;
        let Ok(request) = session.send(Self::pointer_command(position, gesture, Vim9PointerPhase::Press)) else {
            return false;
        };
        self.imp().gesture_sequence.set(gesture);
        *self.imp().source_gesture.borrow_mut() = Some(SourceGesture {
            gesture, request, last: position, pending: None, released: false,
        });
        // Drop unsent legacy wheel impulses; an already accepted source-bound
        // scroll stays ordered ahead of the click on the same bridge channel.
        let mut input = self.imp().scroll_input.borrow_mut();
        input.pending.clear(); input.fractions.clear(); input.requested.clear();
        self.imp().motions.borrow_mut().clear();
        true
    }

    fn queue_source_pointer(&self, x: f64, y: f64, phase: Vim9PointerPhase) -> bool {
        let target = self.source_at(x, y);
        let mut gesture = self.imp().source_gesture.borrow_mut();
        let Some(gesture) = gesture.as_mut() else { return false; };
        if gesture.released { return true; }
        let position = target.filter(|p| p.window == gesture.last.window && p.buffer == gesture.last.buffer
            && p.changedtick == gesture.last.changedtick);
        if phase == Vim9PointerPhase::Release {
            // Releasing outside the split completes at the last admitted
            // source target, never at another split or fabricated source row.
            gesture.pending = Some((position.unwrap_or(gesture.last), phase));
            gesture.released = true;
        } else if let Some(position) = position {
            if position != gesture.last {
                gesture.pending = Some((position, phase));
                gesture.last = position;
            }
        }
        true
    }

    fn advance_native_input(&self, session: &Vim9Session, next: &Option<Arc<Vim9RenderFrame>>,
        metrics: Metrics, enabled: bool) {
        self.imp().pixel_enabled.set(enabled);
        let Some(frame) = next.as_ref() else {
            self.imp().pixels.borrow_mut().clear();
            self.imp().source_gesture.borrow_mut().take();
            self.imp().presented.borrow_mut().take();
            return;
        };
        let changed_geometry = {
            let state = self.imp().state.borrow();
            state.metrics != metrics || state.frame.as_ref().is_some_and(|old| {
                old.rows != frame.rows || old.columns != frame.columns
                    || old.windows.len() != frame.windows.len()
                    || old.windows.iter().zip(&frame.windows).any(|(a, b)| a.window != b.window || a.view != b.view)
            })
        };
        let mut pointer = self.imp().source_gesture.borrow_mut();
        if changed_geometry || !matches!(frame.mode.as_str(), "n" | "v")
            || pointer.as_ref().is_some_and(|gesture| !frame.windows.iter().any(|w|
                w.window == gesture.last.window && w.buffer == gesture.last.buffer
                && w.changedtick == gesture.last.changedtick && w.native_pointer)) {
            *pointer = None;
        }
        let mut pixels = self.imp().pixels.borrow_mut();
        let before = pixels.len();
        if !enabled || changed_geometry || (frame.mode != "n" && pointer.is_none()) {
            pixels.clear();
        } else {
            pixels.retain(|window, scroll| {
                frame.windows.iter().find(|w| w.window == *window).is_some_and(|w| {
                    let pending = scroll.request_id().is_some_and(|id| session.command_pending(id));
                    scroll.reconcile(w, metrics, pending)
                })
            });
        }
        if pixels.len() != before { self.queue_draw(); }
        if pointer.is_none() && frame.mode == "n" {
            for scroll in pixels.values_mut() {
                if let Some(command) = scroll.command() {
                    if let Ok(id) = session.send(command) { scroll.submitted(id); }
                }
            }
        }
        if let Some(gesture) = pointer.as_mut() {
            if !session.command_pending(gesture.request) {
                if let Some((position, phase)) = gesture.pending.take() {
                    match session.send(Self::pointer_command(position, gesture.gesture, phase)) {
                        Ok(id) => { gesture.request = id; gesture.last = position; }
                        // Fail closed on transport saturation/disconnect. Do
                        // not replay an old drag after unrelated keyboard input.
                        Err(_) => { *pointer = None; }
                    }
                } else if gesture.released { *pointer = None; }
            }
        }
    }
