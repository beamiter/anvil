from pathlib import Path
import os
import tomllib

HERE = Path(__file__).resolve().parent
p = Path('src/vim9_ide_buffer.rs')
s = p.read_text()


def replace(old, new):
    global s
    if s.count(old) != 1:
        raise SystemExit(f'Expected one GTK patch site ({s.count(old)}): {old[:100]!r}')
    s = s.replace(old, new)


replace('    Vim9Command, Vim9PixelRect,', '    Vim9BufferPosition, Vim9PixelScroll, Vim9PointerPhase, Vim9Viewport,\n    Vim9Command, Vim9PixelRect,')
replace('struct PaintState {', '''struct PresentedFrame {
    frame: Arc<Vim9RenderFrame>, metrics: Metrics, offsets: HashMap<i64, f32>,
}

struct SourceGesture {
    gesture: u64, request: u64, last: Vim9BufferPosition,
    pending: Option<(Vim9BufferPosition, Vim9PointerPhase)>, released: bool,
}

struct PaintState {''')
replace('        pub(super) pointer: Cell<Option<(f64, f64)>>,', '''        pub(super) pointer: Cell<Option<(f64, f64)>>,
        pub(super) pixels: RefCell<HashMap<i64, Vim9PixelScroll>>,
        pub(super) pixel_enabled: Cell<bool>,
        pub(super) source_gesture: RefCell<Option<SourceGesture>>,
        pub(super) gesture_sequence: Cell<u64>,
        pub(super) presented: RefCell<Option<PresentedFrame>>,''')
# Reuse the exact Pango shaping/decorations code; only geometry comes from the
# shared projection. This avoids divergence between the two GTK applications.
a = s.index('            let (foreground, _, special) = colors(&run.style, &frame.default_style);', s.index('fn build_text_scene('))
b = s.index('\n        }\n\n        snapshot.pop();', a)
glyph = s[a:b].replace('&snapshot,', 'snapshot,')
projected = '''fn paint_pixel_scene(surface: &BufferSurface, snapshot: &gtk::Snapshot,
    state: &PaintState, frame: &Vim9RenderFrame,
    layouts: &mut HashMap<LayoutKey, pango::Layout>) {
    let metrics = state.metrics;
    let baseline = row_baseline(surface, state);
    if layouts.len() > 4096 { layouts.clear(); }
    for window in &frame.windows {
        let offset = surface.presentation_offset(state, frame, window.window);
        let Some(view) = Vim9Viewport::new(frame, window.window, metrics, offset) else { continue; };
        snapshot.push_clip(&graphene_rect(view.clip()));
        let (_, background, _) = colors(&frame.default_style, &frame.default_style);
        snapshot.append_color(&rgb(background), &graphene_rect(view.clip()));
        for projected in view.runs() {
            let (_, background, _) = colors(&projected.run.style, &frame.default_style);
            snapshot.append_color(&rgb(background), &graphene_rect(projected.clip));
        }
        for projected in view.runs() {
            let run = projected.run;
            let x = projected.rect.x;
            let y = projected.rect.y;
            let width = projected.rect.width;
''' + glyph + '''
        }
        snapshot.pop();
    }
}

'''
replace('fn graphene_rect(rect: Vim9PixelRect)', projected + 'fn graphene_rect(rect: Vim9PixelRect)')
# Retained nodes remain the idle/legacy path. Direct pixel gestures use real
# margin rows; translating the old, already-clipped scene would lose edge ink.
a = s.index('            let mut scene = self.text_scene.borrow_mut();')
b = s.index('            // Cursor and composition are transient layers', a)
old = s[a:b]
s = s[:a] + '''            if !self.pixels.borrow().is_empty() {
                paint_pixel_scene(&surface, snapshot, &state, frame, &mut self.layouts.borrow_mut());
            } else {
''' + old + '''            }
            *self.presented.borrow_mut() = Some(PresentedFrame {
                frame: frame.clone(), metrics,
                offsets: frame.windows.iter().map(|w| (w.window,
                    surface.presentation_offset(&state, frame, w.window))).collect(),
            });
''' + s[b:]
replace('let Some(cursor) = metrics.cursor_rect(&frame.cursor).map(graphene_rect) else {',
        'let Some(cursor) = surface.presented_cursor().map(graphene_rect) else {')
a = s.index('                    let offset = self\n                        .motions', s.index('// Cursor and composition'))
b = s.index('                    paint_cursor(', a)
s = s[:a] + s[b:]
replace('                    snapshot.restore();\n                }\n                snapshot.pop();\n            }\n            #[cfg(test)]',
        '                }\n                snapshot.pop();\n            }\n            #[cfg(test)]')
replace('    pub(super) fn cancel_scroll(&self) {', '''    pub(super) fn cancel_scroll(&self) {
        self.imp().pixels.borrow_mut().clear();
        self.imp().source_gesture.borrow_mut().take();''')
replace('        let was_animating = !motions.is_empty();', '''        if !self.imp().pixels.borrow().is_empty() {
            motions.clear();
            return;
        }
        let was_animating = !motions.is_empty();''')
replace('        let count = input.fractions.add(window.window, lines);', '''        if self.imp().source_gesture.borrow().is_some() { return true; }
        if unit == gdk::ScrollUnit::Surface && self.imp().pixel_enabled.get()
            && frame.mode == "n" && window.native_pointer
            && !input.pending.contains_key(&window.window) {
            let mut pixels = self.imp().pixels.borrow_mut();
            if let Some(scroll) = pixels.get_mut(&window.window) {
                scroll.push(lines);
            } else if let Some(mut scroll) = Vim9PixelScroll::new(window, state.metrics) {
                scroll.push(lines);
                pixels.insert(window.window, scroll);
            } else { return false; }
            input.fractions.discard(window.window);
            self.imp().motions.borrow_mut().clear();
            self.queue_draw();
            return true;
        }
        // Discrete wheels keep their established acknowledged-row animation.
        self.imp().pixels.borrow_mut().clear();
        let count = input.fractions.add(window.window, lines);''')
replace('    #[cfg(test)]\n    pub(super) fn scroll_offset', HERE.joinpath('gtk-pixel-methods.rs').read_text() + '\n    #[cfg(test)]\n    pub(super) fn scroll_offset')
replace('        motion.set_name(Some("vim9-buffer-pointer"));', '''        motion.set_name(Some("vim9-buffer-pointer"));
        motion.set_propagation_phase(gtk::PropagationPhase::Capture);''')
replace('                    surface.imp().pointer.set(Some((x, y)));', '''                    surface.imp().pointer.set(Some((x, y)));
                    surface.queue_source_pointer(x, y, Vim9PointerPhase::Drag);''')
a = s.index('        // Finish the short visual easing tail')
b = s.index('        // Zoom belongs to the buffer', a)
s = s[:a] + '''        // Keyboard/preedit cancels unsent native gestures before PTY input.
        let editing = gtk::EventControllerKey::new();
        editing.set_name(Some("vim9-buffer-scroll-stop"));
        editing.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let surface = surface.downgrade();
            editing.connect_key_pressed(move |_, _, _, _| {
                if let Some(surface) = surface.upgrade() { surface.cancel_scroll(); }
                glib::Propagation::Proceed
            });
        }
        terminal.add_controller(editing);
        let clicking = gtk::GestureClick::new();
        clicking.set_button(0);
        clicking.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let surface = surface.downgrade();
            let terminal = terminal.downgrade();
            let session = session.clone();
            clicking.connect_pressed(move |gesture, count, x, y| {
                let Some(surface) = surface.upgrade() else { return; };
                let modified = gesture.current_event_state().intersects(
                    gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK
                        | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK);
                if gesture.current_button() == 1 && count == 1 && !modified
                    && surface.press_source(&session, x, y) {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    if let Some(terminal) = terminal.upgrade() { terminal.grab_focus(); }
                } else { surface.cancel_scroll(); }
            });
        }
        {
            let surface = surface.downgrade();
            clicking.connect_released(move |gesture, _, x, y| {
                if let Some(surface) = surface.upgrade() {
                    if surface.queue_source_pointer(x, y, Vim9PointerPhase::Release) {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                    }
                }
            });
        }
        terminal.add_controller(clicking);

''' + s[b:]
replace('                surface.imp().state.borrow_mut().focused = terminal.has_focus();', '''                if !terminal.has_focus() { surface.cancel_scroll(); }
                surface.imp().state.borrow_mut().focused = terminal.has_focus();''')
replace('            poll_surface.retarget_scroll(&next, metrics, now, animations);', '''            poll_surface.advance_native_input(&session, &next, metrics, active && animations);
            poll_surface.retarget_scroll(&next, metrics, now, animations);''')
replace('''                if let Some(cursor) = frame
                    .as_ref()
                    .and_then(|frame| metrics.cursor_rect(&frame.cursor))''',
        '                if let Some(cursor) = poll_surface.presented_cursor()')
replace('Animate acknowledged Vim viewport movement; obeys system reduced-motion settings.',
        'Direct precision-device pixels with source-checked clicks; discrete wheels use bounded easing. Obeys reduced-motion settings.')
s = s.rstrip()[:-1] + '\n' + HERE.joinpath('gtk-pixel-tests.rs').read_text() + '\n}\n'
p.write_text(s)
# Only update the exact shared dependency; never re-resolve unrelated packages.
target = os.environ['CORE_REVISION']
manifest, lock = Path('Cargo.toml'), Path('Cargo.lock')
text, old_lock = manifest.read_text(), lock.read_text()
parsed = tomllib.loads(text)
pins = [parsed[section]['jterm_core']['rev'] for section in ('dependencies', 'dev-dependencies')]
if len(set(pins)) != 1 or text.count(pins[0]) != 2: raise SystemExit('unexpected core pins')
old = pins[0]
source = f'git+https://github.com/beamiter/jterm_core.git?rev={old}#{old}'
if old_lock.count(source) != 1: raise SystemExit('unexpected lock source')
manifest.write_text(text.replace(old, target))
lock.write_text(old_lock.replace(source, source.replace(old, target)))
print('Applied GTK direct pixel presentation and source pointer routing')
