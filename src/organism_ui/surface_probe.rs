//! Test-only real X11 integer-scale surface readback. Never call from production code.
//! This does not validate Wayland fractional scaling.
use gtk::prelude::*;
use relm4::gtk;
use std::cell::Cell;
use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CStr, CString};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

type Display = *mut c_void;
type Image = *mut c_void;
type Handler = unsafe extern "C" fn(Display, *mut XError) -> c_int;

// ABI fields checked against X11/Xlib.h. No assumptions about XImage pixel order.
#[repr(C)]
struct XError {
    type_: c_int,
    display: Display,
    resource_id: c_ulong,
    serial: c_ulong,
    error_code: u8,
    request_code: u8,
    minor_code: u8,
}

// Only the documented prefix through the RGB masks is accessed.
#[repr(C)]
struct ImagePrefix {
    width: c_int,
    height: c_int,
    xoffset: c_int,
    format: c_int,
    data: *mut c_char,
    byte_order: c_int,
    bitmap_unit: c_int,
    bitmap_bit_order: c_int,
    bitmap_pad: c_int,
    depth: c_int,
    bytes_per_line: c_int,
    bits_per_pixel: c_int,
    red_mask: c_ulong,
    green_mask: c_ulong,
    blue_mask: c_ulong,
}

static ERROR_LOCK: Mutex<()> = Mutex::new(());
static ERROR_DISPLAY: AtomicUsize = AtomicUsize::new(0);
static ERROR_CODE: AtomicUsize = AtomicUsize::new(0);
static PREVIOUS_HANDLER: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn on_error(display: Display, event: *mut XError) -> c_int {
    if display as usize == ERROR_DISPLAY.load(Ordering::SeqCst) && !event.is_null() {
        let event = unsafe { &*event };
        let code = usize::from(event.error_code)
            | (usize::from(event.request_code) << 8)
            | (usize::from(event.minor_code) << 16);
        let _ = ERROR_CODE.compare_exchange(0, code, Ordering::SeqCst, Ordering::SeqCst);
        return 0;
    }
    let previous = PREVIOUS_HANDLER.load(Ordering::SeqCst);
    if previous != 0 {
        let previous: Handler = unsafe { std::mem::transmute(previous) };
        return unsafe { previous(display, event) };
    }
    0
}

struct Library(*mut c_void);
impl Library {
    fn open() -> Result<Self, String> {
        let ptr = unsafe { libc::dlopen(c"libX11.so.6".as_ptr(), libc::RTLD_NOW) };
        if ptr.is_null() {
            Err("libX11.so.6 is required for the X11 fixture".into())
        } else {
            Ok(Self(ptr))
        }
    }
    unsafe fn symbol<T: Copy>(&self, name: &CStr) -> Result<T, String> {
        let ptr = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        if ptr.is_null() {
            return Err(format!("missing Xlib symbol {}", name.to_string_lossy()));
        }
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of_val(&ptr));
        Ok(unsafe { std::mem::transmute_copy(&ptr) })
    }
}
impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            libc::dlclose(self.0);
        }
    }
}

struct Api {
    open: unsafe extern "C" fn(*const c_char) -> Display,
    close: unsafe extern "C" fn(Display) -> c_int,
    sync: unsafe extern "C" fn(Display, c_int) -> c_int,
    set_handler: unsafe extern "C" fn(Option<Handler>) -> Option<Handler>,
    get_image: unsafe extern "C" fn(
        Display,
        c_ulong,
        c_int,
        c_int,
        c_uint,
        c_uint,
        c_ulong,
        c_int,
    ) -> Image,
    get_pixel: unsafe extern "C" fn(Image, c_int, c_int) -> c_ulong,
    destroy_image: unsafe extern "C" fn(Image) -> c_int,
    clear_area:
        unsafe extern "C" fn(Display, c_ulong, c_int, c_int, c_uint, c_uint, c_int) -> c_int,
    get_geometry: unsafe extern "C" fn(
        Display,
        c_ulong,
        *mut c_ulong,
        *mut c_int,
        *mut c_int,
        *mut c_uint,
        *mut c_uint,
        *mut c_uint,
        *mut c_uint,
    ) -> c_int,
}

struct ErrorGuard<'a> {
    api: &'a Api,
    previous: Option<Handler>,
    _lock: std::sync::MutexGuard<'static, ()>,
}
impl<'a> ErrorGuard<'a> {
    fn new(api: &'a Api, display: Display) -> Self {
        let lock = ERROR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ERROR_DISPLAY.store(display as usize, Ordering::SeqCst);
        ERROR_CODE.store(0, Ordering::SeqCst);
        let previous = unsafe { (api.set_handler)(Some(on_error)) };
        PREVIOUS_HANDLER.store(previous.map_or(0, |f| f as usize), Ordering::SeqCst);
        Self {
            api,
            previous,
            _lock: lock,
        }
    }
    fn result(&self, operation: &str) -> Result<(), String> {
        let code = ERROR_CODE.load(Ordering::SeqCst);
        if code == 0 {
            Ok(())
        } else {
            Err(format!(
                "{operation}: X11 error={} request={} minor={}",
                code & 255,
                (code >> 8) & 255,
                (code >> 16) & 255
            ))
        }
    }
}
impl Drop for ErrorGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.api.set_handler)(self.previous);
        }
        ERROR_DISPLAY.store(0, Ordering::SeqCst);
        PREVIOUS_HANDLER.store(0, Ordering::SeqCst);
    }
}

struct OwnedImage<'a> {
    image: Image,
    api: &'a Api,
}
impl Drop for OwnedImage<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.api.destroy_image)(self.image);
        }
    }
}

// BEGIN PURE SURFACE RECT
/// GTK snapshots the native widget after translating by +surface_transform.
/// Convert the old/new logical envelope to outward-rounded physical pixels.
pub fn surface_rect(
    old: [f64; 4],
    new: [f64; 4],
    offset: (f64, f64),
    scale: i32,
    size: (u32, u32),
) -> [i32; 4] {
    let s = f64::from(scale);
    let left = ((old[0].min(new[0]) + offset.0 - 3.0) * s).floor().max(0.0) as i32;
    let top = ((old[1].min(new[1]) + offset.1 - 3.0) * s).floor().max(0.0) as i32;
    let right = (((old[0] + old[2]).max(new[0] + new[2]) + offset.0 + 3.0) * s)
        .ceil()
        .min(f64::from(size.0)) as i32;
    let bottom = (((old[1] + old[3]).max(new[1] + new[3]) + offset.1 + 3.0) * s)
        .ceil()
        .min(f64::from(size.1)) as i32;
    [left, top, right, bottom]
}
// END PURE SURFACE RECT

// BEGIN PURE SURFACE TESTS
#[test]
fn surface_roi_adds_native_offset_and_rounds_outward() {
    let old = [10.25, 20.5, 40.0, 12.0];
    let new = [19.25, 20.5, 40.0, 12.0];
    assert_eq!(
        surface_rect(old, new, (8.0, 12.0), 1, (200, 100)),
        [15, 29, 71, 48]
    );
    assert_eq!(
        surface_rect(old, new, (8.0, 12.0), 2, (400, 200)),
        [30, 59, 141, 95]
    );
    assert_eq!(
        surface_rect(old, new, (-10.0, -20.0), 1, (20, 10)),
        [0, 0, 20, 10]
    );
}
// END PURE SURFACE TESTS

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub struct Frame {
    pub rect: PixelRect,
    pub scale: i32,
    pub pixels: Vec<c_ulong>,
    rgb_masks: [c_ulong; 3],
}
impl Frame {
    /// Diagnostic only: no assertion, threshold, filtering, or pixel mutation.
    pub fn log_differences(&self, full: &Self, stage: &str) {
        let mut differences = 0usize;
        let mut bounds: Option<(i32, i32, i32, i32)> = None;
        for (i, (a, b)) in self.pixels.iter().zip(&full.pixels).enumerate() {
            if a == b {
                continue;
            }
            let x = self.rect.x + (i % self.rect.width as usize) as i32;
            let y = self.rect.y + (i / self.rect.width as usize) as i32;
            bounds = Some(match bounds {
                Some((l, t, r, b)) => (l.min(x), t.min(y), r.max(x), b.max(y)),
                None => (x, y, x, y),
            });
            if differences < 128 {
                let rgb = |pixel: c_ulong| {
                    self.rgb_masks.map(|mask| {
                        let shift = mask.trailing_zeros();
                        (u128::from((pixel & mask) >> shift) * 255 / u128::from(mask >> shift))
                            as u8
                    })
                };
                eprintln!(
                    "paint-difference {stage}: ({x},{y}) old={a:#x}/{:?} new={b:#x}/{:?}",
                    rgb(*a),
                    rgb(*b)
                );
            }
            differences += 1;
        }
        eprintln!("paint-difference {stage}: total={differences} bbox={bounds:?} masks={:?} shape={:?}/{:?} pixel_counts={}/{}", self.rgb_masks, self.rect, full.rect, self.pixels.len(), full.pixels.len());
    }

    pub fn assert_same_pixels(&self, full: &Self, stage: &str) {
        assert_eq!(self.rect, full.rect, "{stage}: ROI changed");
        assert_eq!(self.scale, full.scale, "{stage}: scale changed");
        assert_eq!(self.rgb_masks, full.rgb_masks, "{stage}: visual changed");
        assert_eq!(self.pixels.len(), full.pixels.len());
        let (mut differences, mut first_index) = (0_usize, None);
        for (i, (a, b)) in self.pixels.iter().zip(&full.pixels).enumerate() {
            if a != b {
                differences += 1;
                first_index.get_or_insert(i);
            }
        }
        let first = first_index.map(|i| {
            (
                self.rect.x + (i % self.rect.width as usize) as i32,
                self.rect.y + (i / self.rect.width as usize) as i32,
            )
        });
        assert_eq!(
            differences, 0,
            "{stage}: {differences} differing physical pixels, first={first:?}, ROI={:?}, scale={}",
            self.rect, self.scale
        );
    }
    /// Optional diagnostic artifact. RGB is decoded using XImage's actual masks.
    pub fn write_ppm(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        write!(file, "P6\n{} {}\n255\n", self.rect.width, self.rect.height)?;
        for pixel in &self.pixels {
            let rgb = self.rgb_masks.map(|mask| {
                let shift = mask.trailing_zeros();
                let max = mask >> shift;
                (u128::from((pixel & mask) >> shift) * 255 / u128::from(max)) as u8
            });
            file.write_all(&rgb)?;
        }
        Ok(())
    }
}

pub struct SurfaceProbe {
    // Keep native GDK surface/display alive until the Xlib connection closes.
    surface: gtk::gdk::Surface,
    display: Display,
    xid: c_ulong,
    api: Api,
    _library: Library,
}
impl SurfaceProbe {
    pub fn new(window: &gtk::Window) -> Result<Self, String> {
        let surface = window
            .surface()
            .ok_or("fixture window has no native surface")?;
        if !surface.type_().name().contains("X11") {
            return Err("fixture requires GDK_BACKEND=x11".into());
        }
        let library = Library::open()?;
        let api = unsafe {
            Api {
                open: library.symbol(c"XOpenDisplay")?,
                close: library.symbol(c"XCloseDisplay")?,
                sync: library.symbol(c"XSync")?,
                set_handler: library.symbol(c"XSetErrorHandler")?,
                get_image: library.symbol(c"XGetImage")?,
                get_pixel: library.symbol(c"XGetPixel")?,
                destroy_image: library.symbol(c"XDestroyImage")?,
                clear_area: library.symbol(c"XClearArea")?,
                get_geometry: library.symbol(c"XGetGeometry")?,
            }
        };
        let xid_ptr =
            unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"gdk_x11_surface_get_xid".as_ptr()) };
        if xid_ptr.is_null() {
            return Err("missing gdk_x11_surface_get_xid".into());
        }
        let xid_fn: unsafe extern "C" fn(*mut c_void) -> c_ulong =
            unsafe { std::mem::transmute(xid_ptr) };
        let xid = unsafe { xid_fn(surface.as_ptr().cast()) };
        if xid == 0 {
            return Err("fixture surface has no XID".into());
        }
        let name = CString::new(surface.display().name().as_str()).map_err(|e| e.to_string())?;
        let display = unsafe { (api.open)(name.as_ptr()) };
        if display.is_null() {
            return Err("XOpenDisplay for the GDK display failed".into());
        }
        Ok(Self {
            surface,
            display,
            xid,
            api,
            _library: library,
        })
    }
    fn checked<T>(
        &self,
        operation: &str,
        f: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        // No GTK/main-context iteration while the process-global handler is installed.
        let guard = ErrorGuard::new(&self.api, self.display);
        unsafe {
            (self.api.sync)(self.display, 0);
        }
        guard.result("earlier X11 request")?;
        let result = f();
        unsafe {
            (self.api.sync)(self.display, 0);
        }
        guard.result(operation)?;
        result
    }
    pub fn scale(&self) -> i32 {
        self.surface.scale_factor()
    }
    pub fn full_rect(&self) -> Result<PixelRect, String> {
        self.surface.display().sync();
        let scale = self.scale();
        let width = self
            .surface
            .width()
            .checked_mul(scale)
            .ok_or("physical width overflow")?;
        let height = self
            .surface
            .height()
            .checked_mul(scale)
            .ok_or("physical height overflow")?;
        if width <= 0 || height <= 0 {
            return Err("unallocated fixture surface".into());
        }
        self.checked("XGetGeometry(integer-scale check)", || {
            let (mut root, mut x, mut y) = (0, 0, 0);
            let (mut native_width, mut native_height, mut border, mut depth) = (0, 0, 0, 0);
            let ok = unsafe { (self.api.get_geometry)(self.display, self.xid, &mut root, &mut x, &mut y, &mut native_width, &mut native_height, &mut border, &mut depth) };
            if ok == 0 { return Err("XGetGeometry failed".into()); }
            if native_width != width as u32 || native_height != height as u32 {
                return Err(format!("fixture requires X11 integer scaling: native={native_width}x{native_height}, logical={}x{}, scale={scale}", self.surface.width(), self.surface.height()));
            }
            Ok(())
        })?;
        Ok(PixelRect {
            x: 0,
            y: 0,
            width: width as u32,
            height: height as u32,
        })
    }
    pub fn capture(&self, rect: PixelRect) -> Result<Frame, String> {
        self.surface.display().sync();
        let full = self.full_rect()?;
        if rect.x < 0
            || rect.y < 0
            || rect.width == 0
            || rect.height == 0
            || i64::from(rect.x) + i64::from(rect.width) > i64::from(full.width)
            || i64::from(rect.y) + i64::from(rect.height) > i64::from(full.height)
        {
            return Err(format!(
                "invalid physical capture ROI {rect:?} within {full:?}"
            ));
        }
        let count = (rect.width as usize)
            .checked_mul(rect.height as usize)
            .ok_or("capture size overflow")?;
        if count > 16_777_216 {
            return Err("fixture capture exceeds 16M pixel safety bound".into());
        }
        self.checked("XGetImage", || {
            let image = unsafe {
                (self.api.get_image)(
                    self.display,
                    self.xid,
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    c_ulong::MAX,
                    2,
                )
            };
            if image.is_null() {
                return Err("XGetImage returned no image".into());
            }
            let image = OwnedImage {
                image,
                api: &self.api,
            };
            let prefix = unsafe { &*image.image.cast::<ImagePrefix>() };
            if prefix.width != rect.width as i32
                || prefix.height != rect.height as i32
                || prefix.format != 2
                || !(1..=64).contains(&prefix.depth)
                || prefix.bits_per_pixel < prefix.depth
                || prefix.bits_per_pixel > 64
            {
                return Err(format!(
                    "unexpected XImage: {}x{} format={} depth={} bpp={} for ROI={rect:?}",
                    prefix.width, prefix.height, prefix.format, prefix.depth, prefix.bits_per_pixel
                ));
            }
            let masks = [prefix.red_mask, prefix.green_mask, prefix.blue_mask];
            if masks.contains(&0) {
                return Err("fixture requires a TrueColor visual with RGB masks".into());
            }
            if masks[0] & masks[1] != 0 || masks[0] & masks[2] != 0 || masks[1] & masks[2] != 0 {
                return Err("XImage RGB masks overlap".into());
            }
            for mask in masks {
                let channel = mask >> mask.trailing_zeros();
                if channel & channel.wrapping_add(1) != 0 {
                    return Err("XImage RGB channel mask is not contiguous".into());
                }
            }
            let combined = masks.iter().fold(0, |a, b| a | b);
            let mut pixels = Vec::with_capacity(count);
            for y in 0..rect.height {
                for x in 0..rect.width {
                    pixels.push(
                        (unsafe { (self.api.get_pixel)(image.image, x as i32, y as i32) })
                            & combined,
                    );
                }
            }
            Ok(Frame {
                rect,
                scale: self.scale(),
                pixels,
                rgb_masks: masks,
            })
        })
    }
    /// Test-only complete native exposure, with no resize or widget mutation.
    /// Caller must await a *new* after-paint using the fixture's existing timeout.
    pub fn expose_full(&self) -> Result<(), String> {
        self.surface.display().sync();
        self.checked("XClearArea(full expose)", || {
            unsafe {
                (self.api.clear_area)(self.display, self.xid, 0, 0, 0, 0, 1);
            }
            Ok(())
        })
    }
    pub fn log_metadata(&self, label: &gtk::Label, rect: PixelRect, stage: &str) {
        let context = label.pango_context();
        let font = context
            .font_description()
            .map(|f| f.to_string())
            .unwrap_or_default();
        let resolved_font = context
            .font_description()
            .and_then(|description| context.load_font(&description))
            .map(|font| font.describe().to_string())
            .unwrap_or_default();
        let (ink, logical) = label.layout().extents();
        let (lx, ly) = label.layout_offsets();
        eprintln!("surface-probe stage={stage:?} gtk={}.{}.{} scale={} physical_roi={rect:?} font={font:?} resolved_font={resolved_font:?} layout_origin=({lx},{ly}) baseline={} pango_ink={ink:?} pango_logical={logical:?}", gtk::major_version(), gtk::minor_version(), gtk::micro_version(), self.scale(), label.layout().baseline());
    }
}
impl Drop for SurfaceProbe {
    fn drop(&mut self) {
        let guard = ErrorGuard::new(&self.api, self.display);
        unsafe {
            (self.api.sync)(self.display, 0);
            (self.api.close)(self.display);
        }
        if let Err(error) = guard.result("X11 probe cleanup") {
            eprintln!("{error}");
        }
    }
}

/// Does not request frames, start timers, or spin its own event loop.
pub struct PaintSerial {
    clock: gtk::gdk::FrameClock,
    handler: Option<gtk::glib::SignalHandlerId>,
    serial: Rc<Cell<u64>>,
}
impl PaintSerial {
    pub fn new(window: &gtk::Window) -> Result<Self, String> {
        let clock = window.frame_clock().ok_or("fixture has no frame clock")?;
        let serial = Rc::new(Cell::new(0_u64));
        let next = serial.clone();
        let handler = clock.connect_after_paint(move |_| next.set(next.get().wrapping_add(1)));
        Ok(Self {
            clock,
            handler: Some(handler),
            serial,
        })
    }
    pub fn current(&self) -> u64 {
        self.serial.get()
    }
}
impl Drop for PaintSerial {
    fn drop(&mut self) {
        if let Some(handler) = self.handler.take() {
            self.clock.disconnect(handler);
        }
    }
}
