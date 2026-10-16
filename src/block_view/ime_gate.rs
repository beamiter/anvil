use relm4::gtk;
// Observe public GTK preedit lifecycle without inspecting VTE's private IM
// context or storing composed text. Older GTK cannot expose a context's owner,
// so the Send-only gate is deliberately application-wide on the GTK thread.
use gtk::glib::{self, translate::*};
use gtk::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    OnceLock,
};

pub(super) const NOTICE: &str =
    "Finish or cancel text composition before using Send. Use the keyboard to complete composing.";
static INSTALLED: OnceLock<bool> = OnceLock::new();
static BROKEN: AtomicBool = AtomicBool::new(false);
thread_local! {
    static ACTIVE: RefCell<HashMap<usize, glib::WeakRef<gtk::IMContext>>> = RefCell::new(HashMap::new());
}

// Hooks are process-lifetime registrations with no heap/user data. GTK emits
// these signals on its UI thread. No GTK/UI calls occur while ACTIVE is borrowed,
// and no panic may escape the GObject ABI.
unsafe extern "C" fn started(
    _: *mut glib::gobject_ffi::GSignalInvocationHint,
    count: u32,
    values: *const glib::gobject_ffi::GValue,
    _: glib::ffi::gpointer,
) -> glib::ffi::gboolean {
    observe(count, values, true)
}
unsafe extern "C" fn ended(
    _: *mut glib::gobject_ffi::GSignalInvocationHint,
    count: u32,
    values: *const glib::gobject_ffi::GValue,
    _: glib::ffi::gpointer,
) -> glib::ffi::gboolean {
    observe(count, values, false)
}
fn observe(
    count: u32,
    values: *const glib::gobject_ffi::GValue,
    active: bool,
) -> glib::ffi::gboolean {
    let result = std::panic::catch_unwind(|| {
        if !gtk::is_initialized_main_thread() || count == 0 || values.is_null() {
            BROKEN.store(true, Ordering::Relaxed);
            return;
        }
        // SAFETY: GObject supplies the instance GValue as the first parameter
        // of the registered GtkIMContext preedit-start/end signals.
        let ptr = unsafe { glib::gobject_ffi::g_value_get_object(values) };
        if ptr.is_null() {
            BROKEN.store(true, Ordering::Relaxed);
            return;
        }
        let context: glib::translate::Borrowed<gtk::IMContext> =
            unsafe { from_glib_borrow(ptr.cast()) };
        let weak = active.then(|| context.downgrade());
        let _ = ACTIVE.try_with(|contexts| {
            let mut contexts = contexts.borrow_mut();
            if let Some(weak) = weak {
                contexts.insert(ptr as usize, weak);
            } else {
                contexts.remove(&(ptr as usize));
            }
        });
    });
    if result.is_err() {
        BROKEN.store(true, Ordering::Relaxed);
    }
    1 // Keep the hooks installed; never suppress native IM processing.
}

pub(super) fn install() {
    INSTALLED.get_or_init(|| {
        // SAFETY: class_ref initializes the abstract IMContext class and its
        // signals before lookup. These two public signals allow emission hooks.
        unsafe {
            let type_ = gtk::IMContext::static_type().into_glib();
            let class = glib::gobject_ffi::g_type_class_ref(type_);
            if class.is_null() {
                return false;
            }
            let start = glib::gobject_ffi::g_signal_lookup(c"preedit-start".as_ptr(), type_);
            let end = glib::gobject_ffi::g_signal_lookup(c"preedit-end".as_ptr(), type_);
            let a = if start != 0 {
                glib::gobject_ffi::g_signal_add_emission_hook(
                    start,
                    0,
                    Some(started),
                    std::ptr::null_mut(),
                    None,
                )
            } else {
                0
            };
            let b = if end != 0 {
                glib::gobject_ffi::g_signal_add_emission_hook(
                    end,
                    0,
                    Some(ended),
                    std::ptr::null_mut(),
                    None,
                )
            } else {
                0
            };
            glib::gobject_ffi::g_type_class_unref(class);
            a != 0 && b != 0
        }
    });
}

pub(super) fn notice() -> &'static str {
    if INSTALLED.get() != Some(&true) || BROKEN.load(Ordering::Relaxed) {
        "Send is unavailable. Use the keyboard to finish composing and submit."
    } else {
        NOTICE
    }
}

pub(super) fn blocked() -> bool {
    if INSTALLED.get() != Some(&true) || BROKEN.load(Ordering::Relaxed) {
        return true;
    }
    // Upgrade outside the registry borrow: no finalization/signal callbacks
    // can reenter a held borrow. Destroyed contexts cannot permanently block Send.
    let snapshot = ACTIVE.with(|contexts| {
        contexts
            .borrow()
            .iter()
            .map(|(id, weak)| (*id, weak.clone()))
            .collect::<Vec<_>>()
    });
    let dead = snapshot
        .iter()
        .filter_map(|(id, weak)| weak.upgrade().is_none().then_some(*id))
        .collect::<Vec<_>>();
    ACTIVE.with(|contexts| {
        let mut contexts = contexts.borrow_mut();
        for id in dead {
            contexts.remove(&id);
        }
        !contexts.is_empty()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires DISPLAY; public IMContext hook lifecycle"]
    fn preedit_lifecycle_tracks_each_live_context_without_text() {
        gtk::init().expect("GTK display");
        install();
        assert!(!blocked());
        let first = gtk::IMMulticontext::new();
        let second = gtk::IMMulticontext::new();
        first.emit_by_name::<()>("preedit-start", &[]);
        first.emit_by_name::<()>("preedit-start", &[]); // idempotent identity
        second.emit_by_name::<()>("preedit-start", &[]);
        assert!(blocked());
        first.emit_by_name::<()>("preedit-end", &[]);
        assert!(blocked(), "another context is still composing");
        second.emit_by_name::<()>("preedit-changed", &[]); // empty is not end
        second.emit_by_name::<()>("commit", &[&"x"]); // partial commit is not end
        assert!(blocked());
        second.emit_by_name::<()>("preedit-end", &[]);
        assert!(!blocked(), "commit/cancel end reopens Send");
        first.emit_by_name::<()>("preedit-start", &[]);
        drop(first);
        assert!(!blocked(), "destroyed contexts do not retain a gate");
    }
}
