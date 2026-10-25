//! Buffer text is shaped by Pango and painted by GTK from Vim's semantic RGB
//! render frames. No terminal cells, ANSI escapes, or VTE palette entries are
//! consulted here. VTE remains the PTY/input adapter and compatibility view.

use super::gtk;
use gtk::{gdk, glib, pango, subclass::prelude::*};
use jterm_core::vim9::{
    Vim9BufferPosition, Vim9Command, Vim9CursorBlink, Vim9InputSnapshot, Vim9PixelRect,
    Vim9PixelScroll, Vim9PointerPhase, Vim9RenderCursor, Vim9RenderFrame, Vim9RenderMetrics,
    Vim9RenderPopup, Vim9RenderRect, Vim9RenderRun, Vim9RenderStyle, Vim9RenderWindow,
    Vim9ScrollAccumulator, Vim9ScrollAnimation, Vim9Session, Vim9Viewport,
};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use vte4::prelude::*;

const DEFAULT_FOREGROUND: u32 = 0xd9e0ee;
const DEFAULT_BACKGROUND: u32 = 0x121720;

type Metrics = Vim9RenderMetrics;

#[derive(Clone)]
struct Preedit {
    text: String,
    attributes: pango::AttrList,
    cursor: i32,
}

struct PresentedFrame {
    frame: Arc<Vim9RenderFrame>,
    metrics: Metrics,
    offsets: HashMap<i64, f32>,
    precision_windows: Vec<i64>,
    cursor: Option<Vim9PixelRect>,
}

struct SourceGesture {
    gesture: u64,
    request: u64,
    last: Vim9BufferPosition,
    pending: Option<(Vim9BufferPosition, Vim9PointerPhase)>,
    released: bool,
}

struct PaintState {
    cursor_blink: Vim9CursorBlink,
    cursor_on: bool,
    frame: Option<Arc<Vim9RenderFrame>>,
    metrics: Metrics,
    font: pango::FontDescription,
    font_generation: u64,
    ligatures: bool,
    focused: bool,
    preedit: Option<Preedit>,
    time: f64,
}

impl Default for PaintState {
    fn default() -> Self {
        Self {
            cursor_blink: Vim9CursorBlink::default(),
            cursor_on: true,
            frame: None,
            metrics: Metrics {
                origin_x: 0.0,
                origin_y: 0.0,
                cell_width: 0.0,
                line_height: 0.0,
            },
            font: pango::FontDescription::from_string("Monospace 12"),
            font_generation: 0,
            ligatures: true,
            focused: false,
            preedit: None,
            time: 0.0,
        }
    }
}

impl PaintState {
    fn restart_cursor_blink(&mut self) {
        self.cursor_blink.reset();
        self.cursor_on = true;
    }
}

/// Style buckets allow borrowed text lookup: a warm paint does not allocate
/// a new String for each syntax run just to query the layout cache.
struct StyledCache<T> {
    buckets: [HashMap<String, T>; 4],
}

impl<T> Default for StyledCache<T> {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| HashMap::new()),
        }
    }
}

impl<T> StyledCache<T> {
    fn bucket(bold: bool, italic: bool) -> usize {
        usize::from(bold) | (usize::from(italic) << 1)
    }

    fn get(&self, text: &str, bold: bool, italic: bool) -> Option<&T> {
        self.buckets[Self::bucket(bold, italic)].get(text)
    }

    fn get_or_insert_with(
        &mut self,
        text: &str,
        bold: bool,
        italic: bool,
        create: impl FnOnce() -> T,
    ) -> &T {
        let bucket = &mut self.buckets[Self::bucket(bold, italic)];
        if !bucket.contains_key(text) {
            bucket.insert(text.to_owned(), create());
        }
        &bucket[text]
    }

    fn len(&self) -> usize {
        self.buckets.iter().map(HashMap::len).sum()
    }

    fn clear(&mut self) {
        for bucket in &mut self.buckets {
            bucket.clear();
        }
    }
}

struct ShapedLayout {
    layout: pango::Layout,
    width: f32,
    baseline: f32,
}

type LayoutCache = StyledCache<ShapedLayout>;

/// Retain live shapes under cache pressure without reintroducing allocating
/// tuple keys. Style buckets preserve the same bold/italic shape identity.
fn trim_layout_cache<T>(frame: &Vim9RenderFrame, layouts: &mut StyledCache<T>) {
    if layouts.len() <= 4096 {
        return;
    }
    let live: HashSet<_> = frame
        .runs
        .iter()
        .chain(
            frame
                .windows
                .iter()
                .flat_map(|window| &window.overscan)
                .flat_map(|row| &row.runs),
        )
        .chain(frame.completion.iter().flat_map(|menu| &menu.runs))
        .chain(frame.popups.iter().flat_map(|popup| &popup.runs))
        .map(|run| {
            (
                StyledCache::<T>::bucket(run.style.bold, run.style.italic),
                run.text.as_str(),
            )
        })
        .collect();
    for (style, bucket) in layouts.buckets.iter_mut().enumerate() {
        bucket.retain(|text, _| live.contains(&(style, text.as_str())));
    }
}

/// Retain the immutable text/background scene independently of cursor, focus
/// and input-method overlays. A cursor-only frame reuses GTK render nodes.
struct TextScene {
    frame: Arc<Vim9RenderFrame>,
    metrics: Metrics,
    font_generation: u64,
    ligatures: bool,
    scale_factor: i32,
    node: gtk::gsk::RenderNode,
}

fn same_text_content(previous: &Vim9RenderFrame, next: &Vim9RenderFrame) -> bool {
    previous.default_style == next.default_style
        && previous.paint_views().eq(next.paint_views())
        && previous.runs.len() == next.runs.len()
        && (previous.runs == next.runs
            // Full frames arrive grouped by window; row deltas are grouped by
            // screen row. Compare the actual per-view painting order so that
            // their equivalent interleaving does not rebuild the text scene.
            || previous.paint_views().all(|view| {
                previous
                    .runs
                    .iter()
                    .filter(|run| view.contains(run.row, run.column))
                    .eq(next
                        .runs
                        .iter()
                        .filter(|run| view.contains(run.row, run.column)))
            }))
}

impl TextScene {
    fn matches(&self, state: &PaintState, frame: &Arc<Vim9RenderFrame>, scale_factor: i32) -> bool {
        self.metrics == state.metrics
            && self.font_generation == state.font_generation
            && self.ligatures == state.ligatures
            && self.scale_factor == scale_factor
            && (Arc::ptr_eq(&self.frame, frame) || same_text_content(&self.frame, frame))
    }
}

/// Popup text and completion stay in screen space. Cursor/focus snapshots
/// reuse this node independently of the buffer's scrolling text scene.
struct OverlayScene {
    frame: Arc<Vim9RenderFrame>,
    metrics: Metrics,
    font_generation: u64,
    ligatures: bool,
    scale_factor: i32,
    size: (i32, i32),
    node: Option<gtk::gsk::RenderNode>,
}

fn same_overlay_content(previous: &Vim9RenderFrame, next: &Vim9RenderFrame) -> bool {
    previous.default_style == next.default_style
        && previous.completion == next.completion
        && previous.popups == next.popups
}

impl OverlayScene {
    fn matches(
        &self,
        state: &PaintState,
        frame: &Arc<Vim9RenderFrame>,
        scale: i32,
        size: (i32, i32),
    ) -> bool {
        self.metrics == state.metrics
            && self.font_generation == state.font_generation
            && self.ligatures == state.ligatures
            && self.scale_factor == scale
            && self.size == size
            && (Arc::ptr_eq(&self.frame, frame) || same_overlay_content(&self.frame, frame))
    }
}

/// Each retained scene supplies rows exposed at the edge while a new Vim
/// viewport eases into place. Keeping the outgoing text prevents blank bands.
#[derive(Clone)]
struct ScrollScene {
    topline: u64,
    node: gtk::gsk::RenderNode,
}

#[derive(Default)]
struct WindowMotion {
    animation: Vim9ScrollAnimation,
    history: Vec<ScrollScene>,
}

#[derive(Default)]
struct ScrollInput {
    fractions: Vim9ScrollAccumulator,
    pending: HashMap<i64, i32>,
    requested: HashMap<i64, f64>,
}

/// Metadata distinguishes an explicit modern render-only window from old
/// producers whose optional native-input flags were absent on the wire.
fn render_only_window(window: &Vim9RenderWindow) -> bool {
    window.line_count > 0
        && !window.line_lengths.is_empty()
        && !window.native_pointer
        && !window.native_scroll
}

fn legacy_scroll_target(frame: &Vim9RenderFrame, window_id: i64) -> bool {
    frame
        .windows
        .iter()
        .any(|window| window.window == window_id && !render_only_window(window))
}

impl ScrollInput {
    fn retire_window(&mut self, window: i64) {
        self.fractions.discard(window);
        self.pending.remove(&window);
        self.requested.remove(&window);
    }

    fn retire_render_only(&mut self, frame: &Vim9RenderFrame) {
        for window in frame
            .windows
            .iter()
            .filter(|window| render_only_window(window))
        {
            self.retire_window(window.window);
        }
    }
}

fn paint_shifted_scene(
    snapshot: &gtk::Snapshot,
    node: &gtk::gsk::RenderNode,
    view: &gtk::graphene::Rect,
    offset: f32,
) {
    snapshot.save();
    snapshot.translate(&gtk::graphene::Point::new(0.0, offset));
    snapshot.push_clip(view);
    snapshot.append_node(node);
    snapshot.pop();
    snapshot.restore();
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct BufferSurface {
        pub(super) state: RefCell<PaintState>,
        pub(super) layouts: RefCell<LayoutCache>,
        pub(super) layout_context_serial: Cell<u32>,
        pub(super) text_scene: RefCell<Option<TextScene>>,
        pub(super) overlay_scene: RefCell<Option<OverlayScene>>,
        pub(super) motions: RefCell<HashMap<i64, WindowMotion>>,
        pub(super) scroll_input: RefCell<ScrollInput>,
        pub(super) pointer: Cell<Option<(f64, f64)>>,
        pub(super) pixels: RefCell<HashMap<i64, Vim9PixelScroll>>,
        pub(super) pixel_enabled: Cell<bool>,
        pub(super) wheel_animation: Cell<bool>,
        pub(super) source_gesture: RefCell<Option<SourceGesture>>,
        pub(super) gesture_sequence: Cell<u64>,
        pub(super) presented: RefCell<Option<PresentedFrame>>,
        #[cfg(test)]
        pub(super) painted_revision: Cell<u64>,
        #[cfg(test)]
        pub(super) scene_builds: Cell<usize>,
        #[cfg(test)]
        pub(super) overlay_builds: Cell<usize>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BufferSurface {
        const NAME: &'static str = "AnvilVim9BufferSurface";
        type Type = super::BufferSurface;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for BufferSurface {}

    impl WidgetImpl for BufferSurface {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let surface = self.obj();
            // Pango may update DPI/font options without changing GTK's integer
            // scale factor. Cached measurements and render nodes share that
            // context generation and must retire together.
            let serial = surface.pango_context().serial();
            if self.layout_context_serial.replace(serial) != serial {
                self.layouts.borrow_mut().clear();
                self.text_scene.borrow_mut().take();
                self.overlay_scene.borrow_mut().take();
                self.motions.borrow_mut().clear();
            }
            let state = self.state.borrow();
            let Some(frame) = state.frame.as_ref() else {
                self.presented.borrow_mut().take();
                self.overlay_scene.borrow_mut().take();
                return;
            };
            let metrics = state.metrics;
            if !metrics.is_valid() {
                self.presented.borrow_mut().take();
                return;
            }
            if !self.pixels.borrow().is_empty() {
                paint_pixel_scene(
                    &surface,
                    snapshot,
                    &state,
                    frame,
                    &mut self.layouts.borrow_mut(),
                );
            } else {
                let mut scene = self.text_scene.borrow_mut();
                if !scene
                    .as_ref()
                    .is_some_and(|cached| cached.matches(&state, frame, surface.scale_factor()))
                {
                    #[cfg(test)]
                    self.scene_builds.set(self.scene_builds.get() + 1);
                    let mut layouts = self.layouts.borrow_mut();
                    *scene = build_text_scene(&surface, &state, frame, &mut layouts).map(|node| {
                        TextScene {
                            frame: frame.clone(),
                            metrics,
                            font_generation: state.font_generation,
                            ligatures: state.ligatures,
                            scale_factor: surface.scale_factor(),
                            node,
                        }
                    });
                }
                if let Some(scene) = scene.as_mut() {
                    // Advance the frame identity after reusing the same scene, so
                    // another focus/preedit snapshot is an O(1) pointer check.
                    scene.frame = frame.clone();
                    let motions = self.motions.borrow();
                    if motions.is_empty() {
                        snapshot.append_node(&scene.node);
                    } else {
                        for window in &frame.windows {
                            let view = view_rectangle(&window.view, metrics);
                            snapshot.push_clip(&view);
                            let (_, background, _) = colors(
                                window_normal_style(frame, window.window),
                                &frame.default_style,
                            );
                            snapshot.append_color(&rgb(background), &view);
                            if let Some(motion) = motions.get(&window.window) {
                                let offset = motion.animation.offset(state.time) as f32;
                                if let Some(outgoing) = motion.history.first() {
                                    let delta = window.topline as f64 - outgoing.topline as f64;
                                    paint_shifted_scene(
                                        snapshot,
                                        &outgoing.node,
                                        &view,
                                        offset - (delta * f64::from(metrics.line_height)) as f32,
                                    );
                                }
                                paint_shifted_scene(snapshot, &scene.node, &view, offset);
                            } else {
                                snapshot.append_node(&scene.node);
                            }
                            snapshot.pop();
                        }
                    }
                }
                drop(scene);
            }
            let cursor = surface.projected_cursor();
            *self.presented.borrow_mut() = Some(PresentedFrame {
                frame: frame.clone(),
                metrics,
                offsets: frame
                    .windows
                    .iter()
                    .map(|w| {
                        (
                            w.window,
                            surface.presentation_offset(&state, frame, w.window),
                        )
                    })
                    .collect(),
                precision_windows: self.pixels.borrow().keys().copied().collect(),
                cursor,
            });
            // Cursor and composition are transient layers over the retained
            // buffer scene, and never invalidate unchanged shaped text.
            for region in frame.paint_views() {
                let view = view_rectangle(region, metrics);
                snapshot.push_clip(&view);
                if frame.cursor.visible && region.contains(frame.cursor.row, frame.cursor.column) {
                    let Some(cursor) = cursor.map(graphene_rect) else {
                        snapshot.pop();
                        continue;
                    };
                    let (foreground, _, _) = colors(
                        window_normal_style(frame, frame.window),
                        &frame.default_style,
                    );
                    if state.cursor_on {
                        if frame.cursor.appearance.is_some() {
                            paint_native_cursor(
                                &surface,
                                snapshot,
                                &state,
                                frame,
                                &cursor,
                                &self.layouts.borrow(),
                            );
                        } else {
                            paint_cursor(snapshot, &cursor, foreground, &frame.mode, state.focused);
                        }
                    }
                    if let Some(preedit) = state.preedit.as_ref() {
                        paint_preedit(&surface, snapshot, &state, preedit, &cursor, &view);
                    }
                }
                snapshot.pop();
            }
            paint_active_split(&surface, snapshot, &state, frame);
            paint_retained_overlays(
                &surface,
                snapshot,
                &state,
                frame,
                &mut self.layouts.borrow_mut(),
                &mut self.overlay_scene.borrow_mut(),
            );
            #[cfg(test)]
            self.painted_revision.set(frame.revision);
        }
    }
}

glib::wrapper! {
    pub struct BufferSurface(ObjectSubclass<imp::BufferSurface>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

fn paint_active_split(
    surface: &BufferSurface,
    snapshot: &gtk::Snapshot,
    state: &PaintState,
    frame: &Vim9RenderFrame,
) {
    if frame.windows.len() < 2 {
        return;
    }
    let Some(window) = frame
        .windows
        .iter()
        .find(|window| window.window == frame.window)
    else {
        return;
    };
    let view = view_rectangle(&window.view, state.metrics);
    let under = window_normal_style(frame, frame.window);
    let (foreground, background, _) = colors(under, &frame.default_style);
    let accent = frame
        .cursor
        .appearance
        .as_ref()
        .map_or(foreground, |cursor| {
            let (_, caret) = cursor.colors(under, &frame.default_style);
            if caret == background {
                foreground
            } else {
                caret
            }
        });
    let mut color = rgb(accent);
    color.set_alpha(if state.focused { 0.60 } else { 0.24 });
    let edge = (1.0 / surface.scale_factor().max(1) as f32)
        .min(view.width() * 0.5)
        .min(view.height() * 0.5);
    // Overlay only the inside edge of the active text viewport. Its position
    // is fixed during pixel scrolling and cannot cover a Vim separator or
    // status row. Completion paints afterward, above this transient layer.
    for rect in [
        gtk::graphene::Rect::new(view.x(), view.y(), view.width(), edge),
        gtk::graphene::Rect::new(
            view.x(),
            view.y() + view.height() - edge,
            view.width(),
            edge,
        ),
        gtk::graphene::Rect::new(view.x(), view.y() + edge, edge, view.height() - edge * 2.0),
        gtk::graphene::Rect::new(
            view.x() + view.width() - edge,
            view.y() + edge,
            edge,
            view.height() - edge * 2.0,
        ),
    ] {
        snapshot.append_color(&color, &rect);
    }
}

/// Elide fills already supplied by the window backdrop. Once a row contains
/// another color, preserve all later fills: a Normal run may erase a guide or
/// selection beneath it. Empty sets allocate nothing for ordinary source text.
struct BackgroundRows {
    normal: u32,
    changed: HashSet<usize>,
}

impl BackgroundRows {
    fn new(normal: u32) -> Self {
        Self {
            normal,
            changed: HashSet::new(),
        }
    }

    fn needs_fill(&mut self, row: usize, background: u32) -> bool {
        if background != self.normal {
            self.changed.insert(row);
            true
        } else {
            self.changed.contains(&row)
        }
    }
}

fn build_text_scene(
    surface: &BufferSurface,
    state: &PaintState,
    frame: &Vim9RenderFrame,
    layouts: &mut LayoutCache,
) -> Option<gtk::gsk::RenderNode> {
    let snapshot = gtk::Snapshot::new();
    let metrics = state.metrics;
    let baseline = row_baseline(surface, state);
    // Clip and cover each Vim editing window independently. A single
    // bounding rectangle would erase live PTY status lines, split
    // separators and plugin-owned chrome between the native panes.
    trim_layout_cache(frame, layouts);
    for (index, region) in frame.paint_views().enumerate() {
        let view = view_rectangle(region, metrics);
        snapshot.push_clip(&view);
        let window = frame
            .windows
            .get(index)
            .map_or(frame.window, |window| window.window);
        let (_, background, _) = colors(window_normal_style(frame, window), &frame.default_style);
        snapshot.append_color(&rgb(background), &view);
        let mut backgrounds = BackgroundRows::new(background);

        // Paint backgrounds before ink so a subsequent syntax run
        // cannot erase italic glyphs from the previous run.
        for run in frame
            .runs
            .iter()
            .filter(|run| region.contains(run.row, run.column))
        {
            let (_, background, _) = colors(&run.style, &frame.default_style);
            if !backgrounds.needs_fill(run.row, background) {
                continue;
            }
            snapshot.append_color(
                &rgb(background),
                &gtk::graphene::Rect::new(
                    metrics.origin_x + run.column as f32 * metrics.cell_width,
                    metrics.origin_y + run.row as f32 * metrics.line_height,
                    run.cells as f32 * metrics.cell_width,
                    metrics.line_height,
                ),
            );
        }

        // Layouts are cached across cursor and viewport updates. Pango
        // shapes a complete same-style run, including font fallback and
        // supported ligatures; GTK retains device-independent text nodes.
        for run in frame
            .runs
            .iter()
            .filter(|run| region.contains(run.row, run.column))
        {
            let x = metrics.origin_x + run.column as f32 * metrics.cell_width;
            let y = metrics.origin_y + run.row as f32 * metrics.line_height;
            let width = run.cells as f32 * metrics.cell_width;
            paint_text_run(
                surface,
                &snapshot,
                state,
                run,
                &frame.default_style,
                Vim9PixelRect {
                    x,
                    y,
                    width,
                    height: metrics.line_height,
                },
                baseline,
                decoration_phase_x(frame, run, metrics),
                &view,
                layouts,
            );
        }

        snapshot.pop();
    }
    snapshot.to_node()
}

fn paint_pixel_scene(
    surface: &BufferSurface,
    snapshot: &gtk::Snapshot,
    state: &PaintState,
    frame: &Vim9RenderFrame,
    layouts: &mut LayoutCache,
) {
    let metrics = state.metrics;
    let baseline = row_baseline(surface, state);
    trim_layout_cache(frame, layouts);
    for window in &frame.windows {
        let offset = surface.presentation_offset(state, frame, window.window);
        let Some(view) = Vim9Viewport::new(frame, window.window, metrics, offset) else {
            continue;
        };
        snapshot.push_clip(&graphene_rect(view.clip()));
        let (_, background, _) = colors(
            window_normal_style(frame, window.window),
            &frame.default_style,
        );
        snapshot.append_color(&rgb(background), &graphene_rect(view.clip()));
        let mut backgrounds = BackgroundRows::new(background);
        for projected in view.runs() {
            let (_, background, _) = colors(&projected.run.style, &frame.default_style);
            // Pixel Y identifies the presented row, including overscan whose
            // source run.row may alias a visible row's coordinate.
            if backgrounds.needs_fill(projected.rect.y.to_bits() as usize, background) {
                snapshot.append_color(&rgb(background), &graphene_rect(projected.clip));
            }
        }
        for projected in view.runs() {
            paint_text_run(
                surface,
                snapshot,
                state,
                projected.run,
                &frame.default_style,
                projected.rect,
                baseline,
                decoration_phase_x(frame, projected.run, metrics),
                &graphene_rect(view.clip()),
                layouts,
            );
        }
        snapshot.pop();
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_text_run(
    surface: &BufferSurface,
    snapshot: &gtk::Snapshot,
    state: &PaintState,
    run: &Vim9RenderRun,
    normal: &Vim9RenderStyle,
    rect: Vim9PixelRect,
    baseline: f32,
    decoration_phase: f32,
    clip: &gtk::graphene::Rect,
    layouts: &mut LayoutCache,
) {
    // Snapshot clips apply on the GPU. Reject invisible ink before Pango
    // shaping or Cairo path construction, while retaining italic overhang.
    let overhang = state.metrics.line_height * 0.25;
    if clip_run_rect(
        Vim9PixelRect {
            x: rect.x - overhang,
            width: rect.width + overhang * 2.0,
            ..rect
        },
        clip,
    )
    .is_none()
    {
        return;
    }
    let x = rect.x;
    let y = rect.y;
    let width = rect.width;
    let (foreground, _, special) = colors(&run.style, normal);
    if !run.text.is_empty() && width > 0.0 {
        // Spaces have no glyph ink; their background and decorations remain.
        if run.text.bytes().any(|byte| byte != b' ') {
            let shaped =
                layouts.get_or_insert_with(&run.text, run.style.bold, run.style.italic, || {
                    let layout = surface.create_pango_layout(Some(&run.text));
                    let mut font = state.font.clone();
                    if run.style.bold {
                        font.set_weight(pango::Weight::Bold);
                    }
                    if run.style.italic {
                        font.set_style(pango::Style::Italic);
                    }
                    layout.set_font_description(Some(&font));
                    layout.set_single_paragraph_mode(true);
                    layout.set_auto_dir(false);
                    let attributes = pango::AttrList::new();
                    attributes.insert(pango::AttrFontFeatures::new(if state.ligatures {
                        "calt=1,liga=1"
                    } else {
                        "calt=0,liga=0"
                    }));
                    layout.set_attributes(Some(&attributes));
                    let (_, logical) = layout.extents();
                    let width = logical.width() as f32 / pango::SCALE as f32;
                    let baseline = layout.baseline() as f32 / pango::SCALE as f32;
                    ShapedLayout {
                        layout,
                        width,
                        baseline,
                    }
                });
            // A fallback CJK font can have different ascent and
            // descent from the Latin font. Centering every run's
            // logical box independently makes their baselines drift.
            let text_y = y + baseline - shaped.baseline;
            snapshot.save();
            snapshot.translate(&gtk::graphene::Point::new(x, text_y));
            // VTE rounds its input grid to whole logical pixels. Match
            // that advance without independently slicing Unicode or
            // breaking a shaped run into terminal-sized glyph boxes.
            if shaped.width > 0.0 {
                snapshot.scale(width / shaped.width, 1.0);
            }
            snapshot.append_layout(&shaped.layout, &rgb(foreground));
            snapshot.restore();
        }
        if let Some(visible) = clip_run_rect(rect, clip) {
            decorations(
                snapshot,
                &run.style,
                special,
                visible,
                decoration_phase + visible.x - rect.x,
            );
        }
    }
}

fn paint_retained_overlays(
    surface: &BufferSurface,
    snapshot: &gtk::Snapshot,
    state: &PaintState,
    frame: &Arc<Vim9RenderFrame>,
    layouts: &mut LayoutCache,
    cached: &mut Option<OverlayScene>,
) {
    if !frame.has_overlays() {
        cached.take();
        return;
    }
    let size = (surface.width(), surface.height());
    let scale = surface.scale_factor();
    if !cached
        .as_ref()
        .is_some_and(|scene| scene.matches(state, frame, scale, size))
    {
        let overlay = gtk::Snapshot::new();
        paint_overlays(surface, &overlay, state, frame, layouts);
        #[cfg(test)]
        surface
            .imp()
            .overlay_builds
            .set(surface.imp().overlay_builds.get() + 1);
        *cached = Some(OverlayScene {
            frame: frame.clone(),
            metrics: state.metrics,
            font_generation: state.font_generation,
            ligatures: state.ligatures,
            scale_factor: scale,
            size,
            node: overlay.to_node(),
        });
    }
    if let Some(scene) = cached {
        if let Some(node) = &scene.node {
            snapshot.append_node(node);
        }
        // Keep repeated focus/blink snapshots on the O(1) Arc identity path.
        scene.frame = frame.clone();
    }
}

/// Popup windows and the completion menu share Vim's screen coordinates, not
/// a buffer's scroll transform. Vim reserves z-index 100 for the menu.
fn paint_overlays(
    surface: &BufferSurface,
    snapshot: &gtk::Snapshot,
    state: &PaintState,
    frame: &Vim9RenderFrame,
    layouts: &mut LayoutCache,
) {
    trim_layout_cache(frame, layouts);
    let mut popups: Vec<_> = frame.popups.iter().collect();
    // At the same z-index, Vim paints the older (lower-id) popup on top.
    popups.sort_by_key(|popup| (popup.zindex, std::cmp::Reverse(popup.id)));
    for popup in popups.iter().filter(|popup| popup.zindex < 100) {
        paint_popup(surface, snapshot, state, frame, popup, layouts);
    }
    paint_completion(surface, snapshot, state, frame, layouts);
    for popup in popups.iter().filter(|popup| popup.zindex >= 100) {
        paint_popup(surface, snapshot, state, frame, popup, layouts);
    }
}

fn paint_popup(
    surface: &BufferSurface,
    snapshot: &gtk::Snapshot,
    state: &PaintState,
    frame: &Vim9RenderFrame,
    popup: &Vim9RenderPopup,
    layouts: &mut LayoutCache,
) {
    let metrics = state.metrics;
    let baseline = row_baseline(surface, state);
    let view = view_rectangle(&popup.view, metrics);
    let card = popup.is_documentation_card();
    let (foreground, background, _) = colors(&popup.default_style, &frame.default_style);
    snapshot.push_clip(&gtk::graphene::Rect::new(
        0.0,
        0.0,
        surface.width() as f32,
        surface.height() as f32,
    ));
    let rounded = gtk::gsk::RoundedRect::from_rect(view, 6.0);
    if card {
        snapshot.append_outset_shadow(
            &rounded,
            &gdk::RGBA::new(0.0, 0.0, 0.0, 0.18),
            0.0,
            2.0,
            0.0,
            6.0,
        );
        snapshot.push_rounded_clip(&rounded);
    } else {
        snapshot.push_clip(&view);
    }
    snapshot.append_color(&rgb(background), &view);
    for run in &popup.runs {
        let (_, run_background, _) = colors(&run.style, &popup.default_style);
        if card && run_background == background {
            continue;
        }
        snapshot.append_color(
            &rgb(run_background),
            &view_rectangle(
                &Vim9RenderRect {
                    row: run.row,
                    column: run.column,
                    rows: 1,
                    columns: run.cells,
                },
                metrics,
            ),
        );
    }
    for run in &popup.runs {
        paint_text_run(
            surface,
            snapshot,
            state,
            run,
            &popup.default_style,
            Vim9PixelRect {
                x: metrics.origin_x + run.column as f32 * metrics.cell_width,
                y: metrics.origin_y + run.row as f32 * metrics.line_height,
                width: run.cells as f32 * metrics.cell_width,
                height: metrics.line_height,
            },
            baseline,
            run.column.saturating_sub(popup.view.column) as f32 * metrics.cell_width,
            &view,
            layouts,
        );
    }
    if card {
        let mut border = rgb(foreground);
        border.set_alpha(0.20);
        let pixel = 1.0 / surface.scale_factor().max(1) as f32;
        snapshot.append_border(&rounded, &[pixel; 4], &[border; 4]);
    }
    snapshot.pop();
    snapshot.pop();
}

fn paint_completion(
    surface: &BufferSurface,
    snapshot: &gtk::Snapshot,
    state: &PaintState,
    frame: &Vim9RenderFrame,
    layouts: &mut LayoutCache,
) {
    let Some(completion) = &frame.completion else {
        return;
    };
    let metrics = state.metrics;
    let baseline = row_baseline(surface, state);
    let view = view_rectangle(&completion.view, metrics);
    // The popup uses Vim's exact grid geometry. It may cross editing panes
    // and status lines, so this layer sits outside every per-window clip.
    snapshot.push_clip(&gtk::graphene::Rect::new(
        0.0,
        0.0,
        surface.width() as f32,
        surface.height() as f32,
    ));
    // Keep elevation outside Vim's rectangle; menu rows and input targets
    // retain their original geometry, including at the widget edges.
    for (extent, alpha) in [
        (4.0, 4.0 / 255.0),
        (3.0, 5.0 / 255.0),
        (2.0, 7.0 / 255.0),
        (1.0, 10.0 / 255.0),
    ] {
        snapshot.append_color(
            &gdk::RGBA::new(0.0, 0.0, 0.0, alpha),
            &gtk::graphene::Rect::new(
                view.x() - extent,
                view.y() - extent,
                view.width() + extent * 2.0,
                view.height() + extent * 2.0,
            ),
        );
    }
    snapshot.push_clip(&view);
    let (foreground, background, _) = colors(&completion.default_style, &frame.default_style);
    snapshot.append_color(&rgb(background), &view);
    for run in &completion.runs {
        let (_, background, _) = colors(&run.style, &completion.default_style);
        snapshot.append_color(
            &rgb(background),
            &gtk::graphene::Rect::new(
                metrics.origin_x + run.column as f32 * metrics.cell_width,
                metrics.origin_y + run.row as f32 * metrics.line_height,
                run.cells as f32 * metrics.cell_width,
                metrics.line_height,
            ),
        );
    }
    for run in &completion.runs {
        paint_text_run(
            surface,
            snapshot,
            state,
            run,
            &completion.default_style,
            Vim9PixelRect {
                x: metrics.origin_x + run.column as f32 * metrics.cell_width,
                y: metrics.origin_y + run.row as f32 * metrics.line_height,
                width: run.cells as f32 * metrics.cell_width,
                height: metrics.line_height,
            },
            baseline,
            run.column.saturating_sub(completion.view.column) as f32 * metrics.cell_width,
            &view,
            layouts,
        );
    }
    let edge = (1.0 / surface.scale_factor().max(1) as f32)
        .min(view.width() * 0.5)
        .min(view.height() * 0.5);
    let mut border = rgb(foreground);
    border.set_alpha(0.20);
    for rect in [
        gtk::graphene::Rect::new(view.x(), view.y(), view.width(), edge),
        gtk::graphene::Rect::new(
            view.x(),
            view.y() + view.height() - edge,
            view.width(),
            edge,
        ),
        gtk::graphene::Rect::new(view.x(), view.y() + edge, edge, view.height() - edge * 2.0),
        gtk::graphene::Rect::new(
            view.x() + view.width() - edge,
            view.y() + edge,
            edge,
            view.height() - edge * 2.0,
        ),
    ] {
        snapshot.append_color(&border, &rect);
    }
    snapshot.pop();
    snapshot.pop();
}

fn graphene_rect(rect: Vim9PixelRect) -> gtk::graphene::Rect {
    gtk::graphene::Rect::new(rect.x, rect.y, rect.width, rect.height)
}

fn view_rectangle(view: &Vim9RenderRect, metrics: Metrics) -> gtk::graphene::Rect {
    metrics
        .rect(view)
        .map(graphene_rect)
        .unwrap_or_else(|| gtk::graphene::Rect::new(0.0, 0.0, 0.0, 0.0))
}

fn rgb(value: u32) -> gdk::RGBA {
    gdk::RGBA::new(
        ((value >> 16) & 255) as f32 / 255.0,
        ((value >> 8) & 255) as f32 / 255.0,
        (value & 255) as f32 / 255.0,
        1.0,
    )
}

fn colors(style: &Vim9RenderStyle, normal: &Vim9RenderStyle) -> (u32, u32, u32) {
    let foreground = style
        .foreground
        .or(normal.foreground)
        .unwrap_or(DEFAULT_FOREGROUND);
    let background = style
        .background
        .or(normal.background)
        .unwrap_or(DEFAULT_BACKGROUND);
    let (foreground, background) = if style.reverse {
        (background, foreground)
    } else {
        (foreground, background)
    };
    (
        foreground,
        background,
        style.special.or(normal.special).unwrap_or(foreground),
    )
}

/// Keep the wave anchored to its own window when a split or widget moves.
/// Syntax-run boundaries do not restart the phase.
fn decoration_phase_x(frame: &Vim9RenderFrame, run: &Vim9RenderRun, metrics: Metrics) -> f32 {
    let origin = frame
        .windows
        .iter()
        .find(|window| window.window == run.window)
        .map_or(frame.view.column, |window| window.view.column);
    run.column.saturating_sub(origin) as f32 * metrics.cell_width
}

fn undercurl_displacement(phase: f32) -> f32 {
    let phase = phase.rem_euclid(4.0);
    -phase.min(4.0 - phase)
}

/// Emit a clipped piece of one continuous, four-logical-pixel wave. `phase`
/// and `x` are separate so moving the window never shifts the wave in its text.
fn undercurl_path(x: f32, y: f32, width: f32, phase: f32, mut point: impl FnMut(f32, f32)) {
    point(x, y + undercurl_displacement(phase));
    let mut distance = 2.0 - phase.rem_euclid(2.0);
    while distance < width {
        point(x + distance, y + undercurl_displacement(phase + distance));
        distance += 2.0;
    }
    point(x + width, y + undercurl_displacement(phase + width));
}

/// Limit horizontal decoration work to visible pixels. Keep the original
/// row height and baseline when only part of a scrolled row is visible.
fn clip_run_rect(rect: Vim9PixelRect, clip: &gtk::graphene::Rect) -> Option<Vim9PixelRect> {
    let left = rect.x.max(clip.x());
    let right = (rect.x + rect.width).min(clip.x() + clip.width());
    (right > left
        && clip.height() > 0.0
        && rect.height > 0.0
        && rect.y < clip.y() + clip.height()
        && rect.y + rect.height > clip.y())
    .then_some(Vim9PixelRect {
        x: left,
        width: right - left,
        ..rect
    })
}

fn decorations(
    snapshot: &gtk::Snapshot,
    style: &Vim9RenderStyle,
    color: u32,
    rect: Vim9PixelRect,
    phase: f32,
) {
    let Vim9PixelRect {
        x,
        y,
        width,
        height,
    } = rect;
    let color = rgb(color);
    if style.underline {
        snapshot.append_color(
            &color,
            &gtk::graphene::Rect::new(x, y + height - 2.0, width, 1.0),
        );
    }
    if style.strikethrough {
        snapshot.append_color(
            &color,
            &gtk::graphene::Rect::new(x, y + height * 0.55, width, 1.0),
        );
    }
    if style.undercurl {
        let rect = gtk::graphene::Rect::new(x, y, width, height);
        let cr = snapshot.append_cairo(&rect);
        cr.set_source_rgb(
            color.red() as f64,
            color.green() as f64,
            color.blue() as f64,
        );
        cr.set_line_width(1.0);
        let mut first = true;
        undercurl_path(x, y + height - 2.0, width, phase, |x, y| {
            if first {
                cr.move_to(f64::from(x), f64::from(y));
                first = false;
            } else {
                cr.line_to(f64::from(x), f64::from(y));
            }
        });
        let _ = cr.stroke();
    }
}

/// Full-width empty line=0 rows carry the window's resolved Normal style.
/// Do not mistake a narrow colorcolumn or a source glyph for that base palette.
fn window_normal_style(frame: &Vim9RenderFrame, window_id: i64) -> &Vim9RenderStyle {
    let view = frame
        .windows
        .iter()
        .find(|window| window.window == window_id)
        .map_or(&frame.view, |window| &window.view);
    frame
        .runs
        .iter()
        .find(|run| {
            (frame.windows.is_empty() || run.window == window_id)
                && run.row == view.row
                && run.column == view.column
                && run.cells == view.columns
                && run.line == 0
                && run.text.is_empty()
                && run.byte_start == 0
                && run.byte_end == 0
        })
        .map_or(&frame.default_style, |run| &run.style)
}

fn same_scroll_window(
    previous_frame: &Vim9RenderFrame,
    next_frame: &Vim9RenderFrame,
    previous: &Vim9RenderWindow,
    next: &Vim9RenderWindow,
) -> bool {
    !render_only_window(next)
        && previous.window == next.window
        && previous.buffer == next.buffer
        && previous.changedtick == next.changedtick
        && previous.view == next.view
        && previous.leftcol == next.leftcol
        && previous.textoff == next.textoff
        && window_normal_style(previous_frame, previous.window)
            == window_normal_style(next_frame, next.window)
}

fn run_under_cursor(frame: &Vim9RenderFrame, run: &Vim9RenderRun) -> bool {
    (frame.windows.is_empty() || run.window == frame.window)
        && run.row == frame.cursor.row
        && run.column <= frame.cursor.column
        && frame.cursor.column < run.column + run.cells
}

/// The final resolved run under the caret also supplies blank/EOL window
/// colors. A per-window line=0 backdrop is paint, even without source bytes.
fn cursor_under_style(frame: &Vim9RenderFrame) -> &Vim9RenderStyle {
    frame
        .runs
        .iter()
        .rfind(|run| run_under_cursor(frame, run))
        .map_or(&frame.default_style, |run| &run.style)
}

fn cursor_paint_cell(
    metrics: Metrics,
    cursor: &Vim9RenderCursor,
    offset: f32,
) -> Option<Vim9PixelRect> {
    if !offset.is_finite() {
        return None;
    }
    let mut cell = metrics.cursor_rect(cursor)?;
    cell.y += offset;
    Some(cell)
}

fn paint_native_cursor(
    surface: &BufferSurface,
    snapshot: &gtk::Snapshot,
    state: &PaintState,
    frame: &Vim9RenderFrame,
    cell: &gtk::graphene::Rect,
    layouts: &LayoutCache,
) {
    let Some(appearance) = &frame.cursor.appearance else {
        return;
    };
    let runs = || frame.runs.iter().filter(|run| run_under_cursor(frame, run));
    let under = cursor_under_style(frame);
    let (fg, bg) = appearance.colors(under, &frame.default_style);
    // `cell` is already clipped by Vim9Viewport. Shape against the complete
    // source row, otherwise a partially hidden block caret moves its glyph
    // baseline to the split edge and rescales horizontal/vertical carets.
    let Some(full_cell) = cursor_paint_cell(
        state.metrics,
        &frame.cursor,
        surface.presentation_offset(state, frame, frame.window),
    ) else {
        return;
    };
    snapshot.push_clip(cell);
    if !state.focused {
        paint_cursor(snapshot, &graphene_rect(full_cell), bg, &frame.mode, false);
        snapshot.pop();
        return;
    }
    let caret = graphene_rect(appearance.paint_rect(
        full_cell,
        state.metrics.cell_width,
        surface.scale_factor() as f32,
    ));
    snapshot.append_color(&rgb(bg), &caret);
    snapshot.push_clip(&caret);
    let baseline = row_baseline(surface, state);
    for run in runs() {
        if let Some(shaped) = layouts.get(&run.text, run.style.bold, run.style.italic) {
            let width = run.cells as f32 * state.metrics.cell_width;
            let x = state.metrics.origin_x + run.column as f32 * state.metrics.cell_width;
            let y = full_cell.y + baseline - shaped.baseline;
            snapshot.save();
            snapshot.translate(&gtk::graphene::Point::new(x, y));
            if shaped.width > 0.0 {
                snapshot.scale(width / shaped.width, 1.0);
            }
            snapshot.append_layout(&shaped.layout, &rgb(fg));
            snapshot.restore();
            if let Some(visible) = clip_run_rect(
                Vim9PixelRect {
                    x,
                    y: full_cell.y,
                    width,
                    height: state.metrics.line_height,
                },
                &caret,
            ) {
                decorations(
                    snapshot,
                    &run.style,
                    fg,
                    visible,
                    decoration_phase_x(frame, run, state.metrics) + visible.x - x,
                );
            }
        }
    }
    snapshot.pop();
    snapshot.pop();
}

fn paint_cursor(
    snapshot: &gtk::Snapshot,
    rect: &gtk::graphene::Rect,
    color: u32,
    mode: &str,
    focused: bool,
) {
    let mut color = rgb(color);
    if !focused {
        for edge in [
            gtk::graphene::Rect::new(rect.x(), rect.y(), rect.width(), 1.0),
            gtk::graphene::Rect::new(rect.x(), rect.y() + rect.height() - 1.0, rect.width(), 1.0),
            gtk::graphene::Rect::new(rect.x(), rect.y(), 1.0, rect.height()),
            gtk::graphene::Rect::new(rect.x() + rect.width() - 1.0, rect.y(), 1.0, rect.height()),
        ] {
            snapshot.append_color(&color, &edge);
        }
    } else if mode.starts_with('i') {
        snapshot.append_color(
            &color,
            &gtk::graphene::Rect::new(rect.x(), rect.y(), 2.0, rect.height()),
        );
    } else if mode.starts_with('R') {
        snapshot.append_color(
            &color,
            &gtk::graphene::Rect::new(rect.x(), rect.y() + rect.height() - 3.0, rect.width(), 3.0),
        );
    } else {
        color.set_alpha(0.38);
        snapshot.append_color(&color, rect);
    }
}

fn row_baseline(surface: &BufferSurface, state: &PaintState) -> f32 {
    let metrics = surface.pango_context().metrics(Some(&state.font), None);
    let ascent = metrics.ascent() as f32 / pango::SCALE as f32;
    let descent = metrics.descent() as f32 / pango::SCALE as f32;
    (state.metrics.line_height - ascent - descent) * 0.5 + ascent
}

fn paint_preedit(
    surface: &BufferSurface,
    snapshot: &gtk::Snapshot,
    state: &PaintState,
    preedit: &Preedit,
    cursor: &gtk::graphene::Rect,
    view: &gtk::graphene::Rect,
) {
    let Some(frame) = state.frame.as_ref() else {
        return;
    };
    let layout = surface.create_pango_layout(Some(&preedit.text));
    layout.set_font_description(Some(&state.font));
    layout.set_attributes(Some(&preedit.attributes));
    layout.set_single_paragraph_mode(true);
    let (width, _) = layout.pixel_size();
    let x = cursor
        .x()
        .min((view.x() + view.width() - width as f32 - 4.0).max(view.x()));
    let y =
        cursor.y() + row_baseline(surface, state) - layout.baseline() as f32 / pango::SCALE as f32;
    let (foreground, background, _) = colors(
        window_normal_style(frame, frame.window),
        &frame.default_style,
    );
    snapshot.append_color(
        &rgb(background),
        &gtk::graphene::Rect::new(x, cursor.y(), width as f32 + 4.0, state.metrics.line_height),
    );
    snapshot.save();
    snapshot.translate(&gtk::graphene::Point::new(x, y));
    snapshot.append_layout(&layout, &rgb(foreground));
    snapshot.restore();
    snapshot.append_color(
        &rgb(foreground),
        &gtk::graphene::Rect::new(
            x,
            cursor.y() + state.metrics.line_height - 1.0,
            width as f32,
            1.0,
        ),
    );
    let byte = preedit
        .text
        .char_indices()
        .nth(preedit.cursor.max(0) as usize)
        .map_or(preedit.text.len(), |(byte, _)| byte);
    let position = layout.index_to_pos(byte.min(i32::MAX as usize) as i32);
    let caret_x = x + position.x() as f32 / pango::SCALE as f32;
    snapshot.append_color(
        &rgb(foreground),
        &gtk::graphene::Rect::new(caret_x, cursor.y(), 1.0, state.metrics.line_height),
    );
}

impl BufferSurface {
    fn new() -> Self {
        let surface: Self = glib::Object::builder().build();
        surface.set_widget_name("vim9-native-buffer");
        surface.set_hexpand(true);
        surface.set_vexpand(true);
        surface.set_can_target(false);
        surface.set_focusable(false);
        surface.pango_context().set_round_glyph_positions(false);
        surface
            .imp()
            .layout_context_serial
            .set(surface.pango_context().serial());
        surface.connect_unmap(|surface| {
            surface.cancel_scroll();
            surface.imp().pointer.set(None);
            surface.imp().presented.borrow_mut().take();
        });
        surface.connect_scale_factor_notify(|surface| {
            // Cached layouts belong to the widget's Pango context. Retire
            // them when it moves between monitors with different DPI.
            surface.cancel_scroll();
            surface.imp().layouts.borrow_mut().clear();
            surface.imp().text_scene.borrow_mut().take();
            surface.imp().overlay_scene.borrow_mut().take();
            surface.queue_draw();
        });
        surface.update_property(&[gtk::accessible::Property::Label(
            "Native Vim buffer rendering",
        )]);
        surface
    }

    fn set_font(&self, font: &pango::FontDescription) {
        self.cancel_scroll();
        let mut state = self.imp().state.borrow_mut();
        state.font = font.clone();
        state.font_generation = state.font_generation.wrapping_add(1);
        // Font metrics and VTE's PTY allocation change asynchronously. Do not
        // paint the old geometry with the new font while a resized frame is
        // in flight; the live Vim surface owns this short transition.
        state.frame = None;
        drop(state);
        self.imp().layouts.borrow_mut().clear();
        self.imp().text_scene.borrow_mut().take();
        self.imp().overlay_scene.borrow_mut().take();
        self.queue_draw();
    }

    fn contains_presented_buffer_point(&self, x: f64, y: f64) -> bool {
        self.imp().presented.borrow().as_ref().is_some_and(|shown| {
            shown.frame.paint_views().any(|view| {
                shown
                    .metrics
                    .rect(view)
                    .is_some_and(|rect| rect.contains(x as f32, y as f32))
            })
        })
    }

    fn restart_cursor_blink(&self) {
        self.imp().state.borrow_mut().restart_cursor_blink();
        self.queue_draw();
    }

    fn set_preedit(&self, preedit: Option<Preedit>) {
        if preedit.is_some() {
            self.cancel_scroll();
        }
        let mut state = self.imp().state.borrow_mut();
        state.cursor_blink.reset();
        state.preedit = preedit;
        drop(state);
        self.queue_draw();
    }

    pub(super) fn is_preediting(&self) -> bool {
        self.imp().state.borrow().preedit.is_some()
    }

    pub(super) fn cancel_scroll(&self) {
        self.imp().pixels.borrow_mut().clear();
        self.imp().source_gesture.borrow_mut().take();
        self.imp().motions.borrow_mut().clear();
        let mut input = self.imp().scroll_input.borrow_mut();
        input.fractions.clear();
        input.pending.clear();
        input.requested.clear();
        self.queue_draw();
    }

    /// Called before replacing the acknowledged frame. It never animates a
    /// guessed viewport or a changed buffer, split layout or font geometry.
    fn retarget_scroll(
        &self,
        next: &Option<Arc<Vim9RenderFrame>>,
        metrics: Metrics,
        now: f64,
        enabled: bool,
    ) {
        let state = self.imp().state.borrow();
        let mut motions = self.imp().motions.borrow_mut();
        if !self.imp().pixels.borrow().is_empty() {
            motions.clear();
            return;
        }
        let was_animating = !motions.is_empty();
        if was_animating {
            self.queue_draw();
        }
        motions.retain(|_, motion| motion.animation.is_active(now));
        let (Some(previous), Some(next)) = (&state.frame, next) else {
            motions.clear();
            return;
        };
        if !enabled
            || previous.has_overlays()
            || next.has_overlays()
            || state.metrics != metrics
            || previous.default_style != next.default_style
            || previous.windows.len() != next.windows.len()
            || next.windows.is_empty()
        {
            motions.clear();
            return;
        }
        // Unchanged frames take an O(1) path even on high-refresh monitors.
        if Arc::ptr_eq(previous, next) {
            return;
        }
        let input = self.imp().scroll_input.borrow();
        let mut scene = self.imp().text_scene.borrow_mut();
        if !scene
            .as_ref()
            .is_some_and(|scene| scene.matches(&state, previous, self.scale_factor()))
        {
            let requested_movement = next.windows.iter().any(|window| {
                input
                    .requested
                    .get(&window.window)
                    .is_some_and(|time| now - time < 0.25)
                    && previous
                        .windows
                        .iter()
                        .any(|old| old.window == window.window && old.topline != window.topline)
            });
            if requested_movement {
                // Precision snapshots paint directly and do not update the
                // retained integer scene. Rebuild it lazily when a real wheel
                // acknowledgement needs that outgoing viewport; otherwise the
                // first wheel after a touchpad gesture would snap instead of
                // animating. Cached Pango layouts remain reusable.
                #[cfg(test)]
                self.imp()
                    .scene_builds
                    .set(self.imp().scene_builds.get() + 1);
                *scene =
                    build_text_scene(self, &state, previous, &mut self.imp().layouts.borrow_mut())
                        .map(|node| TextScene {
                            frame: previous.clone(),
                            metrics,
                            font_generation: state.font_generation,
                            ligatures: state.ligatures,
                            scale_factor: self.scale_factor(),
                            node,
                        });
            }
        }
        let Some(scene) = scene
            .as_ref()
            .filter(|scene| scene.matches(&state, previous, self.scale_factor()))
        else {
            motions.clear();
            return;
        };
        for window in &next.windows {
            let old = previous
                .windows
                .iter()
                .find(|old| old.window == window.window);
            let Some(old) = old.filter(|old| same_scroll_window(previous, next, old, window))
            else {
                motions.remove(&window.window);
                continue;
            };
            if old.topline == window.topline {
                continue;
            }
            let delta =
                (window.topline as f64 - old.topline as f64) * f64::from(metrics.line_height);
            let recent = input
                .requested
                .get(&window.window)
                .is_some_and(|time| now - time < 0.25);
            if !recent || delta.abs() >= window.view.rows as f64 * f64::from(metrics.line_height) {
                motions.remove(&window.window);
                continue;
            }
            let motion = motions.entry(window.window).or_default();
            motion.animation.retarget(delta, now);
            motion.history.push(ScrollScene {
                topline: old.topline,
                node: scene.node.clone(),
            });
            // Paint the newest outgoing viewport that covers the exposed
            // edge. Keep its newer candidates too: a sustained gesture will
            // need them once this source moves beyond the visible span.
            // Older scenes outside the overlap are discarded every frame.
            let offset = motion.animation.offset(now);
            let height = window.view.rows as f64 * f64::from(metrics.line_height);
            let outgoing = motion.history.iter().rposition(|scene| {
                let distance =
                    (window.topline as f64 - scene.topline as f64) * f64::from(metrics.line_height);
                if offset >= 0.0 {
                    distance >= offset && distance <= height
                } else {
                    distance <= offset && distance >= -height
                }
            });
            if let Some(outgoing) = outgoing.filter(|_| offset.abs() < height) {
                motion.history.drain(..outgoing);
            } else {
                motions.remove(&window.window);
            }
        }
        if was_animating || !motions.is_empty() {
            self.queue_draw();
        }
    }

    pub(super) fn enqueue_scroll(&self, x: f64, y: f64, dy: f64, unit: gdk::ScrollUnit) -> bool {
        self.enqueue_scroll_input(x, y, dy, unit, false)
    }

    fn enqueue_scroll_input(
        &self,
        x: f64,
        y: f64,
        dy: f64,
        unit: gdk::ScrollUnit,
        smooth_event: bool,
    ) -> bool {
        if self.overlays_visible() {
            self.cancel_scroll();
            return false;
        }
        if self.stale_scroll_source_at(x, y) {
            // The event belongs to the image still on screen. Do not send it
            // to a replacement buffer before GTK has presented that buffer.
            self.cancel_scroll();
            return true;
        }
        let state = self.imp().state.borrow();
        let Some(frame) = &state.frame else {
            return false;
        };
        let Some((row, column)) = state.metrics.cell_at(x as f32, y as f32) else {
            return false;
        };
        let Some(window) = frame
            .windows
            .iter()
            .find(|window| window.view.contains(row, column))
        else {
            return false;
        };
        if render_only_window(window) {
            // Preserve the real Vim mouse protocol and mappings in help,
            // quickfix and other explicitly render-only windows.
            self.imp()
                .scroll_input
                .borrow_mut()
                .retire_window(window.window);
            // A controller can retire before GTK submits the integer image.
            // Consume this transition event rather than route coordinates
            // from a still-visible fractional image into the PTY grid.
            return self.precision_image_at(x, y);
        }
        if !dy.is_finite() || state.preedit.is_some() {
            return false;
        }
        let lines = match unit {
            gdk::ScrollUnit::Surface => dy / f64::from(state.metrics.line_height),
            _ => dy * 3.0,
        };
        // X11 touchpad valuators use smooth wheel units. Preserve their
        // supplied fractional distance and momentum just like surface pixels.
        let precision = smooth_event || unit == gdk::ScrollUnit::Surface || dy.fract() != 0.0;
        let mut input = self.imp().scroll_input.borrow_mut();
        if self.imp().source_gesture.borrow().is_some() {
            return true;
        }
        if self.imp().pixel_enabled.get()
            && matches!(frame.mode.as_str(), "n" | "i")
            && window.native_scroll
            && !input.pending.contains_key(&window.window)
        {
            let mut pixels = self.imp().pixels.borrow_mut();
            if let Some(scroll) = pixels.get_mut(&window.window) {
                if precision || !self.imp().wheel_animation.get() {
                    scroll.push(lines);
                } else {
                    scroll.push_wheel(lines);
                }
            } else if let Some(mut scroll) = Vim9PixelScroll::new(window, state.metrics) {
                if precision || !self.imp().wheel_animation.get() {
                    scroll.push(lines);
                } else {
                    scroll.push_wheel(lines);
                }
                pixels.insert(window.window, scroll);
            } else {
                return false;
            }
            input.fractions.discard(window.window);
            self.imp().motions.borrow_mut().clear();
            self.queue_draw();
            return true;
        }
        // Unsupported native pointer/input semantics keep Vim's compatibility
        // route and its acknowledged-row presentation.
        self.imp().pixels.borrow_mut().clear();
        let count = input.fractions.add(window.window, lines);
        if count != 0 {
            let pending = input.pending.entry(window.window).or_default();
            *pending = (*pending + count).clamp(-120, 120);
        }
        true
    }

    fn presentation_offset(&self, state: &PaintState, frame: &Vim9RenderFrame, window: i64) -> f32 {
        if let Some(scroll) = self.imp().pixels.borrow().get(&window) {
            return scroll.offset(frame, state.metrics);
        }
        self.imp()
            .motions
            .borrow()
            .get(&window)
            .map_or(0.0, |motion| motion.animation.offset(state.time) as f32)
    }

    fn projected_cursor(&self) -> Option<Vim9PixelRect> {
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

    /// The IM candidate rectangle must follow the caret already submitted to
    /// GTK, even when the polling loop has received a newer cursor/scroll frame.
    /// Geometry and buffer switches retire it until a replacement is painted.
    fn presented_cursor(&self) -> Option<Vim9PixelRect> {
        let state = self.imp().state.borrow();
        let current = state.frame.as_ref()?;
        let presented = self.imp().presented.borrow();
        let presented = presented.as_ref()?;
        if state.metrics != presented.metrics
            || current.window != presented.frame.window
            || current.buffer != presented.frame.buffer
        {
            return None;
        }
        presented.cursor
    }

    fn overlays_visible(&self) -> bool {
        self.imp()
            .state
            .borrow()
            .frame
            .as_ref()
            .is_some_and(|frame| frame.has_overlays())
            || self
                .imp()
                .presented
                .borrow()
                .as_ref()
                .is_some_and(|shown| shown.frame.has_overlays())
    }

    /// Hit the last submitted native image, not a newer frame received between
    /// presentation ticks. Source generation is checked again before dispatch.
    fn source_at(&self, x: f64, y: f64) -> Option<Vim9BufferPosition> {
        if self.overlays_visible() {
            return None;
        }
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let state = self.imp().state.borrow();
        if state.preedit.is_some() {
            return None;
        }
        let current = state.frame.as_ref()?;
        if !matches!(current.mode.as_str(), "n" | "v") {
            return None;
        }
        let presented = self.imp().presented.borrow();
        let presented = presented.as_ref()?;
        if presented.metrics != state.metrics {
            return None;
        }
        for window in &presented.frame.windows {
            let live = current.windows.iter().find(|w| w.window == window.window)?;
            if !window.native_pointer
                || !live.native_pointer
                || live.buffer != window.buffer
                || live.changedtick != window.changedtick
                || live.view != window.view
                || live.leftcol != window.leftcol
                || live.textoff != window.textoff
            {
                continue;
            }
            let offset = presented
                .offsets
                .get(&window.window)
                .copied()
                .unwrap_or(0.0);
            if let Some(view) =
                Vim9Viewport::new(&presented.frame, window.window, presented.metrics, offset)
            {
                if let Some(anchor) = view.anchor_at(x as f32, y as f32) {
                    return Some(anchor.position);
                }
            }
        }
        None
    }

    fn pointer_command(
        position: Vim9BufferPosition,
        gesture: u64,
        phase: Vim9PointerPhase,
    ) -> Vim9Command {
        Vim9Command::Pointer {
            window: position.window,
            buffer: position.buffer,
            changedtick: position.changedtick,
            line: position.line,
            byte_column: position.byte_column,
            gesture,
            phase,
        }
    }

    fn precision_image_at(&self, x: f64, y: f64) -> bool {
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        self.imp().presented.borrow().as_ref().is_some_and(|shown| {
            shown.frame.windows.iter().any(|window| {
                shown.precision_windows.contains(&window.window)
                    && shown
                        .offsets
                        .get(&window.window)
                        .is_some_and(|offset| *offset != 0.0)
                    && shown
                        .metrics
                        .rect(&window.view)
                        .is_some_and(|rect| rect.contains(x as f32, y as f32))
            })
        })
    }

    fn stale_precision_at(&self, x: f64, y: f64) -> bool {
        self.precision_image_at(x, y) && self.source_at(x, y).is_none()
    }

    fn should_settle_press(&self, x: f64, y: f64, button: u32, count: i32, modified: bool) -> bool {
        self.stale_precision_at(x, y)
            || (self.precision_image_at(x, y) && (button != 1 || count != 1 || modified))
    }

    fn stale_scroll_source_at(&self, x: f64, y: f64) -> bool {
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        let presented = self.imp().presented.borrow();
        let Some(shown) = presented.as_ref() else {
            return false;
        };
        let Some(window) = shown.frame.windows.iter().find(|window| {
            shown
                .metrics
                .rect(&window.view)
                .is_some_and(|rect| rect.contains(x as f32, y as f32))
        }) else {
            return false;
        };
        let state = self.imp().state.borrow();
        state.metrics != shown.metrics
            || state.frame.as_ref().is_none_or(|current| {
                current
                    .windows
                    .iter()
                    .find(|live| live.window == window.window)
                    .is_none_or(|live| {
                        live.buffer != window.buffer
                            || live.changedtick != window.changedtick
                            || live.view != window.view
                            || live.leftcol != window.leftcol
                            || live.textoff != window.textoff
                    })
            })
    }

    fn direct_source_at(&self, x: f64, y: f64) -> Option<Vim9BufferPosition> {
        let position = self.source_at(x, y)?;
        // Do not replace stationary Vim mouse behavior. Source transactions
        // are necessary only while precision presentation differs from its
        // integer grid; plain clicks/multi-clicks otherwise remain Vim-owned.
        let presented = self.imp().presented.borrow();
        let presented = presented.as_ref()?;
        // Cancellation/acknowledgement may retire the live pixel controller
        // before GTK paints its replacement. Ownership follows the submitted
        // image until that image is replaced, just like its hit coordinates.
        if !presented.precision_windows.contains(&position.window) {
            return None;
        }
        let offset = presented.offsets.get(&position.window).copied()?;
        (offset != 0.0).then_some(position)
    }

    fn press_source(&self, session: &Vim9Session, x: f64, y: f64) -> bool {
        let Some(position) = self.direct_source_at(x, y) else {
            return false;
        };
        let gesture = self.imp().gesture_sequence.get() % i32::MAX as u64 + 1;
        let Ok(request) = session.send(Self::pointer_command(
            position,
            gesture,
            Vim9PointerPhase::Press,
        )) else {
            // This press targets a visible fractional image. Failed transport
            // must settle it, not fall through to different terminal cells.
            self.cancel_scroll();
            return true;
        };
        self.imp().gesture_sequence.set(gesture);
        for scroll in self.imp().pixels.borrow_mut().values_mut() {
            scroll.stop_animation();
        }
        *self.imp().source_gesture.borrow_mut() = Some(SourceGesture {
            gesture,
            request,
            last: position,
            pending: None,
            released: false,
        });
        // Drop unsent legacy wheel impulses; an already accepted source-bound
        // scroll stays ordered ahead of the click on the same bridge channel.
        let mut input = self.imp().scroll_input.borrow_mut();
        input.pending.clear();
        input.fractions.clear();
        input.requested.clear();
        self.imp().motions.borrow_mut().clear();
        true
    }

    fn queue_source_pointer(&self, x: f64, y: f64, phase: Vim9PointerPhase) -> bool {
        if self.overlays_visible() {
            self.cancel_scroll();
            return false;
        }
        let target = self.source_at(x, y);
        let mut gesture = self.imp().source_gesture.borrow_mut();
        let Some(gesture) = gesture.as_mut() else {
            return false;
        };
        if gesture.released {
            return true;
        }
        let position = target.filter(|p| {
            p.window == gesture.last.window
                && p.buffer == gesture.last.buffer
                && p.changedtick == gesture.last.changedtick
        });
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

    fn source_motion(&self, x: f64, y: f64, primary_down: bool) {
        if self.overlays_visible() {
            self.cancel_scroll();
            return;
        }
        if primary_down {
            self.queue_source_pointer(x, y, Vim9PointerPhase::Drag);
        } else if let Some(gesture) = self.imp().source_gesture.borrow_mut().as_mut() {
            // Some compositors/widget grabs lose the release event. A later
            // hover must finish at the last admitted target, never extend the
            // selection to the unpressed pointer's new location.
            if !gesture.released {
                gesture.pending = Some((gesture.last, Vim9PointerPhase::Release));
                gesture.released = true;
            }
        }
    }

    fn advance_native_input(
        &self,
        session: &Vim9Session,
        input: &Vim9InputSnapshot,
        next: &Option<Arc<Vim9RenderFrame>>,
        metrics: Metrics,
        enabled: bool,
    ) {
        self.imp().pixel_enabled.set(enabled);
        let Some(frame) = next.as_ref() else {
            self.imp().pixels.borrow_mut().clear();
            self.imp().source_gesture.borrow_mut().take();
            // Keep last-image ownership until GTK actually clears/replaces
            // the surface; an intervening click must not hit a different grid.
            return;
        };
        if frame.has_overlays() {
            // Keep popup and completion navigation on Vim's original input
            // path, with no queued source-bound wheel or pointer transaction.
            self.imp().pixel_enabled.set(false);
            if self
                .imp()
                .state
                .borrow()
                .frame
                .as_ref()
                .is_none_or(|old| !old.has_overlays())
            {
                self.cancel_scroll();
            }
            return;
        }
        self.imp()
            .scroll_input
            .borrow_mut()
            .retire_render_only(frame);
        {
            let mut motions = self.imp().motions.borrow_mut();
            let before = motions.len();
            motions.retain(|window, _| legacy_scroll_target(frame, *window));
            if motions.len() != before {
                self.queue_draw();
            }
        }
        let changed_geometry = {
            let state = self.imp().state.borrow();
            state.metrics != metrics
                || state.frame.as_ref().is_some_and(|old| {
                    old.rows != frame.rows
                        || old.columns != frame.columns
                        || old.windows.len() != frame.windows.len()
                        || old
                            .windows
                            .iter()
                            .zip(&frame.windows)
                            .any(|(a, b)| a.window != b.window || a.view != b.view)
                })
        };
        let mut pointer = self.imp().source_gesture.borrow_mut();
        if !enabled
            || changed_geometry
            || !matches!(frame.mode.as_str(), "n" | "v")
            || pointer.as_ref().is_some_and(|gesture| {
                !frame.windows.iter().any(|w| {
                    w.window == gesture.last.window
                        && w.buffer == gesture.last.buffer
                        && w.changedtick == gesture.last.changedtick
                        && w.native_pointer
                })
            })
        {
            *pointer = None;
        }
        let mut pixels = self.imp().pixels.borrow_mut();
        let before = pixels.len();
        let was_animating = pixels.values().any(Vim9PixelScroll::is_animating);
        // Releasing a source-bound drag ends its transaction, but Visual
        // selection still refers to the same frozen fractional image. Keep
        // that image until actual input or an incompatible frame retires it.
        if !enabled || changed_geometry || !matches!(frame.mode.as_str(), "n" | "i" | "v") {
            pixels.clear();
        } else {
            pixels.retain(|window, scroll| {
                frame
                    .windows
                    .iter()
                    .find(|w| w.window == *window)
                    .is_some_and(|w| {
                        let pending = scroll
                            .request_id()
                            .is_some_and(|id| input.command_pending(id));
                        scroll.reconcile(w, metrics, pending)
                    })
            });
        }
        if pixels.len() != before || was_animating {
            self.queue_draw();
        }
        if pointer.is_none() && matches!(frame.mode.as_str(), "n" | "i") {
            for scroll in pixels.values_mut() {
                if let Some(command) = scroll.command() {
                    if let Ok(id) = session.send(command) {
                        scroll.submitted(id);
                    }
                }
            }
        }
        if let Some(gesture) = pointer.as_mut() {
            if !input.command_pending(gesture.request) {
                if let Some((position, phase)) = gesture.pending.take() {
                    match session.send(Self::pointer_command(position, gesture.gesture, phase)) {
                        Ok(id) => {
                            gesture.request = id;
                            gesture.last = position;
                        }
                        // Fail closed on transport saturation/disconnect. Do
                        // not replay an old drag after unrelated keyboard input.
                        Err(_) => {
                            *pointer = None;
                        }
                    }
                } else if gesture.released {
                    *pointer = None;
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn scroll_offset(&self, window: i64) -> f64 {
        let state = self.imp().state.borrow();
        state.frame.as_ref().map_or(0.0, |frame| {
            f64::from(self.presentation_offset(&state, frame, window))
        })
    }

    #[cfg(test)]
    pub(super) fn native_font_size(&self) -> i32 {
        self.imp().state.borrow().font.size()
    }

    #[cfg(test)]
    pub(super) fn scene_build_count(&self) -> usize {
        self.imp().scene_builds.get()
    }

    #[cfg(test)]
    pub(super) fn painted_frame(&self) -> Option<Arc<Vim9RenderFrame>> {
        let frame = self.imp().state.borrow().frame.clone()?;
        (self.imp().painted_revision.get() == frame.revision).then_some(frame)
    }

    #[cfg(test)]
    pub(super) fn painted_geometry(&self) -> Option<(Metrics, Vim9PixelRect)> {
        let presented = self.imp().presented.borrow();
        let presented = presented.as_ref()?;
        Some((presented.metrics, presented.cursor?))
    }
}

fn owns_scroll_zoom(modifiers: gdk::ModifierType, preediting: bool) -> bool {
    !preediting
        && modifiers.contains(gdk::ModifierType::CONTROL_MASK)
        && !modifiers.intersects(gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK)
}

fn scroll_zoom_step(delta: f64, unit: gdk::ScrollUnit) -> Option<f64> {
    if !delta.is_finite() || delta == 0.0 {
        return None;
    }
    // GDK defines wheel units as detents and surface units as logical pixels.
    // 40 pixels per point is our zoom sensitivity, not a device conversion.
    Some(match unit {
        gdk::ScrollUnit::Surface => -delta / 40.0,
        _ => -delta,
    })
}

pub(super) fn font_zoom_key(key: gdk::Key, modifiers: gdk::ModifierType) -> Option<Option<f64>> {
    let character = match key {
        gdk::Key::KP_Add => Some('+'),
        gdk::Key::KP_Subtract => Some('-'),
        gdk::Key::KP_0 => Some('0'),
        _ => key.to_unicode(),
    };
    crate::vim9_zoom::key_step(
        character,
        modifiers.contains(gdk::ModifierType::CONTROL_MASK),
        modifiers.intersects(gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK),
    )
}

pub(super) fn contains_buffer_pointer(root: &gtk::Widget) -> bool {
    if let Some(surface) = root.downcast_ref::<BufferSurface>() {
        return surface.is_mapped() && surface.imp().pointer.get().is_some();
    }
    let mut child = root.first_child();
    while let Some(widget) = child {
        if contains_buffer_pointer(&widget) {
            return true;
        }
        child = widget.next_sibling();
    }
    false
}

fn zoom_font(font_button: &gtk::FontDialogButton, step: Option<f64>) {
    let Some(mut font) = font_button.font_desc() else {
        return;
    };
    let current = f64::from(font.size()) / f64::from(pango::SCALE);
    let size = step.map_or(12.0, |step| (current + step).clamp(6.0, 72.0));
    if font.is_size_absolute() {
        font.set_absolute_size(size * f64::from(pango::SCALE));
    } else {
        font.set_size((size * f64::from(pango::SCALE)).round() as i32);
    }
    font_button.set_font_desc(&font);
}

pub(super) struct NativeBuffer {
    pub widget: gtk::Overlay,
    pub menu: gtk::MenuButton,
    pub badge: gtk::Label,
    pub surface: BufferSurface,
}

impl NativeBuffer {
    pub(super) fn new(
        session: &Vim9Session,
        terminal: &vte4::Terminal,
        native_enabled: bool,
        smooth_enabled: bool,
    ) -> Self {
        let widget = gtk::Overlay::new();
        widget.set_child(Some(terminal));
        let surface = BufferSurface::new();
        widget.add_overlay(&surface);
        widget.set_measure_overlay(&surface, false);
        let badge = gtk::Label::new(Some("GUI · connecting"));
        badge.add_css_class("caption");
        badge.set_ellipsize(pango::EllipsizeMode::End);
        badge.set_max_width_chars(28);
        badge.set_single_line_mode(true);
        badge.set_tooltip_text(Some(
            "The editor buffer is drawn from Vim GUI highlight colors.",
        ));

        let menu = gtk::MenuButton::new();
        menu.set_icon_name("preferences-desktop-font-symbolic");
        menu.set_tooltip_text(Some("Editor rendering, font and line spacing"));
        let popover = gtk::Popover::new();
        let settings = gtk::Box::new(gtk::Orientation::Vertical, 10);
        settings.set_margin_start(14);
        settings.set_margin_end(14);
        settings.set_margin_top(14);
        settings.set_margin_bottom(14);
        let render_status = gtk::Label::new(Some("GUI · connecting"));
        render_status.set_xalign(0.0);
        render_status.set_wrap(true);
        render_status.add_css_class("caption");
        settings.append(&render_status);
        let enabled = gtk::CheckButton::with_label("Native GUI buffer");
        enabled.set_active(native_enabled);
        enabled.set_tooltip_text(Some("Use Vim's GUI RGB highlights and Pango text rendering. Unsupported views use Vim's compatibility display."));
        settings.append(&enabled);
        let smooth = gtk::CheckButton::with_label("Smooth scrolling");
        smooth.set_active(smooth_enabled);
        smooth.set_tooltip_text(Some(
            "Animate discrete wheel movement. Precision devices always follow their supplied pixels; wheel animation obeys reduced-motion settings.",
        ));
        settings.append(&smooth);
        {
            let surface = surface.clone();
            smooth.connect_toggled(move |_| surface.cancel_scroll());
        }
        let font_dialog = gtk::FontDialog::new();
        font_dialog.set_title("Editor font");
        font_dialog.set_filter(Some(&gtk::CustomFilter::new(|object| {
            object.downcast_ref::<pango::FontFamily>().map_or_else(
                || {
                    object
                        .downcast_ref::<pango::FontFace>()
                        .is_some_and(|face| face.family().is_monospace())
                },
                |family| family.is_monospace(),
            )
        })));
        let font = gtk::FontDialogButton::new(Some(font_dialog));
        font.set_font_desc(&pango::FontDescription::from_string("Monospace 12"));
        font.set_tooltip_text(Some(
            "Editor font; workspace and sidebar fonts keep their own settings",
        ));
        settings.append(&font);
        let ligatures = gtk::CheckButton::with_label("Font ligatures");
        ligatures.set_active(true);
        settings.append(&ligatures);
        let spacing_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        spacing_row.append(&gtk::Label::new(Some("Line spacing")));
        let spacing = gtk::SpinButton::with_range(1.0, 1.8, 0.05);
        spacing.set_digits(2);
        spacing.set_value(1.15);
        spacing.set_tooltip_text(Some("Line height multiplier for this editor"));
        terminal.set_cell_height_scale(1.15);
        spacing_row.append(&spacing);
        settings.append(&spacing_row);
        popover.set_child(Some(&settings));
        menu.set_popover(Some(&popover));

        // A single native IM context owns composition while the native buffer
        // is visible. Commits still enter the real Vim PTY; ordinary special
        // keys fall through to VTE. Otherwise VTE would draw preedit beneath
        // the opaque native buffer and hide Chinese/Japanese composition.
        let im = gtk::IMMulticontext::new();
        im.set_client_widget(Some(terminal));
        im.set_use_preedit(true);
        let im_keys = gtk::EventControllerKey::new();
        im_keys.set_name(Some("vim9-native-input-method"));
        im_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        terminal.add_controller(im_keys.clone());
        {
            let weak_terminal = terminal.downgrade();
            let weak_surface = surface.downgrade();
            im.connect_commit(move |_, text| {
                if let Some(terminal) = weak_terminal.upgrade() {
                    terminal.feed_child(text.as_bytes());
                }
                if let Some(surface) = weak_surface.upgrade() {
                    surface.set_preedit(None);
                }
            });
        }
        {
            let weak_surface = surface.downgrade();
            im.connect_preedit_changed(move |im| {
                if let Some(surface) = weak_surface.upgrade() {
                    let (text, attributes, cursor) = im.preedit_string();
                    surface.set_preedit((!text.is_empty()).then(|| Preedit {
                        text: text.into(),
                        attributes,
                        cursor,
                    }));
                }
            });
        }
        {
            let weak_surface = surface.downgrade();
            im.connect_preedit_end(move |_| {
                if let Some(surface) = weak_surface.upgrade() {
                    surface.set_preedit(None);
                }
            });
        }
        {
            let terminal = terminal.clone();
            let surface = surface.clone();
            font.connect_font_desc_notify(move |font| {
                if let Some(font) = font.font_desc() {
                    terminal.set_font(Some(&font));
                    surface.set_font(&font);
                }
            });
        }
        {
            let terminal = terminal.clone();
            spacing.connect_value_changed(move |spacing| {
                terminal.set_cell_height_scale(spacing.value())
            });
        }
        {
            let surface = surface.clone();
            ligatures.connect_toggled(move |button| {
                surface.imp().state.borrow_mut().ligatures = button.is_active();
                surface.imp().layouts.borrow_mut().clear();
                surface.queue_draw();
            });
        }

        let motion = gtk::EventControllerMotion::new();
        motion.set_name(Some("vim9-buffer-pointer"));
        motion.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let surface = surface.downgrade();
            motion.connect_enter(move |_, x, y| {
                if let Some(surface) = surface.upgrade() {
                    // A stationary pointer can enter a newly mapped editor
                    // without a motion event before its first wheel gesture.
                    surface.imp().pointer.set(Some((x, y)));
                }
            });
        }
        {
            let surface = surface.downgrade();
            motion.connect_motion(move |controller, x, y| {
                if let Some(surface) = surface.upgrade() {
                    surface.imp().pointer.set(Some((x, y)));
                    surface.source_motion(
                        x,
                        y,
                        controller
                            .current_event_state()
                            .contains(gdk::ModifierType::BUTTON1_MASK),
                    );
                }
            });
        }
        {
            let surface = surface.downgrade();
            motion.connect_leave(move |_| {
                if let Some(surface) = surface.upgrade() {
                    surface.imp().pointer.set(None);
                }
            });
        }
        terminal.add_controller(motion);
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        scroll.set_name(Some("vim9-buffer-smooth-scroll"));
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let surface = surface.downgrade();
            scroll.connect_scroll(move |controller, dx, dy| {
                let Some(surface) = surface.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                if surface.overlays_visible() {
                    surface.cancel_scroll();
                    return glib::Propagation::Proceed;
                }
                let Some((x, y)) = surface.imp().pointer.get() else {
                    return glib::Propagation::Proceed;
                };
                if surface.stale_scroll_source_at(x, y) {
                    surface.cancel_scroll();
                    return glib::Propagation::Stop;
                }
                let modifiers = controller.current_event_state();
                if owns_scroll_zoom(modifiers, surface.is_preediting()) {
                    // The zoom controller owns this gesture and invalidates
                    // old viewport geometry through the font-change handler.
                    return glib::Propagation::Proceed;
                }
                if modifiers.intersects(
                    gdk::ModifierType::CONTROL_MASK
                        | gdk::ModifierType::ALT_MASK
                        | gdk::ModifierType::SUPER_MASK
                        | gdk::ModifierType::SHIFT_MASK,
                ) || dy == 0.0
                    || dx.abs() > dy.abs()
                {
                    if surface.precision_image_at(x, y) {
                        surface.cancel_scroll();
                        return glib::Propagation::Stop;
                    }
                    return glib::Propagation::Proceed;
                }
                let smooth_event = controller.current_event().is_some_and(|event| {
                    event
                        .downcast_ref::<gdk::ScrollEvent>()
                        .is_some_and(|event| event.direction() == gdk::ScrollDirection::Smooth)
                });
                let accepted = if smooth_event {
                    surface.enqueue_scroll_input(x, y, dy, controller.unit(), true)
                } else {
                    surface.enqueue_scroll(x, y, dy, controller.unit())
                };
                if accepted {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
        }
        terminal.add_controller(scroll);
        // Keyboard/preedit cancels unsent native gestures before PTY input.
        let editing = gtk::EventControllerKey::new();
        editing.set_name(Some("vim9-buffer-scroll-stop"));
        editing.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let surface = surface.downgrade();
            editing.connect_key_pressed(move |_, _, _, _| {
                if let Some(surface) = surface.upgrade() {
                    surface.cancel_scroll();
                    surface.imp().state.borrow_mut().cursor_blink.reset();
                }
                glib::Propagation::Proceed
            });
        }
        terminal.add_controller(editing);
        let clicking = gtk::GestureClick::new();
        clicking.set_name(Some("vim9-buffer-click"));
        clicking.set_button(0);
        clicking.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let surface = surface.downgrade();
            let terminal = terminal.downgrade();
            let session = session.clone();
            clicking.connect_pressed(move |gesture, count, x, y| {
                let Some(surface) = surface.upgrade() else {
                    return;
                };
                if surface.contains_presented_buffer_point(x, y) {
                    if let Some(terminal) = terminal.upgrade() {
                        terminal.grab_focus();
                    }
                    // A same-cell click has no cursor-frame change to restart
                    // the clock. Show it now and retain Vim's next wait period.
                    surface.restart_cursor_blink();
                }
                if surface.overlays_visible() {
                    surface.cancel_scroll();
                    return;
                }
                let modified = gesture.current_event_state().intersects(
                    gdk::ModifierType::CONTROL_MASK
                        | gdk::ModifierType::SHIFT_MASK
                        | gdk::ModifierType::ALT_MASK
                        | gdk::ModifierType::SUPER_MASK,
                );
                if surface.should_settle_press(x, y, gesture.current_button(), count, modified) {
                    // Consume unsupported clicks and stale images until the
                    // fractional view settles. Their integer terminal cells
                    // would not address the source currently on screen.
                    surface.cancel_scroll();
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    return;
                }
                if gesture.current_button() == 1
                    && count == 1
                    && !modified
                    && surface.press_source(&session, x, y)
                {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                } else {
                    surface.cancel_scroll();
                }
            });
        }
        {
            let surface = surface.downgrade();
            clicking.connect_released(move |gesture, _, x, y| {
                if gesture.current_button() != 1 {
                    return;
                }
                if let Some(surface) = surface.upgrade() {
                    if surface.queue_source_pointer(x, y, Vim9PointerPhase::Release) {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                    }
                }
            });
        }
        {
            let surface = surface.downgrade();
            clicking.connect_unpaired_release(move |_, x, y, button, _| {
                if button == 1 {
                    if let Some(surface) = surface.upgrade() {
                        surface.queue_source_pointer(x, y, Vim9PointerPhase::Release);
                    }
                }
            });
        }
        {
            let surface = surface.downgrade();
            clicking.connect_cancel(move |_, _| {
                if let Some(surface) = surface.upgrade() {
                    surface.cancel_scroll();
                }
            });
        }
        terminal.add_controller(clicking);

        // Zoom belongs to the buffer even when the workbench chrome is off.
        // Updating the font control uses the same frame invalidation and real
        // Vim-grid resize path as the native font chooser.
        let zoom_keys = gtk::EventControllerKey::new();
        zoom_keys.set_name(Some("vim9-buffer-zoom"));
        zoom_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let font = font.downgrade();
            let surface = surface.downgrade();
            zoom_keys.connect_key_pressed(move |_, key, _, modifiers| {
                if surface
                    .upgrade()
                    .is_some_and(|surface| surface.is_preediting())
                {
                    return glib::Propagation::Proceed;
                }
                let step = font_zoom_key(key, modifiers);
                match (font.upgrade(), step) {
                    (Some(font), Some(step)) => {
                        zoom_font(&font, step);
                        glib::Propagation::Stop
                    }
                    _ => glib::Propagation::Proceed,
                }
            });
        }
        terminal.add_controller(zoom_keys);
        let zoom_scroll =
            gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        zoom_scroll.set_name(Some("vim9-buffer-zoom-scroll"));
        zoom_scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let font = font.downgrade();
            let surface = surface.downgrade();
            zoom_scroll.connect_scroll(move |controller, _, dy| {
                if !owns_scroll_zoom(
                    controller.current_event_state(),
                    surface
                        .upgrade()
                        .is_some_and(|surface| surface.is_preediting()),
                ) {
                    return glib::Propagation::Proceed;
                }
                let Some(step) = scroll_zoom_step(dy, controller.unit()) else {
                    return glib::Propagation::Proceed;
                };
                let Some(font) = font.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                zoom_font(&font, Some(step));
                glib::Propagation::Stop
            });
        }
        terminal.add_controller(zoom_scroll);

        let im_active = Rc::new(Cell::new(false));
        {
            let im = im.clone();
            let im_active = im_active.clone();
            let surface = surface.clone();
            terminal.connect_has_focus_notify(move |terminal| {
                if im_active.get() {
                    if terminal.has_focus() {
                        im.focus_in();
                    } else {
                        im.focus_out();
                    }
                }
                if !terminal.has_focus() {
                    surface.cancel_scroll();
                }
                surface.imp().state.borrow_mut().focused = terminal.has_focus();
                surface.queue_draw();
            });
        }
        let session = session.clone();
        let weak_terminal = terminal.downgrade();
        let poll_surface = surface.clone();
        let poll_badge = badge.clone();
        let poll = RefCell::new((None, 0, 0, String::new()));
        // Follow the compositor's frame clock (including high refresh displays)
        // rather than a fixed 16 ms polling timer. Hidden widgets do no work.
        widget.add_tick_callback(move |_, clock| {
            let mut poll = poll.borrow_mut();
            let (subscribed, font_generation, render_revision_floor, old_status) = &mut *poll;
            let now = clock.frame_time() as f64 / 1_000_000.0;
            let Some(terminal) = weak_terminal.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let wanted = enabled.is_active();
            let input = session.input_snapshot();
            let frame = input.frame.clone();
            let generation = poll_surface.imp().state.borrow().font_generation;
            if generation != *font_generation {
                *font_generation = generation;
                *render_revision_floor = frame.as_ref().map_or(0, |frame| frame.revision);
                // A font-family/style change can retain the same PTY geometry.
                // Re-subscribing explicitly requests a fresh keyframe in that
                // case, instead of waiting forever for a Vim resize event.
                *subscribed = None;
            }
            // send() is bounded and refuses unauthenticated sessions; retry
            // subscription after startup rather than silently losing it.
            // Wait for VTE's new grid before requesting a keyframe: racing
            // that request with allocation can leave Vim 9.1 on the old grid.
            // Disabling rendering does not need to wait for allocation.
            if *subscribed != Some(wanted)
                && (!wanted || terminal_grid_is_allocated(&terminal))
                && session
                    .send(Vim9Command::SetRender { enabled: wanted })
                    .is_ok()
            {
                *subscribed = Some(wanted);
            }
            let geometry_matches = frame.as_ref().is_some_and(|frame| {
                frame.revision > *render_revision_floor
                    && terminal_grid_is_allocated(&terminal)
                    && frame.rows == terminal.row_count().max(0) as usize
                    && frame.columns == terminal.column_count().max(0) as usize
            });
            let terminal_selection = terminal.has_selection();
            let active = wanted
                && !terminal_selection
                && geometry_matches
                && frame.as_ref().is_some_and(|frame| frame.supported);
            let metrics = terminal_metrics(&terminal);
            let next = if active { frame.clone() } else { None };
            let animations =
                smooth.is_active() && poll_surface.settings().is_gtk_enable_animations();
            if poll_surface.imp().wheel_animation.replace(animations) && !animations {
                for scroll in poll_surface.imp().pixels.borrow_mut().values_mut() {
                    scroll.stop_animation();
                }
            }
            // Raw surface pixels describe direct manipulation, independent
            // of the optional animation used to interpolate wheel notches.
            poll_surface.advance_native_input(&session, &input, &next, metrics, active);
            poll_surface.retarget_scroll(&next, metrics, now, animations);
            {
                let mut input = poll_surface.imp().scroll_input.borrow_mut();
                input.requested.retain(|_, time| now - *time < 0.25);
                if active {
                    let pending = std::mem::take(&mut input.pending);
                    for (window, lines) in pending {
                        if lines == 0
                            || !frame
                                .as_ref()
                                .is_some_and(|frame| legacy_scroll_target(frame, window))
                        {
                            continue;
                        }
                        if session.send(Vim9Command::Scroll { window, lines }).is_ok() {
                            input.requested.insert(window, now);
                        } else {
                            input.pending.insert(window, lines);
                        }
                    }
                } else {
                    input.pending.clear();
                    input.fractions.clear();
                    input.requested.clear();
                }
            }
            let mut state = poll_surface.imp().state.borrow_mut();
            let mut changed = match (&state.frame, &next) {
                (Some(previous), Some(next)) => !Arc::ptr_eq(previous, next),
                (None, None) => false,
                _ => true,
            } || state.metrics != metrics;
            let focused = terminal.has_focus();
            let cursor_on =
                if let Some(frame) = next.as_ref().filter(|f| f.cursor.appearance.is_some()) {
                    state
                        .cursor_blink
                        .sample(frame, focused, std::time::Instant::now())
                        .0
                } else {
                    state.cursor_blink.reset();
                    true
                };
            changed |= state.cursor_on != cursor_on;
            state.cursor_on = cursor_on;
            state.metrics = metrics;
            state.time = now;
            state.frame = next;
            state.focused = terminal.has_focus();
            if !active {
                state.preedit = None;
            }
            drop(state);
            if changed {
                poll_surface.queue_draw();
            }

            if im_active.replace(active) != active {
                if active {
                    im_keys.set_im_context(Some(&im));
                    if terminal.has_focus() {
                        im.focus_in();
                    }
                } else {
                    im.focus_out();
                    im.reset();
                    im_keys.set_im_context(None::<&gtk::IMContext>);
                }
            }
            if active {
                if let Some(cursor) = poll_surface.presented_cursor() {
                    im.set_cursor_location(&gdk::Rectangle::new(
                        cursor.x.round() as i32,
                        cursor.y.round() as i32,
                        cursor.width.ceil() as i32,
                        cursor.height.ceil() as i32,
                    ));
                }
            }
            let status = if !wanted {
                "Vim · compatibility display".into()
            } else if terminal_selection {
                "Vim · terminal text selection".into()
            } else if active {
                "GUI · RGB / Pango".into()
            } else if let Some(frame) = frame.as_ref() {
                if !frame.supported {
                    format!("Vim · {}", super::visible(&frame.fallback_reason))
                } else {
                    "GUI · resizing".into()
                }
            } else {
                "GUI · connecting".into()
            };
            if status != *old_status {
                poll_badge.set_label(&status);
                poll_badge.set_tooltip_text(Some(&status));
                render_status.set_label(&status);
                *old_status = status;
            }
            glib::ControlFlow::Continue
        });
        Self {
            widget,
            menu,
            badge,
            surface,
        }
    }
}

fn terminal_metrics(terminal: &vte4::Terminal) -> Metrics {
    let border = gtk::prelude::ScrollableExt::border(terminal);
    let (x, y) = border.as_ref().map_or((0.0, 0.0), |border| {
        (border.left() as f32, border.top() as f32)
    });
    Metrics {
        origin_x: x,
        origin_y: y,
        cell_width: terminal.char_width().max(1) as f32,
        line_height: terminal.char_height().max(1) as f32,
    }
}

fn terminal_grid_is_allocated(terminal: &vte4::Terminal) -> bool {
    let border = gtk::prelude::ScrollableExt::border(terminal);
    let (horizontal, vertical) = border.as_ref().map_or((0, 0), |border| {
        (
            i32::from(border.left()) + i32::from(border.right()),
            i32::from(border.top()) + i32::from(border.bottom()),
        )
    });
    let columns = (terminal.width() - horizontal).max(0) as i64 / terminal.char_width().max(1);
    let rows = (terminal.height() - vertical).max(0) as i64 / terminal.char_height().max(1);
    // set_font changes character metrics before GTK reallocates VTE's PTY.
    // Geometry is ready only when the published counts fit the current size.
    rows > 0 && columns > 0 && rows == terminal.row_count() && columns == terminal.column_count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_identity_ignores_source_and_cursor_but_tracks_every_paint_key() {
        use jterm_core::vim9::Vim9RenderCompletion;
        let frame = Arc::new(Vim9RenderFrame {
            completion: Some(Vim9RenderCompletion::default()),
            ..Default::default()
        });
        let mut state = PaintState::default();
        let cached = OverlayScene {
            frame: frame.clone(),
            metrics: state.metrics,
            font_generation: state.font_generation,
            ligatures: state.ligatures,
            scale_factor: 1,
            size: (320, 200),
            node: None,
        };
        let mut next = (*frame).clone();
        next.cursor.column += 1;
        next.runs.push(Vim9RenderRun {
            text: "source changed".into(),
            ..Default::default()
        });
        assert!(cached.matches(&state, &Arc::new(next.clone()), 1, (320, 200)));
        next.completion.as_mut().unwrap().view.column += 1;
        assert!(!cached.matches(&state, &Arc::new(next), 1, (320, 200)));
        let mut next = (*frame).clone();
        next.default_style.background = Some(0x123456);
        assert!(!cached.matches(&state, &Arc::new(next), 1, (320, 200)));
        assert!(!cached.matches(&state, &frame, 2, (320, 200)));
        assert!(!cached.matches(&state, &frame, 1, (321, 200)));
        state.font_generation += 1;
        assert!(!cached.matches(&state, &frame, 1, (320, 200)));
        state.font_generation -= 1;
        state.ligatures = !state.ligatures;
        assert!(!cached.matches(&state, &frame, 1, (320, 200)));
        state.ligatures = !state.ligatures;
        state.metrics.origin_x += 1.0;
        assert!(!cached.matches(&state, &frame, 1, (320, 200)));
    }

    #[test]
    #[ignore = "requires GTK display; checks retained popup/completion nodes and font-context invalidation"]
    fn retained_overlays_reuse_blink_frames_then_refresh_theme_contents_and_pango() {
        use jterm_core::vim9::Vim9RenderCompletion;
        gtk::init().expect("GTK display required");
        let surface = BufferSurface::new();
        surface.allocate(320, 200, -1, None);
        let mut frame = Vim9RenderFrame {
            completion: Some(Vim9RenderCompletion {
                view: Vim9RenderRect {
                    row: 1,
                    column: 1,
                    rows: 2,
                    columns: 10,
                },
                runs: vec![Vim9RenderRun {
                    row: 1,
                    column: 1,
                    cells: 4,
                    text: "menu".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        {
            let mut state = surface.imp().state.borrow_mut();
            state.metrics = Metrics {
                origin_x: 0.0,
                origin_y: 0.0,
                cell_width: 10.0,
                line_height: 20.0,
            };
            state.frame = Some(Arc::new(frame.clone()));
        }
        let draw = || {
            let snapshot = gtk::Snapshot::new();
            surface.imp().snapshot(&snapshot);
            assert!(snapshot.to_node().is_some());
        };
        draw();
        assert_eq!(surface.imp().overlay_builds.get(), 1);
        for column in 0..20 {
            frame.cursor.column = column;
            surface.imp().state.borrow_mut().frame = Some(Arc::new(frame.clone()));
            draw();
        }
        assert_eq!(
            surface.imp().overlay_builds.get(),
            1,
            "cursor-only frames reuse the entire overlay scene"
        );
        frame.completion.as_mut().unwrap().runs[0].text = "next".into();
        surface.imp().state.borrow_mut().frame = Some(Arc::new(frame.clone()));
        draw();
        assert_eq!(surface.imp().overlay_builds.get(), 2);
        frame.default_style.background = Some(0x214365);
        surface.imp().state.borrow_mut().frame = Some(Arc::new(frame.clone()));
        draw();
        assert_eq!(surface.imp().overlay_builds.get(), 3);
        let context = surface.pango_context();
        context.changed();
        draw();
        assert_eq!(
            surface.imp().overlay_builds.get(),
            4,
            "Pango context generation must retire overlays"
        );
        surface.allocate(300, 200, -1, None);
        draw();
        assert_eq!(
            surface.imp().overlay_builds.get(),
            5,
            "widget clipping changes require a new overlay node"
        );
        frame.completion = None;
        surface.imp().state.borrow_mut().frame = Some(Arc::new(frame));
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(
            surface.imp().overlay_scene.borrow().is_none(),
            "closing the last overlay releases its scene"
        );
    }

    #[test]
    fn clipping_limits_decoration_work_without_moving_scrolled_row_baselines() {
        let rect = Vim9PixelRect {
            x: -100000.25,
            y: -7.5,
            width: 200000.0,
            height: 20.0,
        };
        let clip = gtk::graphene::Rect::new(10.25, 0.0, 80.0, 10.0);
        let visible = clip_run_rect(rect, &clip).unwrap();
        assert_eq!((visible.x, visible.width), (10.25, 80.0));
        assert_eq!((visible.y, visible.height), (rect.y, rect.height));
        let phase = 3.25 + visible.x - rect.x;
        let mut points = Vec::new();
        undercurl_path(
            visible.x,
            visible.y + visible.height - 2.0,
            visible.width,
            phase,
            |x, y| points.push((x, y)),
        );
        assert!(
            points.len() <= 42,
            "hidden line width must not grow Cairo paths"
        );
        assert!(points.iter().all(|(x, _)| *x >= 10.25 && *x <= 90.25));
        assert!(clip_run_rect(rect, &gtk::graphene::Rect::new(0.0, 20.0, 10.0, 10.0)).is_none());
        assert!(clip_run_rect(rect, &gtk::graphene::Rect::new(0.0, 0.0, 0.0, 10.0)).is_none());
        assert!(clip_run_rect(rect, &gtk::graphene::Rect::new(0.0, 0.0, 10.0, 0.0)).is_none());
    }

    #[test]
    #[ignore = "requires GTK display; verifies invisible runs never enter Pango cache"]
    fn clipped_ink_skips_shaping_but_retains_italic_overhang() {
        gtk::init().expect("GTK display required");
        let surface = BufferSurface::new();
        let state = PaintState {
            metrics: Metrics {
                origin_x: 0.0,
                origin_y: 0.0,
                cell_width: 10.0,
                line_height: 20.0,
            },
            ..Default::default()
        };
        let run = Vim9RenderRun {
            text: "界e\u{301}".into(),
            cells: 3,
            style: Vim9RenderStyle {
                italic: true,
                undercurl: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let rect = Vim9PixelRect {
            x: 0.0,
            y: 0.0,
            width: 30.0,
            height: 20.0,
        };
        let mut layouts = LayoutCache::default();
        let snapshot = gtk::Snapshot::new();
        paint_text_run(
            &surface,
            &snapshot,
            &state,
            &run,
            &Vim9RenderStyle::default(),
            rect,
            16.0,
            0.0,
            &gtk::graphene::Rect::new(40.0, 0.0, 10.0, 20.0),
            &mut layouts,
        );
        assert_eq!(layouts.len(), 0);
        assert!(snapshot.to_node().is_none());
        let snapshot = gtk::Snapshot::new();
        paint_text_run(
            &surface,
            &snapshot,
            &state,
            &run,
            &Vim9RenderStyle::default(),
            rect,
            16.0,
            0.0,
            &gtk::graphene::Rect::new(32.0, 0.0, 10.0, 20.0),
            &mut layouts,
        );
        assert_eq!(
            layouts.len(),
            1,
            "nearby italic ink must still shape its whole Unicode run"
        );
    }

    #[test]
    fn layout_pressure_keeps_visible_margin_and_overlay_shapes_warm() {
        use jterm_core::vim9::{Vim9RenderCompletion, Vim9RenderMarginRow, Vim9RenderWindow};
        let run = |text: &str| Vim9RenderRun {
            text: text.into(),
            ..Default::default()
        };
        let frame = Vim9RenderFrame {
            runs: vec![run("visible")],
            windows: vec![Vim9RenderWindow {
                overscan: vec![Vim9RenderMarginRow {
                    runs: vec![run("margin")],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            completion: Some(Vim9RenderCompletion {
                runs: vec![run("completion")],
                ..Default::default()
            }),
            popups: vec![Vim9RenderPopup {
                runs: vec![run("popup")],
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut layouts = StyledCache::default();
        for index in 0..4096 {
            layouts.get_or_insert_with(&format!("cold-{index}"), false, false, || index);
        }
        for text in ["visible", "margin", "completion", "popup"] {
            layouts.get_or_insert_with(text, false, false, || usize::MAX);
        }
        trim_layout_cache(&frame, &mut layouts);
        assert_eq!(layouts.len(), 4);
        assert!(layouts
            .buckets
            .iter()
            .flat_map(|bucket| bucket.values())
            .all(|value| *value == usize::MAX));
        // A stale font variant must not count as the visible regular shape.
        for index in 0..4096 {
            layouts.get_or_insert_with(&format!("cold-{index}"), true, false, || index);
        }
        layouts.get_or_insert_with("visible", true, false, || 0);
        trim_layout_cache(&frame, &mut layouts);
        assert_eq!(layouts.len(), 4);
        assert!(layouts.get("visible", true, false).is_none());
        assert_eq!(layouts.get("visible", false, false), Some(&usize::MAX));
    }

    #[test]
    #[ignore = "requires GTK display; verifies clipped native cursor pixels"]
    fn native_cursor_shape_does_not_move_into_clipped_row() {
        use jterm_core::vim9::{Vim9CursorAppearance, Vim9CursorShape};
        gtk::init().unwrap();
        let surface = BufferSurface::new();
        let state = PaintState {
            focused: true,
            metrics: Metrics {
                origin_x: 0.0,
                origin_y: 0.0,
                cell_width: 8.0,
                line_height: 20.0,
            },
            ..Default::default()
        };
        let frame = Vim9RenderFrame {
            default_style: Vim9RenderStyle {
                foreground: Some(0xffffff),
                background: Some(0),
                ..Default::default()
            },
            cursor: Vim9RenderCursor {
                visible: true,
                width: 1,
                appearance: Some(Vim9CursorAppearance {
                    shape: Vim9CursorShape::Horizontal,
                    percentage: 20,
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let render = |height: f32| {
            let snapshot = gtk::Snapshot::new();
            snapshot.append_color(&rgb(0), &gtk::graphene::Rect::new(0.0, 0.0, 8.0, 20.0));
            paint_native_cursor(
                &surface,
                &snapshot,
                &state,
                &frame,
                &gtk::graphene::Rect::new(0.0, 0.0, 8.0, height),
                &LayoutCache::default(),
            );
            let mut image =
                gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, 8, 20).unwrap();
            let cr = gtk::cairo::Context::new(&image).unwrap();
            snapshot.to_node().unwrap().draw(&cr);
            drop(cr);
            let pixels = image.data().unwrap().to_vec();
            pixels
        };
        let full = render(20.0);
        assert!(full
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[..3] != [0, 0, 0]));
        let cropped = render(10.0);
        assert!(
            cropped
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[..3] == [0, 0, 0]),
            "clipping the bottom half must hide, not reposition, the underline"
        );
    }

    #[test]
    #[ignore = "requires GTK display; verifies background-only runs and paint layer order"]
    fn native_background_layers_keep_ink_and_peer_split_clip() {
        use jterm_core::vim9::Vim9RenderWindow;
        gtk::init().unwrap();
        let surface = BufferSurface::new();
        let state = PaintState {
            metrics: Metrics {
                origin_x: 0.0,
                origin_y: 0.0,
                cell_width: 8.0,
                line_height: 10.0,
            },
            ..Default::default()
        };
        for marker in ["→", "·"] {
            let run = Vim9RenderRun {
                text: marker.into(),
                cells: 1,
                line: 1,
                byte_start: 0,
                byte_end: 1,
                style: Vim9RenderStyle {
                    underline: true,
                    ..Default::default()
                },
                ..Default::default()
            };
            let marker_snapshot = gtk::Snapshot::new();
            let mut cache = LayoutCache::default();
            paint_text_run(
                &surface,
                &marker_snapshot,
                &state,
                &run,
                &Vim9RenderStyle::default(),
                Vim9PixelRect {
                    x: 0.0,
                    y: 0.0,
                    width: state.metrics.cell_width,
                    height: state.metrics.line_height,
                },
                row_baseline(&surface, &state),
                0.0,
                &gtk::graphene::Rect::new(0.0, 0.0, 8.0, 10.0),
                &mut cache,
            );
            assert!(cache.get(marker, false, false).unwrap().width > 0.0);
            assert!(
                marker_snapshot.to_node().unwrap().bounds().width()
                    <= state.metrics.cell_width + 0.5,
                "listchars marker must retain one-cell advance"
            );
        }
        let left = Vim9RenderRect {
            row: 0,
            column: 0,
            rows: 1,
            columns: 2,
        };
        let right = Vim9RenderRect {
            column: 3,
            ..left.clone()
        };
        let frame = Vim9RenderFrame {
            supported: true,
            view: left.clone(),
            default_style: Vim9RenderStyle {
                foreground: Some(0xffffff),
                background: Some(0),
                ..Default::default()
            },
            windows: vec![
                Vim9RenderWindow {
                    window: 10,
                    view: left,
                    ..Default::default()
                },
                Vim9RenderWindow {
                    window: 20,
                    view: right,
                    ..Default::default()
                },
            ],
            runs: vec![
                // The later empty background must not erase earlier ink.
                Vim9RenderRun {
                    window: 10,
                    cells: 2,
                    text: "  ".into(),
                    line: 1,
                    style: Vim9RenderStyle {
                        underline: true,
                        special: Some(0xff0000),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Vim9RenderRun {
                    window: 10,
                    cells: 2,
                    line: 0,
                    style: Vim9RenderStyle {
                        background: Some(0x0000ff),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Vim9RenderRun {
                    window: 20,
                    column: 3,
                    cells: 2,
                    line: 0,
                    style: Vim9RenderStyle {
                        background: Some(0x00ff00),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        for pixel_path in [false, true] {
            let snapshot = gtk::Snapshot::new();
            snapshot.append_color(&rgb(0), &gtk::graphene::Rect::new(0.0, 0.0, 40.0, 10.0));
            let mut layouts = LayoutCache::default();
            if pixel_path {
                paint_pixel_scene(&surface, &snapshot, &state, &frame, &mut layouts);
            } else {
                snapshot
                    .append_node(build_text_scene(&surface, &state, &frame, &mut layouts).unwrap());
            }
            let mut image =
                gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, 40, 10).unwrap();
            let cr = gtk::cairo::Context::new(&image).unwrap();
            snapshot.to_node().unwrap().draw(&cr);
            drop(cr);
            let stride = image.stride() as usize;
            let bytes = image.data().unwrap().to_vec();
            let pixel = |x: usize, y: usize| {
                let i = y * stride + x * 4;
                u32::from_ne_bytes(bytes[i..i + 4].try_into().unwrap()) & 0xffffff
            };
            assert_eq!(pixel(4, 3), 0x0000ff);
            assert_eq!(pixel(4, 8), 0xff0000);
            assert_eq!(pixel(18, 3), 0x000000);
            assert_eq!(pixel(28, 3), 0x00ff00);
        }
    }

    #[test]
    fn same_cell_cursor_confirmation_restarts_original_vim_blink_wait() {
        use jterm_core::vim9::Vim9CursorAppearance;
        let frame = Vim9RenderFrame {
            cursor: Vim9RenderCursor {
                visible: true,
                appearance: Some(Vim9CursorAppearance {
                    blinkwait: 100,
                    blinkon: 100,
                    blinkoff: 1000,
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let start = std::time::Instant::now();
        let mut state = PaintState::default();
        assert!(state.cursor_blink.sample(&frame, true, start).0);
        state.cursor_on = state
            .cursor_blink
            .sample(&frame, true, start + std::time::Duration::from_millis(101))
            .0;
        assert!(!state.cursor_on);
        state.restart_cursor_blink();
        assert!(
            state.cursor_on,
            "a same-cell click immediately reveals the caret"
        );
        assert!(
            state
                .cursor_blink
                .sample(&frame, true, start + std::time::Duration::from_millis(102))
                .0
        );
        assert!(
            state
                .cursor_blink
                .sample(&frame, true, start + std::time::Duration::from_millis(201))
                .0
        );
        assert!(
            !state
                .cursor_blink
                .sample(&frame, true, start + std::time::Duration::from_millis(203))
                .0,
            "confirmation restarts rather than disables Vim's blink timing"
        );
    }

    #[test]
    #[ignore = "requires GTK display; verifies native body focus and same-cell cursor confirmation"]
    fn native_body_press_focuses_editor_and_reveals_hidden_cursor() {
        if !jterm_core::child_env::inherited_environment_is_captured() {
            jterm_core::child_env::capture_inherited_environment().unwrap();
        }
        gtk::init().unwrap();
        let session = Vim9Session::start(jterm_core::vim9::Vim9EntryRequest::user_action(
            std::env::temp_dir(),
        ))
        .unwrap();
        let terminal = vte4::Terminal::new();
        let native = NativeBuffer::new(&session, &terminal, true, true);
        let field = gtk::Entry::new();
        let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
        layout.append(&native.widget);
        layout.append(&field);
        let window = gtk::Window::new();
        window.set_default_size(320, 160);
        window.set_child(Some(&layout));
        window.present();
        while glib::MainContext::default().pending() {
            glib::MainContext::default().iteration(false);
        }
        field.grab_focus();
        assert!(!terminal.is_focus());
        {
            let mut state = native.surface.imp().state.borrow_mut();
            state.frame = Some(Arc::new(Vim9RenderFrame {
                supported: true,
                view: Vim9RenderRect {
                    rows: 2,
                    columns: 10,
                    ..Default::default()
                },
                ..Default::default()
            }));
            state.metrics = Metrics {
                origin_x: 0.0,
                origin_y: 0.0,
                cell_width: 8.0,
                line_height: 20.0,
            };
        }
        let snapshot = gtk::Snapshot::new();
        native.surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_some());
        let controllers = terminal.observe_controllers();
        let click = (0..controllers.n_items())
            .find_map(|index| {
                controllers
                    .item(index)?
                    .downcast::<gtk::GestureClick>()
                    .ok()
                    .filter(|controller| controller.name().as_deref() == Some("vim9-buffer-click"))
            })
            .unwrap();
        for _ in 0..2 {
            native.surface.imp().state.borrow_mut().cursor_on = false;
            click.emit_by_name::<()>("pressed", &[&1_i32, &4.0_f64, &4.0_f64]);
            assert!(terminal.is_focus());
            assert!(native.surface.imp().state.borrow().cursor_on);
        }
        window.close();
    }

    #[test]
    fn render_only_policy_preserves_legacy_and_supported_native_inputs() {
        let mut window = Vim9RenderWindow::default();
        assert!(!render_only_window(&window));
        window.line_count = 1;
        assert!(
            !render_only_window(&window),
            "metadata must be complete before routing changes"
        );
        window.line_lengths = vec![0];
        assert!(
            render_only_window(&window),
            "an empty modern buffer still has an explicit line"
        );
        window.native_pointer = true;
        assert!(
            !render_only_window(&window),
            "Visual pointer ownership keeps its frozen pixel image"
        );
        window.native_pointer = false;
        window.native_scroll = true;
        assert!(
            !render_only_window(&window),
            "Insert/custom-mouse windows keep native scrolling"
        );
        window.native_scroll = false;
        window.line_count = 0;
        assert!(
            !render_only_window(&window),
            "old metadata does not opt into render-only input"
        );
    }

    #[test]
    fn render_only_transition_discards_queued_legacy_scroll_without_touching_peers() {
        let render_only = Vim9RenderWindow {
            window: 10,
            line_count: 1,
            line_lengths: vec![0],
            ..Default::default()
        };
        let native = Vim9RenderWindow {
            window: 20,
            native_scroll: true,
            ..render_only.clone()
        };
        let legacy = Vim9RenderWindow {
            window: 30,
            ..Default::default()
        };
        let frame = Vim9RenderFrame {
            windows: vec![render_only.clone(), native, legacy],
            ..Default::default()
        };
        let mut input = ScrollInput::default();
        for window in [10, 20, 30] {
            input.pending.insert(window, 3);
            input.requested.insert(window, 1.0);
            assert_eq!(input.fractions.add(window, 0.6), 0);
        }
        input.retire_render_only(&frame);
        assert!(!input.pending.contains_key(&10));
        assert!(!input.requested.contains_key(&10));
        assert_eq!(input.fractions.add(10, 0.6), 0);
        assert!(
            !legacy_scroll_target(&frame, 10),
            "the final command-send gate also refuses this window"
        );
        for window in [20, 30] {
            assert_eq!(input.pending.get(&window), Some(&3));
            assert_eq!(input.requested.get(&window), Some(&1.0));
            assert_eq!(input.fractions.add(window, 0.6), 1);
            assert!(legacy_scroll_target(&frame, window));
        }
        let mut previous_window = render_only.clone();
        previous_window.native_pointer = true;
        assert!(
            !same_scroll_window(&frame, &frame, &previous_window, &render_only),
            "render-only windows never inherit a legacy animation"
        );
    }

    #[test]
    fn window_normal_changes_retire_only_the_affected_scroll_history() {
        let previous = Vim9RenderFrame {
            window: 10,
            windows: vec![
                Vim9RenderWindow {
                    window: 10,
                    view: Vim9RenderRect {
                        rows: 2,
                        columns: 4,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Vim9RenderWindow {
                    window: 20,
                    view: Vim9RenderRect {
                        column: 5,
                        rows: 2,
                        columns: 4,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ],
            runs: vec![
                Vim9RenderRun {
                    window: 10,
                    cells: 4,
                    style: Vim9RenderStyle {
                        background: Some(0x102030),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Vim9RenderRun {
                    window: 20,
                    column: 5,
                    cells: 4,
                    style: Vim9RenderStyle {
                        background: Some(0xffeedd),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut next = previous.clone();
        // A normal acknowledgement may retain history across a topline move.
        next.windows[0].topline += 1;
        assert!(same_scroll_window(
            &previous,
            &next,
            &previous.windows[0],
            &next.windows[0]
        ));
        // A stationary peer's palette change still retires that peer's motion;
        // the comparison must happen before the equal-topline early return.
        next.runs[1].style.background = Some(0x334455);
        assert_eq!(window_normal_style(&next, 10).background, Some(0x102030));
        assert_eq!(window_normal_style(&next, 20).background, Some(0x334455));
        assert!(same_scroll_window(
            &previous,
            &next,
            &previous.windows[0],
            &next.windows[0]
        ));
        assert!(!same_scroll_window(
            &previous,
            &next,
            &previous.windows[1],
            &next.windows[1]
        ));
        next = previous.clone();
        next.runs[0].style.foreground = Some(0xffffff);
        assert!(!same_scroll_window(
            &previous,
            &next,
            &previous.windows[0],
            &next.windows[0]
        ));
        next = previous.clone();
        next.runs[0].style.bold = true;
        assert!(!same_scroll_window(
            &previous,
            &next,
            &previous.windows[0],
            &next.windows[0]
        ));
        assert!(same_scroll_window(
            &previous,
            &next,
            &previous.windows[1],
            &next.windows[1]
        ));
    }

    #[test]
    fn cursor_palette_resolves_blank_eol_and_glyph_styles_per_window() {
        use jterm_core::vim9::{Vim9CursorAppearance, Vim9RenderWindow};
        let light = Vim9RenderStyle {
            foreground: Some(0x102030),
            background: Some(0xffeedd),
            ..Default::default()
        };
        let dark = Vim9RenderStyle {
            foreground: Some(0xddeeff),
            background: Some(0x203040),
            ..Default::default()
        };
        let token = Vim9RenderStyle {
            foreground: Some(0xaa3300),
            ..light.clone()
        };
        let mut frame = Vim9RenderFrame {
            window: 10,
            default_style: Vim9RenderStyle {
                foreground: Some(0xffffff),
                background: Some(0),
                ..Default::default()
            },
            cursor: Vim9RenderCursor {
                row: 0,
                column: 3,
                visible: true,
                width: 1,
                ..Default::default()
            },
            windows: vec![
                Vim9RenderWindow {
                    window: 10,
                    view: Vim9RenderRect {
                        rows: 1,
                        columns: 4,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Vim9RenderWindow {
                    window: 20,
                    view: Vim9RenderRect {
                        column: 5,
                        rows: 1,
                        columns: 4,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ],
            runs: vec![
                Vim9RenderRun {
                    window: 10,
                    cells: 4,
                    line: 0,
                    style: light.clone(),
                    ..Default::default()
                },
                Vim9RenderRun {
                    window: 10,
                    cells: 1,
                    line: 1,
                    text: "a".into(),
                    style: token.clone(),
                    ..Default::default()
                },
                Vim9RenderRun {
                    window: 20,
                    column: 5,
                    cells: 4,
                    line: 0,
                    style: dark.clone(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(window_normal_style(&frame, frame.window), &light);
        assert_eq!(
            cursor_under_style(&frame),
            &light,
            "blank/EOL must use its own full-row backdrop"
        );
        assert_eq!(
            Vim9CursorAppearance::default()
                .colors(cursor_under_style(&frame), &frame.default_style),
            (0xffeedd, 0x102030)
        );
        frame.cursor.column = 0;
        assert_eq!(
            window_normal_style(&frame, frame.window),
            &light,
            "IME Normal is independent of syntax"
        );
        assert_eq!(
            cursor_under_style(&frame),
            &token,
            "resolved glyph style wins over backdrop"
        );
        frame.window = 20;
        frame.cursor.column = 7;
        assert_eq!(cursor_under_style(&frame), &dark);
        assert_eq!(window_normal_style(&frame, frame.window), &dark);
        frame.cursor.row = 99;
        assert_eq!(
            window_normal_style(&frame, frame.window),
            &dark,
            "offscreen caret keeps its window Normal"
        );
        frame.cursor.row = 0;
        // A peer's source cannot supply the active palette, even if queried
        // during an inconsistent incoming frame before its geometry retires.
        frame.cursor.column = 0;
        assert_eq!(cursor_under_style(&frame), &frame.default_style);
        let mut legacy_palette = frame.clone();
        legacy_palette.window = 10;
        legacy_palette.runs.retain(|run| !run.text.is_empty());
        assert_eq!(
            window_normal_style(&legacy_palette, legacy_palette.window),
            &legacy_palette.default_style,
            "older producers preserve their global Normal overlay palette"
        );
        assert_eq!(cursor_under_style(&legacy_palette), &token);
        frame.windows.clear();
        assert_eq!(
            cursor_under_style(&frame),
            &token,
            "legacy single-view frames retain their palette"
        );
    }

    #[test]
    #[ignore = "requires GTK display; verifies per-window IME composition background"]
    fn native_preedit_uses_blank_window_backdrop() {
        gtk::init().unwrap();
        let surface = BufferSurface::new();
        let frame = Vim9RenderFrame {
            window: 10,
            view: Vim9RenderRect {
                rows: 1,
                columns: 4,
                ..Default::default()
            },
            default_style: Vim9RenderStyle {
                foreground: Some(0xffffff),
                background: Some(0),
                ..Default::default()
            },
            cursor: Vim9RenderCursor {
                column: 1,
                width: 1,
                visible: true,
                ..Default::default()
            },
            runs: vec![Vim9RenderRun {
                window: 10,
                cells: 4,
                line: 0,
                style: Vim9RenderStyle {
                    foreground: Some(0x102030),
                    background: Some(0xffeedd),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let state = PaintState {
            frame: Some(Arc::new(frame)),
            metrics: Metrics {
                origin_x: 0.0,
                origin_y: 0.0,
                cell_width: 8.0,
                line_height: 20.0,
            },
            ..Default::default()
        };
        let preedit = Preedit {
            text: " ".into(),
            attributes: pango::AttrList::new(),
            cursor: 0,
        };
        let snapshot = gtk::Snapshot::new();
        snapshot.append_color(&rgb(0), &gtk::graphene::Rect::new(0.0, 0.0, 40.0, 20.0));
        paint_preedit(
            &surface,
            &snapshot,
            &state,
            &preedit,
            &gtk::graphene::Rect::new(8.0, 0.0, 8.0, 20.0),
            &gtk::graphene::Rect::new(0.0, 0.0, 40.0, 20.0),
        );
        let mut image =
            gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, 40, 20).unwrap();
        let cr = gtk::cairo::Context::new(&image).unwrap();
        snapshot.to_node().unwrap().draw(&cr);
        drop(cr);
        let stride = image.stride() as usize;
        let bytes = image.data().unwrap().to_vec();
        let i = 2 * stride + 9 * 4;
        assert_eq!(
            u32::from_ne_bytes(bytes[i..i + 4].try_into().unwrap()) & 0xffffff,
            0xffeedd
        );
    }

    #[test]
    fn undercurl_is_continuous_across_run_splits_at_fractional_origins_and_font_sizes() {
        fn points(x: f32, y: f32, width: f32, phase: f32) -> Vec<(f32, f32)> {
            let mut points = Vec::new();
            undercurl_path(x, y, width, phase, |x, y| points.push((x, y)));
            points
        }
        fn sample(points: &[(f32, f32)], x: f32) -> f32 {
            let edge = points
                .windows(2)
                .find(|edge| x >= edge[0].0 && x <= edge[1].0)
                .unwrap();
            let t = (x - edge[0].0) / (edge[1].0 - edge[0].0);
            edge[0].1 + t * (edge[1].1 - edge[0].1)
        }
        for cell_width in [6.75, 8.0, 10.5] {
            for origin in [-3.5, 0.0, 123.375] {
                let phase = cell_width * 1.25;
                let widths = [cell_width * 0.25, cell_width * 1.75, cell_width * 0.5];
                let width = widths.iter().sum();
                let whole = points(origin, 30.25, width, phase);
                let mut offset = 0.0;
                let mut previous = None;
                for width in widths {
                    let part = points(origin + offset, 30.25, width, phase + offset);
                    if let Some(previous) = previous {
                        assert_eq!(part[0], previous, "run boundary changed curl phase");
                    }
                    previous = part.last().copied();
                    for step in 0..=32 {
                        let x = origin + offset + width * step as f32 / 32.0;
                        assert!((sample(&whole, x) - sample(&part, x)).abs() < 0.0001);
                    }
                    offset += width;
                }
                let translated = points(origin + 17.5, 23.0, width, phase);
                assert_eq!(whole.len(), translated.len());
                for (a, b) in whole.iter().zip(translated) {
                    assert_eq!((a.0 + 17.5, a.1 - 7.25), b);
                }
            }
        }
    }

    #[test]
    fn undercurl_phase_follows_own_window_not_widget_or_peer_split_origin() {
        use jterm_core::vim9::Vim9RenderWindow;
        let metrics = Metrics {
            origin_x: 91.25,
            origin_y: 17.5,
            cell_width: 7.5,
            line_height: 20.0,
        };
        let mut frame = Vim9RenderFrame {
            windows: vec![
                Vim9RenderWindow {
                    window: 10,
                    view: Vim9RenderRect {
                        column: 2,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Vim9RenderWindow {
                    window: 20,
                    view: Vim9RenderRect {
                        column: 42,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let left = Vim9RenderRun {
            window: 10,
            column: 5,
            ..Default::default()
        };
        let mut right = Vim9RenderRun {
            window: 20,
            column: 45,
            ..Default::default()
        };
        assert_eq!(decoration_phase_x(&frame, &left, metrics), 22.5);
        assert_eq!(decoration_phase_x(&frame, &right, metrics), 22.5);
        frame.windows[1].view.column += 3;
        right.column += 3;
        assert_eq!(decoration_phase_x(&frame, &right, metrics), 22.5);
        assert_eq!(
            decoration_phase_x(
                &frame,
                &left,
                Metrics {
                    origin_x: 0.0,
                    ..metrics
                }
            ),
            22.5
        );
    }

    #[test]
    fn background_only_run_changes_invalidate_retained_scene() {
        let frame = Vim9RenderFrame {
            view: Vim9RenderRect {
                rows: 1,
                columns: 8,
                ..Default::default()
            },
            runs: vec![Vim9RenderRun {
                cells: 1,
                line: 0,
                text: String::new(),
                style: Vim9RenderStyle {
                    background: Some(0x102030),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut next = frame.clone();
        next.cursor.column = 1;
        assert!(same_text_content(&frame, &next));
        next.runs[0].style.background = Some(0x203040);
        assert!(!same_text_content(&frame, &next));
        next = frame.clone();
        next.runs[0].column += 1;
        assert!(!same_text_content(&frame, &next));
        let mut layers = frame.clone();
        layers.runs.push(Vim9RenderRun {
            style: Vim9RenderStyle {
                background: Some(0x304050),
                ..Default::default()
            },
            ..layers.runs[0].clone()
        });
        let mut swapped = layers.clone();
        swapped.runs.reverse();
        assert!(!same_text_content(&layers, &swapped));
    }

    #[test]
    fn styled_layout_cache_reuses_hits_and_keeps_font_variants_separate() {
        let mut cache = StyledCache::default();
        let builds = Cell::new(0);
        for bold in [false, true] {
            for italic in [false, true] {
                let expected = builds.get() + 1;
                for _ in 0..240 {
                    assert_eq!(
                        *cache.get_or_insert_with("中文 -> ligature", bold, italic, || {
                            builds.set(builds.get() + 1);
                            builds.get()
                        }),
                        expected
                    );
                }
            }
        }
        assert_eq!(builds.get(), 4);
        assert_eq!(cache.len(), 4);
        assert_eq!(cache.get("中文 -> ligature", true, true), Some(&4));
        assert_eq!(cache.get("different source", true, true), None);
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(cache.get("中文 -> ligature", true, true).is_none());
    }

    #[test]
    fn fractional_cursor_paint_uses_full_row_before_viewport_clip() {
        use jterm_core::vim9::{Vim9CursorAppearance, Vim9CursorShape};
        let metrics = Metrics {
            origin_x: 2.0,
            origin_y: 10.0,
            cell_width: 8.0,
            line_height: 20.0,
        };
        let cursor = Vim9RenderCursor {
            visible: true,
            row: 0,
            column: 2,
            width: 1,
            ..Default::default()
        };
        let full = cursor_paint_cell(metrics, &cursor, -6.25).unwrap();
        assert_eq!(full.y, 3.75);
        assert_eq!(full.height, 20.0);
        let bar = Vim9CursorAppearance {
            shape: Vim9CursorShape::Horizontal,
            percentage: 20,
            ..Default::default()
        };
        let painted = bar.paint_rect(full, metrics.cell_width, 1.0);
        assert_eq!(painted.y, 19.75);
        assert_eq!(painted.height, 4.0);
        // Cropping belongs to the viewport, not to the shape input. A row
        // cut by the bottom edge must not move its underline into view.
        let bottom = cursor_paint_cell(metrics, &cursor, 6.25).unwrap();
        assert_eq!(bar.paint_rect(bottom, metrics.cell_width, 1.0).y, 32.25);
        assert!(cursor_paint_cell(metrics, &cursor, f32::NAN).is_none());
    }

    #[test]
    fn scroll_zoom_preserves_device_distance_and_modifier_ownership() {
        assert_eq!(scroll_zoom_step(1.0, gdk::ScrollUnit::Wheel), Some(-1.0));
        assert_eq!(
            scroll_zoom_step(0.125, gdk::ScrollUnit::Wheel),
            Some(-0.125)
        );
        assert_eq!(
            scroll_zoom_step(10.0, gdk::ScrollUnit::Surface),
            Some(-0.25)
        );
        assert_eq!(scroll_zoom_step(-40.0, gdk::ScrollUnit::Surface), Some(1.0));
        for invalid in [0.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(scroll_zoom_step(invalid, gdk::ScrollUnit::Surface), None);
        }
        let control = gdk::ModifierType::CONTROL_MASK;
        assert!(owns_scroll_zoom(control, false));
        assert!(owns_scroll_zoom(
            control | gdk::ModifierType::SHIFT_MASK,
            false
        ));
        assert!(!owns_scroll_zoom(control, true));
        assert!(!owns_scroll_zoom(gdk::ModifierType::empty(), false));
        for modifier in [gdk::ModifierType::ALT_MASK, gdk::ModifierType::SUPER_MASK] {
            assert!(!owns_scroll_zoom(control | modifier, false));
        }
    }

    #[test]
    #[ignore = "requires GTK display; verifies actual active-split edge pixels"]
    fn native_active_split_outline_stays_inside_its_viewport() {
        gtk::init().expect("GTK display required");
        use jterm_core::vim9::Vim9RenderWindow;
        let surface = BufferSurface::new();
        let mut state = PaintState {
            focused: true,
            metrics: Metrics {
                origin_x: 0.0,
                origin_y: 0.0,
                cell_width: 8.0,
                line_height: 10.0,
            },
            ..Default::default()
        };
        let mut frame = Vim9RenderFrame {
            window: 10,
            default_style: Vim9RenderStyle {
                foreground: Some(0xa0c0f0),
                background: Some(0x102030),
                ..Default::default()
            },
            windows: vec![
                Vim9RenderWindow {
                    window: 10,
                    view: Vim9RenderRect {
                        row: 1,
                        column: 1,
                        rows: 3,
                        columns: 3,
                    },
                    ..Default::default()
                },
                Vim9RenderWindow {
                    window: 20,
                    view: Vim9RenderRect {
                        row: 1,
                        column: 5,
                        rows: 3,
                        columns: 3,
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let render = |frame: &Vim9RenderFrame, state: &PaintState| {
            let snapshot = gtk::Snapshot::new();
            snapshot.append_color(
                &rgb(0x102030),
                &gtk::graphene::Rect::new(0.0, 0.0, 80.0, 50.0),
            );
            paint_active_split(&surface, &snapshot, state, frame);
            let mut image =
                gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, 80, 50).unwrap();
            let cr = gtk::cairo::Context::new(&image).unwrap();
            snapshot.to_node().unwrap().draw(&cr);
            drop(cr);
            let stride = image.stride() as usize;
            let bytes = image.data().unwrap().to_vec();
            (stride, bytes)
        };
        let pixel = |image: &(usize, Vec<u8>), x: usize, y: usize| {
            let index = y * image.0 + x * 4;
            u32::from_ne_bytes(image.1[index..index + 4].try_into().unwrap()) & 0xffffff
        };
        let focused = render(&frame, &state);
        assert_ne!(pixel(&focused, 8, 10), 0x102030);
        for (x, y) in [(8, 9), (7, 10), (9, 11), (32, 20), (40, 10), (8, 40)] {
            assert_eq!(
                pixel(&focused, x, y),
                0x102030,
                "edge crossed the viewport at {x},{y}"
            );
        }
        state.focused = false;
        let unfocused = render(&frame, &state);
        assert!(pixel(&unfocused, 8, 10) < pixel(&focused, 8, 10));
        frame.window = 20;
        let switched = render(&frame, &state);
        assert_eq!(pixel(&switched, 8, 10), 0x102030);
        assert_ne!(pixel(&switched, 40, 10), 0x102030);
        frame.windows.remove(0);
        let single = render(&frame, &state);
        assert_eq!(pixel(&single, 40, 10), 0x102030);
    }

    #[test]
    fn retained_text_scene_ignores_cursor_but_invalidates_real_paint_changes() {
        let frame = Vim9RenderFrame {
            view: Vim9RenderRect {
                row: 0,
                column: 0,
                rows: 4,
                columns: 20,
            },
            runs: vec![jterm_core::vim9::Vim9RenderRun {
                row: 0,
                column: 0,
                text: "text".into(),
                cells: 4,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut moved = frame.clone();
        moved.revision += 1;
        moved.cursor.column = 2;
        moved.mode = "i".into();
        assert!(same_text_content(&frame, &moved));
        moved.completion = Some(jterm_core::vim9::Vim9RenderCompletion::default());
        assert!(
            same_text_content(&frame, &moved),
            "completion is a transient overlay"
        );
        moved.popups.push(Vim9RenderPopup::default());
        assert!(
            same_text_content(&frame, &moved),
            "popup changes must not rebuild the underlying buffer scene"
        );
        moved.runs[0].style.foreground = Some(0x123456);
        assert!(!same_text_content(&frame, &moved));
        moved = frame.clone();
        moved.view.column += 1;
        assert!(!same_text_content(&frame, &moved));
        moved = frame.clone();
        moved.default_style.background = Some(0x102030);
        assert!(!same_text_content(&frame, &moved));
    }

    #[test]
    fn retained_text_scene_accepts_equivalent_full_frame_and_row_delta_order() {
        use jterm_core::vim9::{Vim9RenderRun, Vim9RenderWindow};
        let mut frame = Vim9RenderFrame::default();
        for column in [0, 21] {
            frame.windows.push(Vim9RenderWindow {
                view: Vim9RenderRect {
                    row: 0,
                    column,
                    rows: 2,
                    columns: 20,
                },
                ..Default::default()
            });
            for row in 0..2 {
                frame.runs.push(Vim9RenderRun {
                    row,
                    column,
                    text: format!("{row}, {column}"),
                    cells: 5,
                    ..Default::default()
                });
            }
        }
        let mut delta = frame.clone();
        delta.runs.sort_by_key(|run| run.row);
        assert_ne!(frame.runs, delta.runs);
        assert!(same_text_content(&frame, &delta));
        delta.runs[0].style.background = Some(0x123456);
        assert!(!same_text_content(&frame, &delta));
    }

    #[test]
    fn background_elision_preserves_overlapping_layers_and_row_isolation() {
        let normal = 0x102030;
        let mut backgrounds = BackgroundRows::new(normal);
        let mut full = [[normal; 12]; 4];
        let mut reduced = full;
        let mut state = 41_u32;
        // Normal text, overlapping guides/selection, and a subsequent Normal
        // layer exercise restoration as well as the no-fill common case.
        for i in 0..300 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let row = (state as usize >> 8) % 4;
            let start = (state as usize >> 16) % 10;
            let end = (start + 1 + i % 4).min(12);
            let color = if i % 7 < 5 { normal } else { state & 0xffffff };
            full[row][start..end].fill(color);
            if backgrounds.needs_fill(row, color) {
                reduced[row][start..end].fill(color);
            }
            assert_eq!(full, reduced, "paint order changed at run {i}");
        }
        let mut backgrounds = BackgroundRows::new(normal);
        assert!(!backgrounds.needs_fill(0, normal));
        assert!(backgrounds.needs_fill(0, 0xabcdef));
        assert!(backgrounds.needs_fill(0, normal));
        assert!(!backgrounds.needs_fill(1, normal));
    }

    #[test]
    fn background_elision_removes_plain_syntax_fills_without_allocating() {
        let mut backgrounds = BackgroundRows::new(0x102030);
        let fills = (0..10000)
            .filter(|index| backgrounds.needs_fill(index / 100, 0x102030))
            .count();
        assert_eq!(fills, 0);
        assert_eq!(backgrounds.changed.capacity(), 0);
    }

    #[test]
    #[ignore = "requires GTK display; checks blank-run shaping and decoration pixels"]
    fn space_only_runs_skip_layouts_but_keep_decoration_pixels() {
        gtk::init().expect("GTK display required");
        let surface = BufferSurface::new();
        let state = PaintState::default();
        let rect = Vim9PixelRect {
            x: 0.0,
            y: 0.0,
            width: 80.0,
            height: 24.0,
        };
        for text in ["        ", "   x   ", "·", "\u{a0}", "界", "e\u{301}"] {
            for decoration in 0..4 {
                let run = Vim9RenderRun {
                    text: text.into(),
                    cells: 8,
                    style: Vim9RenderStyle {
                        foreground: Some(0xffffff),
                        special: Some(0x00ff00),
                        underline: decoration == 1,
                        undercurl: decoration == 2,
                        strikethrough: decoration == 3,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let snapshot = gtk::Snapshot::new();
                snapshot.append_color(&rgb(0x102030), &graphene_rect(rect));
                let mut layouts = LayoutCache::default();
                paint_text_run(
                    &surface,
                    &snapshot,
                    &state,
                    &run,
                    &Vim9RenderStyle::default(),
                    rect,
                    16.0,
                    0.0,
                    &graphene_rect(rect),
                    &mut layouts,
                );
                assert_eq!(
                    layouts.len(),
                    usize::from(text != "        "),
                    "unexpected shaping for {text:?}"
                );
                let mut image =
                    gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, 80, 24).unwrap();
                let cr = gtk::cairo::Context::new(&image).unwrap();
                snapshot.to_node().unwrap().draw(&cr);
                drop(cr);
                let pixels = image.data().unwrap();
                let special = pixels
                    .chunks_exact(4)
                    .filter(|pixel| {
                        let rgb = u32::from_ne_bytes((*pixel).try_into().unwrap());
                        let green = (rgb >> 8) & 255;
                        // Cairo antialiases the diagonal undercurl; its green
                        // ink still dominates both white glyphs and the backdrop.
                        green > ((rgb >> 16) & 255) && green > (rgb & 255)
                    })
                    .count();
                assert_eq!(
                    special > 0,
                    decoration != 0,
                    "decoration lost for {text:?}, style {decoration}"
                );
                assert!(pixels.chunks_exact(4).any(|pixel| u32::from_ne_bytes(
                    pixel.try_into().unwrap()
                ) & 0xffffff
                    == 0x102030));
            }
        }
    }

    #[test]
    fn colors_keep_full_gui_rgb_and_inherit_before_reverse() {
        let normal = Vim9RenderStyle {
            foreground: Some(0x123456),
            background: Some(0x789abc),
            ..Default::default()
        };
        let syntax = Vim9RenderStyle {
            foreground: Some(0xd18cf7),
            special: Some(0x26a5d3),
            reverse: true,
            ..Default::default()
        };
        assert_eq!(colors(&syntax, &normal), (0x789abc, 0xd18cf7, 0x26a5d3));
        assert_eq!(
            colors(&Vim9RenderStyle::default(), &normal),
            (0x123456, 0x789abc, 0x123456)
        );
    }

    #[test]
    #[ignore = "requires GTK display; checks actual completion render pixels"]
    fn native_completion_elevation_preserves_rows_and_clips_to_widget() {
        gtk::init().expect("GTK display required");
        use jterm_core::vim9::Vim9RenderCompletion;

        let surface = BufferSurface::new();
        surface.measure(gtk::Orientation::Horizontal, -1);
        surface.measure(gtk::Orientation::Vertical, 96);
        surface.allocate(96, 72, -1, None);
        let state = PaintState {
            metrics: Metrics {
                origin_x: 0.0,
                origin_y: 0.0,
                cell_width: 8.0,
                line_height: 12.0,
            },
            ..Default::default()
        };
        let normal = Vim9RenderStyle {
            foreground: Some(0xe0e0e0),
            background: Some(0x406080),
            ..Default::default()
        };
        let selected = Vim9RenderStyle {
            background: Some(0x80a0c0),
            ..normal.clone()
        };
        let mut frame = Vim9RenderFrame {
            completion: Some(Vim9RenderCompletion {
                view: Vim9RenderRect {
                    row: 1,
                    column: 2,
                    rows: 2,
                    columns: 4,
                },
                default_style: normal,
                runs: vec![Vim9RenderRun {
                    row: 2,
                    column: 2,
                    cells: 4,
                    style: selected,
                    ..Default::default()
                }],
            }),
            ..Default::default()
        };
        // Use a larger image than the widget so out-of-widget shadow paint
        // is observable, and a bright underlying surface to measure blending.
        let render = |frame: &Vim9RenderFrame| {
            let snapshot = gtk::Snapshot::new();
            snapshot.append_color(
                &rgb(0xc0c0c0),
                &gtk::graphene::Rect::new(0.0, 0.0, 104.0, 80.0),
            );
            paint_completion(
                &surface,
                &snapshot,
                &state,
                frame,
                &mut LayoutCache::default(),
            );
            let mut image =
                gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, 104, 80).unwrap();
            let cr = gtk::cairo::Context::new(&image).unwrap();
            snapshot.to_node().unwrap().draw(&cr);
            drop(cr);
            let stride = image.stride() as usize;
            let bytes = image.data().unwrap().to_vec();
            (stride, bytes)
        };
        let pixel = |image: &(usize, Vec<u8>), x: usize, y: usize| {
            let index = y * image.0 + x * 4;
            u32::from_ne_bytes(image.1[index..index + 4].try_into().unwrap()) & 0xffffff
        };
        let image = render(&frame);
        assert_eq!(pixel(&image, 24, 18), 0x406080);
        assert_eq!(pixel(&image, 24, 30), 0x80a0c0);
        // The inside edge is one pixel; the next pixel retains Vim's color.
        assert_ne!(pixel(&image, 24, 12), 0x406080);
        assert_eq!(pixel(&image, 24, 13), 0x406080);
        assert!(pixel(&image, 14, 18) < 0xc0c0c0);
        assert_eq!(pixel(&image, 11, 18), 0xc0c0c0);
        assert_eq!(pixel(&image, 24, 6), 0xc0c0c0);

        // A menu flush with the lower right edge still fills the exact Vim
        // rectangle, while the elevation cannot paint outside the widget.
        frame.completion.as_mut().unwrap().view.row = 4;
        frame.completion.as_mut().unwrap().view.column = 8;
        frame.completion.as_mut().unwrap().runs.clear();
        let edge_image = render(&frame);
        assert_eq!(pixel(&edge_image, 72, 60), 0x406080);
        assert_eq!(pixel(&edge_image, 97, 60), 0xc0c0c0);
        assert_eq!(pixel(&edge_image, 72, 73), 0xc0c0c0);

        frame.completion = None;
        let closed = render(&frame);
        for (x, y) in [(14, 18), (24, 12), (24, 18), (72, 60)] {
            assert_eq!(pixel(&closed, x, y), 0xc0c0c0);
        }
    }

    #[test]
    #[ignore = "requires GTK display; checks virtual text property repaint and removal"]
    fn native_virtual_text_repaints_without_a_source_edit() {
        gtk::init().expect("GTK display required");
        let surface = BufferSurface::new();
        surface.measure(gtk::Orientation::Horizontal, -1);
        surface.measure(gtk::Orientation::Vertical, 200);
        surface.allocate(200, 80, -1, None);
        let mut frame = Vim9RenderFrame {
            revision: 1,
            supported: true,
            rows: 4,
            columns: 25,
            window: 10,
            buffer: 3,
            changedtick: 7,
            default_style: Vim9RenderStyle {
                foreground: Some(0xeeddaa),
                background: Some(0x182430),
                ..Default::default()
            },
            view: Vim9RenderRect {
                rows: 4,
                columns: 12,
                ..Default::default()
            },
            runs: vec![Vim9RenderRun {
                window: 10,
                text: "let".into(),
                cells: 3,
                line: 1,
                byte_end: 3,
                ..Default::default()
            }],
            ..Default::default()
        };
        frame.windows = vec![
            Vim9RenderWindow {
                window: 10,
                buffer: 3,
                changedtick: 7,
                view: frame.view.clone(),
                ..Default::default()
            },
            Vim9RenderWindow {
                window: 20,
                buffer: 4,
                view: Vim9RenderRect {
                    column: 13,
                    ..frame.view.clone()
                },
                ..Default::default()
            },
        ];
        surface.imp().state.borrow_mut().metrics = Metrics {
            origin_x: 0.0,
            origin_y: 0.0,
            cell_width: 8.0,
            line_height: 20.0,
        };
        let render = |frame: &Vim9RenderFrame| {
            surface.imp().state.borrow_mut().frame = Some(Arc::new(frame.clone()));
            let snapshot = gtk::Snapshot::new();
            surface.imp().snapshot(&snapshot);
            let mut image =
                gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, 200, 80).unwrap();
            let cr = gtk::cairo::Context::new(&image).unwrap();
            snapshot.to_node().unwrap().draw(&cr);
            drop(cr);
            let stride = image.stride() as usize;
            let bytes = image.data().unwrap().to_vec();
            (stride, bytes)
        };
        let pixel = |image: &(usize, Vec<u8>), x: usize, y: usize| {
            let index = y * image.0 + x * 4;
            u32::from_ne_bytes(image.1[index..index + 4].try_into().unwrap()) & 0xffffff
        };
        let baseline = render(&frame);
        assert_eq!(surface.imp().scene_builds.get(), 1);
        let source = frame.runs[0].clone();
        for (column, text, cells) in [(4, "参", 2), (6, "e\u{301}", 1)] {
            frame.runs.push(Vim9RenderRun {
                window: 10,
                column,
                text: text.into(),
                cells,
                style: Vim9RenderStyle {
                    foreground: Some(0x19c3a7),
                    background: Some(0x563412),
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        frame.revision += 1;
        let labeled = render(&frame);
        assert_eq!(surface.imp().scene_builds.get(), 2);
        assert_eq!(frame.changedtick, 7);
        assert_eq!(frame.runs[0], source);
        let label_background_pixels = |image: &(usize, Vec<u8>), color: u32| {
            (0..20)
                .flat_map(|y| (32..48).map(move |x| (x, y)))
                .filter(|(x, y)| pixel(image, *x, *y) == color)
                .count()
        };
        // Fallback CJK glyphs may cover any particular sample with antialiasing.
        // Inspect the label's complete two-cell rectangle instead of assuming
        // the first scanline is blank for every installed font.
        assert!(label_background_pixels(&labeled, 0x563412) > 0);
        assert_eq!(pixel(&labeled, 105, 10), pixel(&baseline, 105, 10));
        assert!(surface
            .imp()
            .layouts
            .borrow()
            .get("参", false, false)
            .is_some());
        assert!(surface
            .imp()
            .layouts
            .borrow()
            .get("e\u{301}", false, false)
            .is_some());
        assert!(frame.anchor_at_cell(0, 4).is_none());

        // An asynchronous property update leaves the source changedtick alone.
        // It must still retire the retained GTK text scene, including its RGB.
        frame.runs[1].style.background = Some(0x235671);
        frame.revision += 1;
        let recolored = render(&frame);
        assert_eq!(surface.imp().scene_builds.get(), 3);
        assert!(label_background_pixels(&recolored, 0x235671) > 0);
        assert_eq!(label_background_pixels(&recolored, 0x563412), 0);
        frame.runs.truncate(1);
        frame.revision += 1;
        let cleared = render(&frame);
        assert_eq!(surface.imp().scene_builds.get(), 4);
        assert_eq!(
            cleared, baseline,
            "removed labels must leave no retained paint"
        );
        frame.revision += 1;
        assert_eq!(render(&frame), baseline);
        assert_eq!(surface.imp().scene_builds.get(), 4);
    }

    #[test]
    #[ignore = "requires GTK display; checks inline hint paint, caret, IME and removal"]
    fn native_inline_hints_keep_source_caret_and_ime_geometry() {
        gtk::init().expect("GTK display required");
        let surface = BufferSurface::new();
        surface.measure(gtk::Orientation::Horizontal, -1);
        surface.measure(gtk::Orientation::Vertical, 360);
        surface.allocate(360, 120, -1, None);
        let normal = Vim9RenderStyle {
            foreground: Some(0xeeddaa),
            background: Some(0x182430),
            ..Default::default()
        };
        let source_style = Vim9RenderStyle {
            foreground: Some(0xabcde1),
            background: Some(0x384858),
            ..Default::default()
        };
        let mut frame = Vim9RenderFrame {
            revision: 1,
            supported: true,
            rows: 4,
            columns: 30,
            window: 10,
            buffer: 3,
            changedtick: 7,
            mode: "n".into(),
            default_style: normal.clone(),
            view: Vim9RenderRect {
                rows: 4,
                columns: 15,
                ..Default::default()
            },
            cursor: Vim9RenderCursor {
                column: 4,
                width: 1,
                visible: true,
                line: 1,
                byte_column: 4,
                appearance: Some(Default::default()),
                ..Default::default()
            },
            runs: vec![
                Vim9RenderRun {
                    window: 10,
                    text: "let ".into(),
                    cells: 4,
                    line: 1,
                    byte_end: 4,
                    style: normal.clone(),
                    ..Default::default()
                },
                Vim9RenderRun {
                    window: 10,
                    column: 4,
                    text: "value".into(),
                    cells: 5,
                    line: 1,
                    byte_start: 4,
                    byte_end: 9,
                    style: source_style.clone(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        frame.windows = vec![
            Vim9RenderWindow {
                window: 10,
                buffer: 3,
                changedtick: 7,
                view: frame.view.clone(),
                cursor: frame.cursor.clone(),
                topline: 1,
                botline: 1,
                line_count: 1,
                line_lengths: vec![9],
                native_pointer: true,
                native_scroll: true,
                ..Default::default()
            },
            Vim9RenderWindow {
                window: 20,
                buffer: 4,
                view: Vim9RenderRect {
                    column: 16,
                    columns: 14,
                    ..frame.view.clone()
                },
                ..Default::default()
            },
        ];
        let metrics = Metrics {
            origin_x: 0.0,
            origin_y: 0.0,
            cell_width: 8.0,
            line_height: 20.0,
        };
        {
            let mut state = surface.imp().state.borrow_mut();
            state.metrics = metrics;
            state.cursor_on = false;
        }
        let render = |frame: &Vim9RenderFrame| {
            surface.imp().state.borrow_mut().frame = Some(Arc::new(frame.clone()));
            let snapshot = gtk::Snapshot::new();
            surface.imp().snapshot(&snapshot);
            let mut image =
                gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, 360, 120).unwrap();
            let cr = gtk::cairo::Context::new(&image).unwrap();
            snapshot.to_node().unwrap().draw(&cr);
            drop(cr);
            let stride = image.stride() as usize;
            let bytes = image.data().unwrap().to_vec();
            (stride, bytes)
        };
        let pixel = |image: &(usize, Vec<u8>), x: usize, y: usize| {
            let index = y * image.0 + x * 4;
            u32::from_ne_bytes(image.1[index..index + 4].try_into().unwrap()) & 0xffffff
        };
        let count_color = |image: &(usize, Vec<u8>), left: usize, right: usize, color| {
            (0..20)
                .flat_map(|y| (left..right).map(move |x| (x, y)))
                .filter(|(x, y)| pixel(image, *x, *y) == color)
                .count()
        };
        let original = frame.clone();
        let baseline = render(&frame);
        assert_eq!(surface.imp().scene_builds.get(), 1);
        assert_eq!(surface.presented_cursor().unwrap().x, 32.0);

        // A provider changes display columns without inserting its hint into
        // the buffer. The right-hand source retains the original byte range.
        frame.runs[1].column = 7;
        for (column, text, cells) in [(4, "参", 2), (6, "e\u{301}", 1)] {
            frame.runs.push(Vim9RenderRun {
                window: 10,
                column,
                text: text.into(),
                cells,
                style: Vim9RenderStyle {
                    foreground: Some(0x19c3a7),
                    background: Some(0x563412),
                    italic: true,
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        frame.cursor.column = 7;
        frame.windows[0].cursor = frame.cursor.clone();
        frame.revision += 1;
        let labeled = render(&frame);
        assert_eq!(surface.imp().scene_builds.get(), 2);
        assert_eq!(frame.changedtick, original.changedtick);
        assert_eq!(frame.runs[0], original.runs[0]);
        assert_eq!(frame.runs[1].byte_start, 4);
        assert_eq!(frame.runs[1].byte_end, 9);
        assert_eq!(frame.runs[1].text, "value");
        assert!(frame.runs[2..]
            .iter()
            .all(|run| { run.line == 0 && run.byte_start == 0 && run.byte_end == 0 }));
        assert!(count_color(&labeled, 32, 56, 0x563412) > 0);
        assert!(count_color(&labeled, 56, 96, 0x384858) > 0);
        assert_eq!(pixel(&labeled, 130, 10), pixel(&baseline, 130, 10));
        assert_eq!(surface.presented_cursor().unwrap().x, 56.0);
        assert_eq!(surface.projected_cursor(), surface.presented_cursor());
        assert_eq!(cursor_under_style(&frame), &source_style);
        assert_eq!(surface.source_at(60.0, 10.0).unwrap().byte_column, 4);
        assert!(frame.anchor_at_cell(0, 4).is_none());

        // Cursor ink must use the shifted source's syntax palette, not the
        // adjacent hint palette. Transient caret/IME paint reuses the scene.
        {
            let mut state = surface.imp().state.borrow_mut();
            state.cursor_on = true;
            state.focused = true;
        }
        let caret = render(&frame);
        assert!(count_color(&caret, 56, 64, 0xabcde1) > 0);
        assert_eq!(surface.imp().scene_builds.get(), 2);
        surface.imp().state.borrow_mut().cursor_on = false;

        // The GUI cell geometry changes on font zoom. Both the painted caret
        // and the native input-method candidate rectangle move with it.
        surface.imp().state.borrow_mut().metrics = Metrics {
            cell_width: 10.0,
            line_height: 24.0,
            ..metrics
        };
        assert!(surface.presented_cursor().is_none());
        render(&frame);
        let zoomed_cursor = surface.presented_cursor().unwrap();
        assert_eq!(
            (zoomed_cursor.x, zoomed_cursor.width, zoomed_cursor.height),
            (70.0, 10.0, 24.0)
        );
        assert_eq!(surface.source_at(75.0, 10.0).unwrap().byte_column, 4);
        assert_eq!(surface.imp().scene_builds.get(), 3);
        surface.imp().state.borrow_mut().preedit = Some(Preedit {
            text: "文".into(),
            attributes: pango::AttrList::new(),
            cursor: 0,
        });
        let composed = render(&frame);
        assert_eq!(pixel(&composed, 70, 2), 0xeeddaa);
        assert_eq!(surface.presented_cursor(), Some(zoomed_cursor));
        assert_eq!(surface.imp().scene_builds.get(), 3);

        // Removing the provider's properties restores both source placement
        // and every pixel, although no source generation has changed.
        {
            let mut state = surface.imp().state.borrow_mut();
            state.metrics = metrics;
            state.preedit = None;
            state.focused = false;
        }
        frame = original;
        frame.revision += 10;
        assert!(
            render(&frame) == baseline,
            "cleared hints must restore every pixel"
        );
        assert_eq!(surface.presented_cursor().unwrap().x, 32.0);
        assert_eq!(surface.imp().scene_builds.get(), 4);
        frame.revision += 1;
        assert!(
            render(&frame) == baseline,
            "cleared hints must restore every pixel"
        );
        assert_eq!(surface.imp().scene_builds.get(), 4);
    }
    #[test]
    #[ignore = "requires GTK display; checks documentation card pixels"]
    fn documentation_card_rounds_corners_without_moving_popup_geometry() {
        gtk::init().expect("GTK display required");
        let surface = BufferSurface::new();
        surface.allocate(160, 120, -1, None);
        let state = PaintState {
            metrics: Metrics {
                origin_x: 0.0,
                origin_y: 0.0,
                cell_width: 10.0,
                line_height: 20.0,
            },
            ..Default::default()
        };
        let popup: Vim9RenderPopup = serde_json::from_value(serde_json::json!({
            "id":5,"kind":"hover","zindex":50,
            "view":{"row":1,"column":2,"rows":3,"columns":8},
            "default_style":{"background":0x123456,"foreground":0xddeeff},
            "runs":[{"row":1,"column":2,"cells":8,"text":""}]
        }))
        .unwrap();
        let render = |popup: &Vim9RenderPopup| {
            let snapshot = gtk::Snapshot::new();
            snapshot.append_color(
                &rgb(0xffffff),
                &gtk::graphene::Rect::new(0.0, 0.0, 160.0, 120.0),
            );
            paint_popup(
                &surface,
                &snapshot,
                &state,
                &Vim9RenderFrame::default(),
                popup,
                &mut LayoutCache::default(),
            );
            let mut image =
                gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, 160, 120).unwrap();
            let cr = gtk::cairo::Context::new(&image).unwrap();
            snapshot.to_node().unwrap().draw(&cr);
            drop(cr);
            let stride = image.stride() as usize;
            let bytes = image.data().unwrap().to_vec();
            (stride, bytes)
        };
        let mut plain = popup.clone();
        plain.kind.clear();
        let card = render(&popup);
        let flat = render(&plain);
        let pixel = |image: &(usize, Vec<u8>), x: usize, y: usize| {
            image.1[y * image.0 + x * 4..y * image.0 + x * 4 + 3].to_vec()
        };
        assert_ne!(
            pixel(&card, 20, 20),
            pixel(&flat, 20, 20),
            "card corner must reveal the underlying surface"
        );
        assert_eq!(
            pixel(&card, 50, 50),
            pixel(&flat, 50, 50),
            "opaque body retains the Vim GUI background"
        );
        assert_ne!(
            pixel(&card, 50, 82),
            pixel(&flat, 50, 82),
            "card shadow stays outside content geometry"
        );
        assert_eq!(popup.view, plain.view);
        assert_eq!(popup.runs, plain.runs);
    }

    #[test]
    #[ignore = "requires GTK display; checks popup layering and opaque render pixels"]
    fn native_popups_cover_split_gaps_and_obey_vim_zindex() {
        gtk::init().expect("GTK display required");
        use jterm_core::vim9::Vim9RenderCompletion;

        let surface = BufferSurface::new();
        surface.measure(gtk::Orientation::Horizontal, -1);
        surface.measure(gtk::Orientation::Vertical, 96);
        surface.allocate(96, 72, -1, None);
        let state = PaintState {
            metrics: Metrics {
                origin_x: 0.0,
                origin_y: 0.0,
                cell_width: 8.0,
                line_height: 12.0,
            },
            ..Default::default()
        };
        let style = |background| Vim9RenderStyle {
            foreground: Some(0xe0e0e0),
            background: Some(background),
            ..Default::default()
        };
        let upper = Vim9RenderPopup {
            kind: String::new(),
            id: 20,
            zindex: 100,
            view: Vim9RenderRect {
                row: 2,
                column: 4,
                rows: 3,
                columns: 5,
            },
            default_style: style(0xa060c0),
            runs: vec![Vim9RenderRun {
                row: 2,
                column: 4,
                cells: 5,
                style: style(0xd03040),
                ..Default::default()
            }],
        };
        let mut newer = upper.clone();
        newer.id = 21;
        newer.default_style = style(0x602080);
        newer.runs.clear();
        let mut frame = Vim9RenderFrame {
            // Two splits leave column 4 and row 3 Vim-owned. The popup must
            // still fill both gaps; clipping to buffer views would leak PTY.
            windows: vec![
                Vim9RenderWindow {
                    view: Vim9RenderRect {
                        row: 0,
                        column: 0,
                        rows: 3,
                        columns: 4,
                    },
                    ..Default::default()
                },
                Vim9RenderWindow {
                    view: Vim9RenderRect {
                        row: 0,
                        column: 5,
                        rows: 3,
                        columns: 7,
                    },
                    ..Default::default()
                },
            ],
            completion: Some(Vim9RenderCompletion {
                view: Vim9RenderRect {
                    row: 1,
                    column: 2,
                    rows: 3,
                    columns: 5,
                },
                default_style: style(0x204080),
                ..Default::default()
            }),
            // Deliberately unsorted so host ordering is independently tested.
            popups: vec![
                upper,
                Vim9RenderPopup {
                    id: 10,
                    zindex: 50,
                    view: Vim9RenderRect {
                        row: 0,
                        column: 1,
                        rows: 4,
                        columns: 9,
                    },
                    default_style: style(0x246824),
                    ..Default::default()
                },
                newer,
            ],
            ..Default::default()
        };
        let render = |frame: &Vim9RenderFrame| {
            let snapshot = gtk::Snapshot::new();
            snapshot.append_color(
                &rgb(0xc0c0c0),
                &gtk::graphene::Rect::new(0.0, 0.0, 104.0, 80.0),
            );
            paint_overlays(
                &surface,
                &snapshot,
                &state,
                frame,
                &mut LayoutCache::default(),
            );
            let mut image =
                gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, 104, 80).unwrap();
            let cr = gtk::cairo::Context::new(&image).unwrap();
            snapshot.to_node().unwrap().draw(&cr);
            drop(cr);
            let stride = image.stride() as usize;
            let bytes = image.data().unwrap().to_vec();
            (stride, bytes)
        };
        let pixel = |image: &(usize, Vec<u8>), x: usize, y: usize| {
            let index = y * image.0 + x * 4;
            u32::from_ne_bytes(image.1[index..index + 4].try_into().unwrap()) & 0xffffff
        };
        let image = render(&frame);
        assert_eq!(pixel(&image, 12, 6), 0x246824, "low popup is opaque");
        assert_eq!(pixel(&image, 24, 18), 0x204080, "menu covers z-index 50");
        assert_eq!(
            pixel(&image, 40, 30),
            0xd03040,
            "popup border run keeps GUI color"
        );
        assert_eq!(
            pixel(&image, 36, 42),
            0xa060c0,
            "popup covers split/status gaps"
        );
        assert_eq!(
            pixel(&image, 40, 42),
            0xa060c0,
            "z-index 100 and older id cover menu and equal-zindex popup"
        );
        assert_eq!(
            pixel(&image, 4, 6),
            0xc0c0c0,
            "popup does not invent a host border"
        );

        frame.completion = None;
        frame.popups.truncate(1);
        frame.popups[0].view = Vim9RenderRect {
            row: 4,
            column: 9,
            rows: 2,
            columns: 3,
        };
        frame.popups[0].runs.clear();
        let edge = render(&frame);
        assert_eq!(pixel(&edge, 80, 60), 0xa060c0);
        assert_eq!(
            pixel(&edge, 97, 60),
            0xc0c0c0,
            "widget clipping is preserved"
        );
        assert_eq!(pixel(&edge, 80, 73), 0xc0c0c0);
        frame.popups.clear();
        let closed = render(&frame);
        assert_eq!(
            pixel(&closed, 80, 60),
            0xc0c0c0,
            "closed popup leaves no stale paint"
        );
    }

    #[test]
    #[ignore = "requires GTK display; checks popup input remains Vim-owned"]
    fn native_popup_input_never_targets_underlying_source() {
        gtk::init().expect("GTK display required");
        let surface = BufferSurface::new();
        let metrics = Metrics {
            origin_x: 0.0,
            origin_y: 0.0,
            cell_width: 8.0,
            line_height: 12.0,
        };
        let mut frame = Vim9RenderFrame {
            supported: true,
            window: 10,
            buffer: 1,
            mode: "n".into(),
            windows: vec![Vim9RenderWindow {
                window: 10,
                buffer: 1,
                native_pointer: true,
                native_scroll: true,
                view: Vim9RenderRect {
                    row: 0,
                    column: 0,
                    rows: 4,
                    columns: 8,
                },
                ..Default::default()
            }],
            runs: vec![Vim9RenderRun {
                window: 10,
                row: 0,
                column: 0,
                line: 1,
                byte_start: 0,
                byte_end: 4,
                text: "text".into(),
                cells: 4,
                ..Default::default()
            }],
            ..Default::default()
        };
        let shown = Arc::new(frame.clone());
        surface.imp().state.borrow_mut().frame = Some(shown.clone());
        surface.imp().state.borrow_mut().metrics = metrics;
        *surface.imp().presented.borrow_mut() = Some(PresentedFrame {
            frame: shown,
            metrics,
            offsets: HashMap::new(),
            precision_windows: Vec::new(),
            cursor: None,
        });
        let source = surface.source_at(4.0, 6.0).unwrap();
        *surface.imp().source_gesture.borrow_mut() = Some(SourceGesture {
            gesture: 1,
            request: 1,
            last: source,
            pending: Some((source, Vim9PointerPhase::Drag)),
            released: false,
        });
        surface
            .imp()
            .scroll_input
            .borrow_mut()
            .pending
            .insert(10, 3);
        frame.popups.push(Vim9RenderPopup {
            id: 12,
            zindex: 50,
            view: Vim9RenderRect {
                row: 0,
                column: 0,
                rows: 2,
                columns: 4,
            },
            ..Default::default()
        });
        let popup_frame = Arc::new(frame);
        surface.imp().state.borrow_mut().frame = Some(popup_frame.clone());
        assert!(surface.overlays_visible());
        assert!(surface.source_at(4.0, 6.0).is_none());
        assert!(!surface.enqueue_scroll(4.0, 6.0, 0.5, gdk::ScrollUnit::Surface));
        assert!(surface.imp().source_gesture.borrow().is_none());
        assert!(surface.imp().scroll_input.borrow().pending.is_empty());
        assert!(!surface.queue_source_pointer(4.0, 6.0, Vim9PointerPhase::Press));
        // The already-presented popup retains input ownership until GTK has
        // painted its disappearance, even if a newer frame has no popups.
        surface.imp().presented.borrow_mut().as_mut().unwrap().frame = popup_frame;
        surface.imp().state.borrow_mut().frame = Some(Arc::new(Vim9RenderFrame::default()));
        assert!(surface.overlays_visible());
        assert!(!surface.enqueue_scroll(4.0, 6.0, 1.0, gdk::ScrollUnit::Wheel));
    }

    #[test]
    #[ignore = "requires GTK display and Vim; exercised by the native pixel integration CI"]
    fn native_pixel_snapshot_and_source_hit_round_trip() {
        gtk::init().expect("GTK display required");
        use jterm_core::vim9::{
            Vim9RenderCursor, Vim9RenderMarginRow, Vim9RenderRun, Vim9RenderWindow,
        };
        let surface = BufferSurface::new();
        let metrics = Metrics {
            origin_x: 0.0,
            origin_y: 0.0,
            cell_width: 8.0,
            line_height: 20.0,
        };
        let cursor = Vim9RenderCursor {
            appearance: None,
            row: 1,
            column: 0,
            width: 1,
            visible: true,
            line: 11,
            byte_column: 0,
        };
        let region = Vim9RenderRect {
            row: 0,
            column: 0,
            rows: 4,
            columns: 8,
        };
        let run = |row, line, text: &str| Vim9RenderRun {
            window: 10,
            row,
            column: 0,
            cells: text.len(),
            text: text.into(),
            line,
            byte_start: 0,
            byte_end: text.len(),
            ..Default::default()
        };
        let frame = Arc::new(Vim9RenderFrame {
            revision: 1,
            supported: true,
            rows: 6,
            columns: 8,
            window: 10,
            buffer: 1,
            changedtick: 3,
            mode: "n".into(),
            view: region.clone(),
            cursor: cursor.clone(),
            windows: vec![Vim9RenderWindow {
                window: 10,
                buffer: 1,
                changedtick: 3,
                view: region,
                cursor,
                topline: 10,
                botline: 13,
                line_count: 100,
                native_pointer: true,
                native_scroll: true,
                line_lengths: vec![2, 2, 2, 0],
                overscan: vec![
                    Vim9RenderMarginRow {
                        row: -1,
                        line: 9,
                        byte_length: 3,
                        runs: vec![run(0, 9, "pre")],
                    },
                    Vim9RenderMarginRow {
                        row: 4,
                        line: 14,
                        byte_length: 4,
                        runs: vec![run(0, 14, "post")],
                    },
                ],
                ..Default::default()
            }],
            runs: vec![run(0, 10, "AB"), run(1, 11, "CD"), run(2, 12, "EF")],
            ..Default::default()
        });
        {
            let mut state = surface.imp().state.borrow_mut();
            state.metrics = metrics;
            state.frame = Some(frame.clone());
        }
        surface.imp().pixel_enabled.set(true);
        // A real GTK snapshot must submit each supplied logical pixel, even
        // while Vim's source viewport has not moved by a whole line.
        for (delta, expected) in [(1.0, -1.0), (0.25, -1.25)] {
            assert!(surface.enqueue_scroll(8.0, 10.0, delta, gdk::ScrollUnit::Surface));
            let snapshot = gtk::Snapshot::new();
            surface.imp().snapshot(&snapshot);
            assert!(snapshot.to_node().is_some());
            let presented = surface.imp().presented.borrow();
            let presented = presented.as_ref().unwrap();
            assert_eq!(presented.offsets[&10], expected);
            assert_eq!(presented.frame.windows[0].topline, 10);
            assert_eq!(presented.cursor.unwrap().y, 20.0 + expected);
        }
        assert!(surface.enqueue_scroll(8.0, 10.0, 5.0, gdk::ScrollUnit::Surface));
        assert_eq!(
            surface.presentation_offset(&surface.imp().state.borrow(), &frame, 10),
            -6.25
        );
        assert_eq!(surface.projected_cursor().unwrap().y, 13.75);
        assert_eq!(surface.presented_cursor().unwrap().y, 18.75);
        // Execute the actual GTK snapshot/Pango path, including a partly
        // exposed semantic margin. No GPU/hardware latency is asserted here.
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_some());
        assert_eq!(surface.presented_cursor().unwrap().y, 13.75);
        assert_eq!(surface.source_at(9.0, 75.0).unwrap().line, 14);
        assert_eq!(surface.source_at(9.0, 75.0).unwrap().byte_column, 1);
        assert_eq!(
            surface.direct_source_at(9.0, 75.0),
            surface.source_at(9.0, 75.0)
        );
        assert_eq!(surface.source_at(45.0, 65.0).unwrap().line, 13);
        assert!(surface.source_at(65.0, 75.0).is_none());
        let mut visual = (*frame).clone();
        visual.mode = "v".into();
        visual.windows[0].native_scroll = false;
        {
            let mut pixels = surface.imp().pixels.borrow_mut();
            let scroll = pixels.get_mut(&10).unwrap();
            scroll.stop_animation();
            assert!(scroll.reconcile(&visual.windows[0], metrics, false));
        }
        if !jterm_core::child_env::inherited_environment_is_captured() {
            jterm_core::child_env::capture_inherited_environment().unwrap();
        }
        let session = Vim9Session::start(jterm_core::vim9::Vim9EntryRequest::user_action(
            std::env::temp_dir(),
        ))
        .unwrap();
        let visual = Some(Arc::new(visual));
        assert!(surface.imp().source_gesture.borrow().is_none());
        // Exercise a polling tick after the release transaction has retired.
        // Visual selection must keep its fractional image and hit mapping.
        surface.advance_native_input(
            &session,
            &Vim9InputSnapshot::default(),
            &visual,
            metrics,
            true,
        );
        surface.imp().state.borrow_mut().frame = visual;
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_some());
        assert_eq!(surface.scroll_offset(10), -6.25);
        assert_eq!(surface.source_at(9.0, 75.0).unwrap().line, 14);
        surface.imp().state.borrow_mut().frame = Some(frame.clone());
        assert!(!surface.should_settle_press(9.0, 75.0, 1, 1, false));
        for (button, count, modified) in [(3, 1, false), (1, 2, false), (1, 1, true)] {
            assert!(surface.should_settle_press(9.0, 75.0, button, count, modified));
        }
        // Input must still hit the submitted image when a new frame is
        // received before GTK has submitted its replacement snapshot.
        let mut received = (*frame).clone();
        received.revision += 1;
        received.windows[0].topline += 1;
        received.windows[0].botline += 1;
        received.cursor.row += 1;
        received.windows[0].cursor.row += 1;
        surface.imp().state.borrow_mut().frame = Some(Arc::new(received));
        assert_eq!(surface.source_at(9.0, 75.0).unwrap().line, 14);
        assert_eq!(surface.presented_cursor().unwrap().y, 13.75);
        assert_ne!(surface.projected_cursor(), surface.presented_cursor());
        assert!(
            !surface.stale_scroll_source_at(9.0, 75.0),
            "a valid viewport acknowledgement may change topline before paint"
        );
        surface.imp().state.borrow_mut().frame = Some(frame.clone());

        // A lost release must not turn later hover movement into a drag. End
        // at the last accepted source position even when hovering elsewhere.
        let last = surface.source_at(9.0, 75.0).unwrap();
        *surface.imp().source_gesture.borrow_mut() = Some(SourceGesture {
            gesture: 1,
            request: 1,
            last,
            pending: None,
            released: false,
        });
        surface.source_motion(9.0, 10.0, false);
        {
            let gesture = surface.imp().source_gesture.borrow();
            let gesture = gesture.as_ref().unwrap();
            assert!(gesture.released);
            assert_eq!(gesture.last, last);
            assert_eq!(gesture.pending, Some((last, Vim9PointerPhase::Release)));
        }
        surface.source_motion(9.0, 30.0, true);
        assert_eq!(
            surface.imp().source_gesture.borrow().as_ref().unwrap().last,
            last
        );
        surface.cancel_scroll();
        assert!(surface.imp().pixels.borrow().is_empty());
        assert!(surface.imp().source_gesture.borrow().is_none());
        assert_eq!(surface.presented_cursor().unwrap().y, 13.75);
        assert_eq!(
            surface.direct_source_at(9.0, 75.0),
            Some(last),
            "retiring input state must not remap an image GTK still displays"
        );
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_some());
        assert_eq!(surface.source_at(9.0, 75.0).unwrap().line, 13);
        assert_eq!(surface.presented_cursor().unwrap().y, 20.0);
        assert!(
            surface.direct_source_at(9.0, 75.0).is_none(),
            "stationary input remains Vim-owned"
        );
        assert!(!surface.stale_precision_at(9.0, 75.0));
        for (button, count, modified) in [(3, 1, false), (1, 2, false), (1, 1, true)] {
            assert!(!surface.should_settle_press(9.0, 75.0, button, count, modified));
        }
        assert!(surface.enqueue_scroll(8.0, 10.0, 6.25, gdk::ScrollUnit::Surface));
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_some());
        let mut edited = (*frame).clone();
        edited.windows[0].changedtick += 1;
        surface.imp().state.borrow_mut().frame = Some(Arc::new(edited));
        assert!(surface.source_at(9.0, 75.0).is_none());
        assert!(surface.stale_precision_at(9.0, 75.0));
        assert!(surface.should_settle_press(9.0, 75.0, 1, 1, false));
        for unit in [gdk::ScrollUnit::Surface, gdk::ScrollUnit::Wheel] {
            assert!(surface.enqueue_scroll(9.0, 75.0, 1.0, unit));
            assert!(surface.imp().pixels.borrow().is_empty());
            assert!(surface.imp().scroll_input.borrow().pending.is_empty());
        }
        let mut replaced = (*frame).clone();
        replaced.windows[0].buffer += 1;
        surface.imp().state.borrow_mut().frame = Some(Arc::new(replaced));
        assert!(surface.stale_scroll_source_at(9.0, 75.0));
        let mut resized = (*frame).clone();
        resized.windows[0].view.columns -= 1;
        surface.imp().state.borrow_mut().frame = Some(Arc::new(resized));
        assert!(surface.stale_scroll_source_at(9.0, 75.0));
        assert!(
            surface.stale_precision_at(9.0, 75.0),
            "cancel has not replaced the visible image"
        );
        surface.imp().state.borrow_mut().frame = None;
        assert!(
            surface.stale_precision_at(9.0, 75.0),
            "an inactive pending frame still has an old visible image"
        );
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_none());
        assert!(!surface.stale_precision_at(9.0, 75.0));

        // A reversal while a bounded scroll request is in flight must fit
        // both source images. This fixture has only one real margin row:
        // receiving the acknowledgement may not snap its visible text/caret.
        surface.cancel_scroll();
        surface.imp().state.borrow_mut().frame = Some(frame.clone());
        assert!(surface.enqueue_scroll(8.0, 10.0, 200.0, gdk::ScrollUnit::Surface));
        {
            let mut pixels = surface.imp().pixels.borrow_mut();
            let scroll = pixels.get_mut(&10).unwrap();
            assert!(matches!(
                scroll.command(),
                Some(Vim9Command::ScrollView { lines: 1, .. })
            ));
            scroll.submitted(88);
        }
        assert!(surface.enqueue_scroll(8.0, 10.0, -200.0, gdk::ScrollUnit::Surface));
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_some());
        let before_hit = surface.source_at(9.0, 10.0).unwrap();
        let before_caret = surface.presented_cursor().unwrap();
        assert_eq!(before_hit.line, 10);

        let mut acknowledged = (*frame).clone();
        acknowledged.revision += 1;
        acknowledged.cursor.row = 0;
        let window = &mut acknowledged.windows[0];
        window.topline = 11;
        window.botline = 14;
        window.scroll_ack = 88;
        window.cursor.row = 0;
        window.line_lengths = vec![2, 2, 0, 4];
        window.overscan = vec![
            Vim9RenderMarginRow {
                row: -1,
                line: 10,
                byte_length: 2,
                runs: vec![run(0, 10, "AB")],
            },
            Vim9RenderMarginRow {
                row: 4,
                line: 15,
                byte_length: 4,
                runs: vec![run(0, 15, "next")],
            },
        ];
        acknowledged.runs = vec![run(0, 11, "CD"), run(1, 12, "EF"), run(3, 14, "post")];
        assert!(surface
            .imp()
            .pixels
            .borrow_mut()
            .get_mut(&10)
            .unwrap()
            .reconcile(window, metrics, false));
        surface.imp().state.borrow_mut().frame = Some(Arc::new(acknowledged));
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_some());
        assert_eq!(surface.source_at(9.0, 10.0), Some(before_hit));
        assert_eq!(surface.presented_cursor(), Some(before_caret));

        // Scroll eligibility is independent of Vim's mouse ownership. A
        // custom mapping/virtualedit or Insert mode must not quantize raw
        // device pixels, including when wheel animations are disabled.
        surface.imp().wheel_animation.set(false);
        for mode in ["n", "i"] {
            surface.cancel_scroll();
            let mut scroll_only = (*frame).clone();
            scroll_only.mode = mode.into();
            scroll_only.windows[0].native_pointer = false;
            surface.imp().state.borrow_mut().frame = Some(Arc::new(scroll_only));
            assert!(surface.enqueue_scroll(8.0, 10.0, 1.25, gdk::ScrollUnit::Surface));
            let snapshot = gtk::Snapshot::new();
            surface.imp().snapshot(&snapshot);
            assert!(snapshot.to_node().is_some());
            assert_eq!(
                surface.imp().presented.borrow().as_ref().unwrap().offsets[&10],
                -1.25
            );
            assert!(surface.direct_source_at(9.0, 10.0).is_none());
            assert!(surface.should_settle_press(9.0, 10.0, 1, 1, false));
            assert!(surface.enqueue_scroll(8.0, 10.0, 0.25, gdk::ScrollUnit::Wheel));
            assert_eq!(surface.scroll_offset(10), -16.25);
            assert!(!surface.imp().pixels.borrow()[&10].is_animating());
        }
        surface.imp().wheel_animation.set(true);
        for (delta, smooth_event) in [(0.125, false), (1.0, true)] {
            surface.cancel_scroll();
            surface.imp().state.borrow_mut().frame = Some(frame.clone());
            assert!(surface.enqueue_scroll_input(
                8.0,
                10.0,
                delta,
                gdk::ScrollUnit::Wheel,
                smooth_event
            ));
            assert_eq!(surface.scroll_offset(10), -(delta * 3.0 * 20.0).min(20.0));
            assert!(!surface.imp().pixels.borrow()[&10].is_animating());
        }
        // A normal source window can turn into display-only help/quickfix
        // without resizing its split. No queued native transaction survives.
        surface.cancel_scroll();
        surface.imp().state.borrow_mut().frame = Some(frame.clone());
        assert!(surface.enqueue_scroll(8.0, 10.0, 1.25, gdk::ScrollUnit::Surface));
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_some());
        assert!(surface.precision_image_at(8.0, 10.0));
        {
            let mut input = surface.imp().scroll_input.borrow_mut();
            input.pending.insert(10, 3);
            input.requested.insert(10, 1.0);
            input.fractions.add(10, 0.6);
        }
        surface
            .imp()
            .motions
            .borrow_mut()
            .insert(10, WindowMotion::default());
        *surface.imp().source_gesture.borrow_mut() = Some(SourceGesture {
            gesture: 999,
            request: 999,
            last,
            pending: None,
            released: false,
        });
        let mut render_only = (*frame).clone();
        render_only.windows[0].native_pointer = false;
        render_only.windows[0].native_scroll = false;
        let render_only = Some(Arc::new(render_only));
        surface.advance_native_input(
            &session,
            &Vim9InputSnapshot::default(),
            &render_only,
            metrics,
            true,
        );
        assert!(surface.imp().pixels.borrow().is_empty());
        assert!(surface.imp().source_gesture.borrow().is_none());
        assert!(surface.imp().motions.borrow().is_empty());
        assert!(surface.imp().scroll_input.borrow().pending.is_empty());
        assert!(surface.imp().scroll_input.borrow().requested.is_empty());
        surface.imp().state.borrow_mut().frame = render_only;
        for unit in [gdk::ScrollUnit::Surface, gdk::ScrollUnit::Wheel] {
            assert!(
                surface.enqueue_scroll(8.0, 10.0, 1.0, unit),
                "the still-presented fractional image must settle before PTY handoff"
            );
            assert!(surface.imp().scroll_input.borrow().pending.is_empty());
        }
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_some());
        for unit in [gdk::ScrollUnit::Surface, gdk::ScrollUnit::Wheel] {
            assert!(
                !surface.enqueue_scroll(8.0, 10.0, 1.0, unit),
                "VTE owns render-only wheel events after the new frame is submitted"
            );
        }
        assert!(
            !surface.should_settle_press(8.0, 10.0, 1, 2, false),
            "a stationary render-only double click remains Vim-owned"
        );
        assert!(surface.direct_source_at(8.0, 10.0).is_none());
    }
}
