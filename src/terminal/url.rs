//! URL detection + Ctrl+click handling for finished-block text views.
//!
//! Plain-text URLs are recognised by the family's one opener policy and made
//! clickable on Ctrl+click. OSC 8 targets are stored as private tag data rather
//! than interpolated into GTK object names. Hovering a URL underlines it and
//! shows the pointer cursor.
//! Ported from forge's `block_view/url.rs`.

use gtk::gio;
use gtk::prelude::*;
use gtk::TextBuffer;
use relm4::gtk;

use super::select::get_semantic_bounds_at_position;

pub(crate) const OSC8_URI_DATA_KEY: &str = "anvil-osc8-uri";

/// The one policy every clickable target must satisfy, shared with ember,
/// forge and frost as `jterm_core::link::is_openable_url`.
///
/// Link text is process-controlled, so a click is the terminal acting on
/// untrusted data: any `cat` of a hostile file could plant it. anvil used to
/// allow `file:`, `ftp:`, `git:`, `ssh:` and `mailto:` beside HTTP(S), and
/// applied no authority rule at all, so a single Ctrl+click handed the desktop
/// opener a local file to open with its default application, started a network
/// client, or passed `https://user:token@host` — a credential the user never
/// typed — to the browser. Only an absolute HTTP(S) URL with an authority and
/// no userinfo qualifies now. The same predicate gates detection and opening,
/// so nothing is ever underlined as clickable that a click would refuse.
pub fn is_openable_url(text: &str) -> bool {
    jterm_core::link::is_openable_url(text)
}

fn trim_trailing(text: &str) -> &str {
    text.trim_end_matches(|c| {
        matches!(
            c,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '>' | '\'' | '"'
        )
    })
}

pub fn open_uri(uri: &str) {
    if !is_openable_url(uri) {
        log::warn!("Refusing to open a malformed or unsupported URI");
        return;
    }
    if let Err(err) = gio::AppInfo::launch_default_for_uri(uri, None::<&gio::AppLaunchContext>) {
        log::warn!(
            "Failed to open a validated URI: {}",
            crate::review_input::safe_inline_display(&err.to_string(), 1024)
        );
    }
}

pub(crate) fn osc8_uri(tag: &gtk::TextTag) -> Option<String> {
    // The String is owned by the tag and lives for exactly as long as it does.
    unsafe {
        tag.data::<String>(OSC8_URI_DATA_KEY)
            .map(|value| value.as_ref().clone())
    }
}

/// Find the URL surrounding `iter` (whitespace/`<>`-delimited), trimming trailing
/// sentence punctuation. Returns the adjusted bounds and the URL text.
pub fn get_url_bounds_at_position(
    buffer: &TextBuffer,
    iter: &gtk::TextIter,
) -> Option<(gtk::TextIter, gtk::TextIter, String)> {
    let mut start = *iter;
    let mut end = *iter;

    while !start.starts_line() {
        let ch = start.char();
        if ch == ' ' || ch == '\n' || ch == '\t' || ch == '<' || ch == '>' {
            start.forward_char();
            break;
        }
        if !start.backward_char() {
            break;
        }
    }

    while !end.ends_line() {
        let ch = end.char();
        if ch == ' ' || ch == '\n' || ch == '\t' || ch == '<' || ch == '>' {
            break;
        }
        if !end.forward_char() {
            break;
        }
    }

    if start.offset() >= end.offset() {
        return None;
    }

    let raw = buffer.text(&start, &end, false).to_string();
    let trimmed = trim_trailing(&raw);
    if !is_openable_url(trimmed) {
        return None;
    }
    let trimmed_chars = trimmed.chars().count();
    let raw_chars = raw.chars().count();
    for _ in 0..(raw_chars - trimmed_chars) {
        end.backward_char();
    }
    Some((start, end, trimmed.to_string()))
}

/// URL at `iter`: prefer a validated OSC 8 tag, else plain-text detection.
pub fn get_url_at_position(buffer: &TextBuffer, iter: &gtk::TextIter) -> Option<String> {
    for tag in iter.tags() {
        if let Some(uri) = osc8_uri(&tag).filter(|uri| is_openable_url(uri)) {
            return Some(uri);
        }
    }
    get_url_bounds_at_position(buffer, iter).map(|(_, _, url)| url)
}

/// If `iter` lies inside a validated OSC 8 tag span, return that span's bounds
/// and the URI. Used by the hover handler to underline OSC 8 hyperlinks the
/// same way plain-text URLs are underlined.
pub fn get_osc8_bounds_at_position(
    _buffer: &TextBuffer,
    iter: &gtk::TextIter,
) -> Option<(gtk::TextIter, gtk::TextIter, String)> {
    let tag = iter
        .tags()
        .into_iter()
        .find(|tag| osc8_uri(tag).is_some_and(|uri| is_openable_url(&uri)))?;
    let uri = osc8_uri(&tag)?;
    let mut start = *iter;
    if !start.starts_tag(Some(&tag)) {
        start.backward_to_tag_toggle(Some(&tag));
    }
    let mut end = *iter;
    if !end.ends_tag(Some(&tag)) {
        end.forward_to_tag_toggle(Some(&tag));
    }
    if start.offset() >= end.offset() {
        return None;
    }
    Some((start, end, uri))
}

/// Attach Ctrl+click-to-open and hover-underline controllers to a read-only
/// output/command `TextView`.
pub fn attach_url_handlers(view: &gtk::TextView) {
    let buffer = view.buffer();

    let click = gtk::GestureClick::new();
    click.set_button(1);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let buffer = buffer.clone();
        let view = view.clone();
        click.connect_pressed(move |controller, n_press, x, y| {
            let (bx, by) =
                view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
            let iter = view.iter_at_location(bx, by);
            if n_press == 1 {
                let state = controller.current_event_state();
                if state.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                    if let Some(iter) = iter {
                        if let Some(url) = get_url_at_position(&buffer, &iter) {
                            open_uri(&url);
                            controller.set_state(gtk::EventSequenceState::Claimed);
                            return;
                        }
                    }
                }
            } else if n_press == 2 {
                // Smart selection: grab the whole semantic token instead of
                // GTK's default plain-word selection.
                if let Some(iter) = iter {
                    if let Some((start, end)) = get_semantic_bounds_at_position(&buffer, &iter) {
                        buffer.select_range(&start, &end);
                        controller.set_state(gtk::EventSequenceState::Claimed);
                        return;
                    }
                }
            }
            controller.set_state(gtk::EventSequenceState::Denied);
        });
    }
    view.add_controller(click);

    let url_tag = gtk::TextTag::new(Some("url-hover"));
    url_tag.set_underline(gtk::pango::Underline::Single);
    buffer.tag_table().add(&url_tag);

    let motion = gtk::EventControllerMotion::new();
    {
        let view = view.clone();
        let buffer = buffer.clone();
        let tag = url_tag.clone();
        motion.connect_motion(move |_, x, y| {
            let (bx, by) =
                view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
            let start = buffer.start_iter();
            let end = buffer.end_iter();
            buffer.remove_tag(&tag, &start, &end);

            if let Some(iter) = view.iter_at_location(bx, by) {
                if let Some((us, ue, _)) = get_url_bounds_at_position(&buffer, &iter) {
                    buffer.apply_tag(&tag, &us, &ue);
                    view.set_cursor(gtk::gdk::Cursor::from_name("pointer", None).as_ref());
                    return;
                }
                if let Some((us, ue, _)) = get_osc8_bounds_at_position(&buffer, &iter) {
                    buffer.apply_tag(&tag, &us, &ue);
                    view.set_cursor(gtk::gdk::Cursor::from_name("pointer", None).as_ref());
                    return;
                }
            }
            view.set_cursor(gtk::gdk::Cursor::from_name("text", None).as_ref());
        });
    }
    {
        let view = view.clone();
        let buffer = buffer.clone();
        let tag = url_tag;
        motion.connect_leave(move |_| {
            let start = buffer.start_iter();
            let end = buffer.end_iter();
            buffer.remove_tag(&tag, &start, &end);
            view.set_cursor(gtk::gdk::Cursor::from_name("text", None).as_ref());
        });
    }
    view.add_controller(motion);
}

#[cfg(test)]
mod tests {
    use super::is_openable_url;

    #[test]
    fn uri_policy_is_the_shared_family_opener_contract() {
        assert!(is_openable_url("https://example.com/a?b=c"));
        assert!(is_openable_url("HTTP://example.com"));
        assert!(!is_openable_url("javascript:alert(1)"));
        assert!(!is_openable_url("data:text/html,boom"));
        assert!(!is_openable_url("https://example.com/a path"));
        assert!(!is_openable_url("https://example.com/line\nnext"));
        assert!(!is_openable_url("https://example.com/safe\u{00ad}hidden"));
        assert!(!is_openable_url("https://example.com/safe\u{e0020}hidden"));
        assert!(!is_openable_url(&format!(
            "https://example.com/{}",
            "x".repeat(jterm_core::link::MAX_OPENABLE_URL_BYTES)
        )));
    }

    /// The schemes anvil used to hand straight to the desktop opener. Each one
    /// turns a `cat` of a hostile file into a one-click local-file open, a
    /// network client launch, or a credential handed to the browser.
    #[test]
    fn the_schemes_anvil_used_to_launch_are_refused() {
        for rejected in [
            "file:///etc/passwd",
            "ftp://example.com/archive",
            "git://example.com/repo.git",
            "ssh://example.com",
            "mailto:user@example.com",
            "https://user:token@example.com/",
            "https://user@example.com/private",
            "https:///missing-host",
            "https://example.com\\evil",
        ] {
            assert!(!is_openable_url(rejected), "{rejected}");
        }
    }
}
