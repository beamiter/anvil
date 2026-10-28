//! Terminal font resolution — keeping icon glyphs on an icon font.
//!
//! A configured family such as `Monospace` carries no Nerd-Font glyph, so
//! every icon a shell prompt or a vim statusline draws misses the primary
//! font and lands in Pango's generic fallback. That fallback is fontconfig's
//! global sort, and it is won by whichever installed font claims the Private
//! Use Area codepoint — which is rarely the icon font. On a box carrying the
//! Arphic CJK fonts, `AR PL UMing HK` answers for U+E0A0 (powerline branch),
//! U+E5FA (Seti file icons) and U+E700 (devicons), so lualine paints a row of
//! unrelated ideographs where its icons belong.
//!
//! Pango walks every family in a description's comma-separated list before it
//! reaches that generic fallback, so naming an installed Nerd Font as a
//! secondary family repairs the icons while leaving the text font the user
//! chose in charge of every glyph it actually covers.

use crate::config::Config;
use gtk::pango::FontDescription;
use gtk::prelude::*;
use relm4::gtk;
use std::sync::OnceLock;

/// Substring every Nerd Font family name carries.
const NERD_FONT_MARKER: &str = "Nerd Font";

/// The preferred family in `DEFAULT_FONT_DESC`; keep saved settings unchanged.
const DEFAULT_FONT_FAMILY: &str = "JetBrainsMono Nerd Font Mono";

/// Resolve only our single default family. Custom names and ordered lists belong
/// to the user, including lists that happen to contain the default family.
fn default_family_fallback(family: &str, default_available: bool) -> &str {
    if !default_available && family.eq_ignore_ascii_case(DEFAULT_FONT_FAMILY) {
        "Monospace"
    } else {
        family
    }
}

fn runtime_family(family: &str) -> &str {
    if !family.eq_ignore_ascii_case(DEFAULT_FONT_FAMILY) || !gtk::is_initialized_main_thread() {
        return family;
    }
    // Font enumeration is expensive: like icon detection, do it once after GTK
    // starts, never caching a headless/unit-test answer. Use the display's Pango
    // family catalog rather than load_font(), which silently substitutes a face.
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    let available = *AVAILABLE.get_or_init(|| {
        gtk::Label::new(None)
            .pango_context()
            .list_families()
            .iter()
            .any(|candidate| candidate.name().eq_ignore_ascii_case(DEFAULT_FONT_FAMILY))
    });
    default_family_fallback(family, available)
}

/// Rank `name` as an icon fallback; lower is better, `None` rejects it.
///
/// The `Mono` cuts hold every icon to a single cell, which is the only shape
/// that lines up in a terminal grid; the proportional cuts overhang their
/// neighbours. Nerd Fonts also ships icon-only `Symbols` cuts that exist to be
/// somebody else's fallback, so those win over borrowing icons out of a full
/// text font that may disagree about metrics.
fn icon_family_rank(name: &str) -> Option<u8> {
    let variant = if name.ends_with("Nerd Font Mono") {
        0
    } else if name.ends_with("Nerd Font Propo") {
        4
    } else if name.ends_with(NERD_FONT_MARKER) {
        2
    } else {
        return None;
    };
    Some(variant + u8::from(!name.starts_with("Symbols")))
}

/// Best icon fallback among `families`, or `None` when none is installed.
///
/// Ties break on the family name so the choice cannot drift between runs with
/// the order fontconfig happens to enumerate in.
pub(crate) fn select_icon_family<I>(families: I) -> Option<String>
where
    I: IntoIterator<Item = String>,
{
    families
        .into_iter()
        .filter_map(|name| icon_family_rank(&name).map(|rank| (rank, name)))
        .min()
        .map(|(_, name)| name)
}

/// The icon family this process falls back to, resolved once.
///
/// Enumerating families walks every font file fontconfig knows about, and the
/// answer cannot change while the process runs.
fn detected_icon_family() -> Option<&'static str> {
    static DETECTED: OnceLock<Option<String>> = OnceLock::new();
    if let Some(detected) = DETECTED.get() {
        return detected.as_deref();
    }
    // Listing families needs a font map, which needs GTK up on this thread —
    // and unit tests reach the CSS builder with no toolkit at all. Answer
    // `None` there without memoizing it, so a verdict taken before GTK existed
    // cannot outlive the moment.
    if !gtk::is_initialized_main_thread() {
        return None;
    }
    // Any widget hands out a context bound to the display's font map; an
    // unparented label needs neither a window nor realization.
    let context = gtk::Label::new(None).pango_context();
    let detected = select_icon_family(
        context
            .list_families()
            .iter()
            .map(|family| family.name().to_string()),
    );
    DETECTED.get_or_init(|| detected).as_deref()
}

/// The icon family to append for `config`: the configured override, the
/// detected default, or `None` when the user turned the fallback off.
pub(crate) fn icon_family(config: &Config) -> Option<&str> {
    match config.icon_font.as_deref() {
        Some(name) if name.is_empty() || name.eq_ignore_ascii_case("none") => None,
        Some(name) => Some(name),
        None => detected_icon_family(),
    }
}

/// Extend a Pango family list with `icon_family`, or `None` to leave it alone.
///
/// A description that already names an icon font — or names nothing, and so
/// asks for the display default — is returned untouched: appending there would
/// change which font draws ordinary text, not just the icons.
fn family_list_with_icon(families: &str, icon_family: &str) -> Option<String> {
    if families.trim().is_empty() {
        return None;
    }
    let already_covered = families.split(',').any(|family| {
        let family = family.trim();
        family.contains(NERD_FONT_MARKER) || family.eq_ignore_ascii_case(icon_family)
    });
    // set_family() does not trim family-list separators: a leading space
    // becomes part of the appended name and silently disables the fallback.
    (!already_covered).then(|| format!("{families},{icon_family}"))
}

/// Parse `desc`, resolve a missing default, and append the icon fallback.
///
/// Everything else in the description — size, weight, style — survives, so the
/// user's font string stays the one thing that decides how text looks.
pub(crate) fn font_description(desc: &str, icon_family: Option<&str>) -> FontDescription {
    let mut font = FontDescription::from_string(desc);
    if let Some(family) = font.family() {
        let resolved = runtime_family(&family);
        if resolved != family.as_str() {
            font.set_family(resolved);
        }
    }
    let Some(icon_family) = icon_family else {
        return font;
    };
    let families = font.family().unwrap_or_default();
    if let Some(list) = family_list_with_icon(&families, icon_family) {
        font.set_family(&list);
    }
    font
}

/// The description every VTE surface should be given for `desc` under `config`.
pub(crate) fn terminal_font_description(desc: &str, config: &Config) -> FontDescription {
    font_description(desc, icon_family(config))
}

/// The CSS `font-family` value for `family`, with the icon fallback behind it.
///
/// Block chrome draws command text through GTK labels rather than the VTE, so
/// it needs the same fallback list spelled the way CSS wants it.
pub(crate) fn css_font_stack(family: &str, icon_family: Option<&str>) -> String {
    let family = runtime_family(family);
    let escape = |value: &str| value.replace('\\', "\\\\").replace('"', "\\\"");
    let primary = format!("\"{}\"", escape(family));
    match icon_family {
        Some(icon) if family_list_with_icon(family, icon).is_some() => {
            format!("{primary}, \"{}\"", escape(icon))
        }
        _ => primary,
    }
}

/// Match the VTE's Pango size and zoom without rounding fractional points or
/// interpreting an absolute pixel size as points. CSS and terminal text share
/// this source; widget icons retain their explicit pixel sizes.
pub(crate) fn effective_font_size_css(desc: &str, scale: f64) -> String {
    let font = FontDescription::from_string(desc);
    let base = if font.size() > 0 {
        f64::from(font.size()) / f64::from(gtk::pango::SCALE)
    } else {
        14.0
    };
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let unit = if font.is_size_absolute() { "px" } else { "pt" };
    format!("{}{unit}", base * scale)
}

/// Preserve Pango's point/absolute-pixel distinction during legacy migration.
/// Converting pixels at an assumed DPI would change UI text on HiDPI desktops.
pub(crate) fn ui_font_size_absolute(config: &Config) -> bool {
    if config.ui_font_size.is_some() {
        config.ui_font_size_absolute
    } else {
        FontDescription::from_string(&config.font_desc).is_size_absolute()
    }
}

pub(crate) fn ui_font_size(config: &Config) -> f64 {
    config.ui_font_size.unwrap_or_else(|| {
        let font = FontDescription::from_string(&config.font_desc);
        if font.size() > 0 {
            f64::from(font.size()) / f64::from(gtk::pango::SCALE)
        } else {
            14.0
        }
    })
}

/// Resolve legacy fallbacks once before any group mutation. Every saved
/// edited state then carries both independent effective groups across restart.
pub(crate) fn freeze_ui_font(config: &mut Config) {
    let absolute = ui_font_size_absolute(config);
    let size = ui_font_size(config);
    let scale = crate::font_groups::ui_scale(config.default_font_scale, config.ui_font_scale);
    config.ui_font_size_absolute = absolute;
    config.ui_font_size = Some(size);
    config.ui_font_scale = Some(scale);
}

/// Preserve authored role proportions relative to the existing nominal 14pt
/// UI default. Resolve both descriptions in the same real widget context so
/// absolute pixels are not mistaken for points or multiplied by device scale.
pub(crate) fn reference_ui_role_multiplier(config: &Config, context: &gtk::pango::Context) -> f64 {
    use gtk::pango::prelude::FontExt;

    let scale = crate::font_groups::ui_scale(config.default_font_scale, config.ui_font_scale);
    let scale = if scale.is_finite() {
        scale.clamp(0.1, 10.0)
    } else {
        1.0
    };
    let size = ui_font_size(config);
    let size = if size.is_finite() {
        size.clamp(0.1, 10_000.0)
    } else {
        14.0
    };
    let units = f64::from(gtk::pango::SCALE);
    let mut nominal = context
        .font_description()
        .unwrap_or_else(|| FontDescription::from_string("sans-serif"));
    nominal.set_size(14 * gtk::pango::SCALE);
    let mut configured = nominal.clone();
    if ui_font_size_absolute(config) {
        configured.set_absolute_size(size * units);
    } else {
        configured.set_size((size * units).round() as i32);
    }
    let resolved = |description: &FontDescription| {
        context
            .load_font(description)
            .map(|font| font.describe_with_absolute_size().size())
            .filter(|size| *size > 0)
            .map(f64::from)
    };
    match (resolved(&configured), resolved(&nominal)) {
        (Some(configured), Some(nominal)) => scale * configured / nominal,
        // A missing font context retains the prior role appearance. Avoid a
        // guessed DPI and retry through the next normal appearance update.
        _ => scale,
    }
}

/// Scope display-wide CSS to this application's window. Explicit text-node
/// rules override desktop heading/caption sizes and fixed pane-header sizes;
/// images and decorative connection dots keep their own icon sizing.
pub(crate) fn window_font_css(config: &Config) -> String {
    let size = effective_font_size_css(&config.font_desc, config.default_font_scale);
    let ui_scale = crate::font_groups::ui_scale(config.default_font_scale, config.ui_font_scale);
    let ui_size = match config.ui_font_size {
        Some(size) => {
            let unit = if config.ui_font_size_absolute {
                "px"
            } else {
                "pt"
            };
            format!("{}{unit}", size * ui_scale)
        }
        None => effective_font_size_css(&config.font_desc, ui_scale),
    };
    format!(
        "window.anvil-font-sync, window.anvil-font-sync label:not(.conn-dot),
         window.anvil-font-sync entry, window.anvil-font-sync text,
         window.anvil-font-sync textview, window.anvil-font-sync treeview {{ font-size: {size}; }}
         window.anvil-font-sync .anvil-ui-font,
         window.anvil-font-sync .anvil-ui-font label:not(.conn-dot),
         window.anvil-font-sync .anvil-ui-font entry,
         window.anvil-font-sync .anvil-ui-font text,
         window.anvil-font-sync .anvil-ui-font textview,
         window.anvil-font-sync .anvil-ui-font treeview {{ font-size: {ui_size}; }}"
    )
}

/// Native popup text follows the configured font without inheriting a
/// selected tab's bold/italic decoration. Pango parses size/style boundaries;
/// the existing CSS stack encoder quotes family names safely.
pub(crate) fn menu_font_css(desc: &str) -> String {
    let font = font_description(desc, None);
    let family = font.family().unwrap_or_else(|| "sans-serif".into());
    let stack = family
        .split(',')
        .map(str::trim)
        .map(|family| css_font_stack(family, None))
        .collect::<Vec<_>>()
        .join(", ");
    let mut css = format!("font-family: {stack}; font-weight: normal; font-style: normal;");
    if font.size() > 0 {
        let size = f64::from(font.size()) / f64::from(gtk::pango::SCALE);
        let unit = if font.is_size_absolute() { "px" } else { "pt" };
        css.push_str(&format!(" font-size: {size}{unit};"));
    }
    css
}

/// Theme only opted-in popup surfaces; native menus elsewhere retain their
/// own presentation. The display-wide provider updates an already-open menu.
pub(crate) fn menu_theme_css(
    desc: &str,
    background: &gtk::gdk::RGBA,
    foreground: &gtk::gdk::RGBA,
) -> String {
    let color = |color: &gtk::gdk::RGBA| {
        format!(
            "rgb({},{},{})",
            (color.red() * 255.0) as u8,
            (color.green() * 255.0) as u8,
            (color.blue() * 255.0) as u8
        )
    };
    let bg = color(background);
    let fg = color(foreground);
    let font = menu_font_css(desc);
    format!(
        r#"
        .terminal-context-menu, .terminal-context-menu label,
        .terminal-context-menu button, .terminal-context-menu entry {{ {font} }}
        .terminal-context-menu > contents {{ background-color: {bg}; color: {fg}; border: 1px solid alpha({fg},0.22); }}
        .terminal-context-menu > arrow {{ background-color: {bg}; border-color: alpha({fg},0.22); }}
        .terminal-context-menu button {{ color: {fg}; background-color: transparent; background-image: none; box-shadow: none; border-color: transparent; }}
        .terminal-context-menu button:hover, .terminal-context-menu button:active,
        .terminal-context-menu button:focus-visible {{ outline-color: alpha({fg},0.5); background-color: alpha({fg},0.12); }}
        .terminal-context-menu button:disabled {{ color: alpha({fg},0.45); }}
        .terminal-context-menu separator {{ background-color: alpha({fg},0.18); }}
        .terminal-context-menu scrolledwindow, .terminal-context-menu viewport,
        .terminal-context-menu scrollbar, .terminal-context-menu scrollbar trough {{ background-color: transparent; }}
        .terminal-context-menu scrollbar slider {{ background-color: alpha({fg},0.3); }}
        .terminal-context-menu scrollbar slider:hover {{ background-color: alpha({fg},0.5); }}
        .terminal-context-menu entry {{ color: {fg}; background-color: alpha({fg},0.06); caret-color: {fg}; }}
    "#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_ui_size_preserves_legacy_pixel_size_and_main_edits() {
        let mut config = Config::safe_defaults();
        config.font_desc = "Monospace 16px".into();
        config.default_font_scale = 1.5;
        assert_eq!(ui_font_size(&config), 16.0);
        assert!(ui_font_size_absolute(&config));
        let legacy = window_font_css(&config);
        assert!(legacy.contains("font-size: 24px"));
        config.ui_font_size_absolute = ui_font_size_absolute(&config);
        config.ui_font_size = Some(ui_font_size(&config));
        config.ui_font_scale = Some(config.default_font_scale);
        config.font_desc = "Monospace 40".into();
        config.default_font_scale = 2.0;
        let split = window_font_css(&config);
        assert!(split.contains("font-size: 80pt"));
        assert!(split.contains("font-size: 24px"));
        for node in [
            "label:not(.conn-dot)",
            "entry",
            "text",
            "textview",
            "treeview",
        ] {
            assert!(split.contains(&format!(".anvil-ui-font {node}")));
        }
    }

    #[test]
    #[ignore = "requires DISPLAY"]
    fn ui_font_group_is_independent_on_live_native_widgets() {
        use relm4::prelude::*;
        gtk::init().unwrap();
        crate::startup_ui::install_static_css();
        let provider = crate::startup_ui::install_dynamic_css_provider();
        let window = gtk::Window::new();
        window.add_css_class("anvil-font-sync");
        window.set_default_size(1600, 1800);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        window.set_child(Some(&root));
        let (_, top_scroll) = crate::startup_ui::build_tab_scrolls();
        let top_label = gtk::Label::new(Some("Top tab"));
        top_scroll.set_child(Some(&top_label));
        top_scroll.set_visible(true);
        let top = crate::top_bar::TopBarModel::builder()
            .launch(top_scroll.clone())
            .detach();
        top.widget().add_css_class("anvil-ui-font");
        root.append(top.widget());
        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar.add_css_class("anvil-ui-font");
        let sidebar_label = gtk::Label::new(Some("Sidebar tab"));
        let sidebar_button = gtk::Button::new();
        sidebar_button.set_child(Some(&sidebar_label));
        sidebar.append(&sidebar_button);
        root.append(&sidebar);
        let right_sidebar = gtk::Stack::new();
        right_sidebar.add_css_class("anvil-ui-font");
        let right_label = gtk::Label::new(Some("AI Chats and Tasks sidebar"));
        right_sidebar.add_named(&right_label, Some("chats"));
        root.append(&right_sidebar);
        let content = gtk::Label::new(Some("Main content"));
        let split = gtk::Label::new(Some("Second pane"));
        root.append(&content);
        root.append(&split);
        let labels = [top_label, sidebar_label, right_label, content, split];
        let mut config = Config::safe_defaults();
        config.font_desc = "Monospace 12px".into();
        config.ui_font_size_absolute = true;
        config.ui_font_size = Some(12.0);
        config.ui_font_scale = Some(1.0);
        window.present();
        // Legacy appearance, main-only step, UI-only step, main-only size edit.
        for (main_size, main_scale, ui_size, ui_scale) in [
            (12, 1.0, 12.0, 1.0),
            (12, 1.5, 12.0, 1.0),
            (12, 1.5, 12.0, 2.0),
            (18, 1.5, 12.0, 2.0),
            (12, 0.1, 12.0, 10.0),
            (12, 10.0, 12.0, 0.1),
        ] {
            config.font_desc = format!("Monospace {main_size}px");
            config.default_font_scale = main_scale;
            config.ui_font_size = Some(ui_size);
            config.ui_font_scale = Some(ui_scale);
            provider.load_from_string(&crate::config_ops::dynamic_css(&config));
            let main = gtk::glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(150);
            while std::time::Instant::now() < deadline {
                while main.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            for (index, label) in labels.iter().enumerate() {
                let pixels = if index < 3 {
                    ui_size * ui_scale
                } else {
                    f64::from(main_size) * main_scale
                };
                assert_eq!(
                    label.pango_context().font_description().unwrap().size(),
                    (pixels * f64::from(gtk::pango::SCALE)).round() as i32
                );
                assert!(
                    label.height() >= label.layout().pixel_size().1,
                    "mixed font limits must not clip text vertically"
                );
            }
            assert!(top_scroll.height() >= labels[0].layout().pixel_size().1);
            assert!(sidebar_button.height() >= labels[1].layout().pixel_size().1);
        }
        window.close();
    }

    #[test]
    fn effective_size_preserves_pango_units_and_fractional_zoom() {
        assert_eq!(
            effective_font_size_css("Monospace Bold Italic 12.5", 1.2),
            "15pt"
        );
        assert_eq!(effective_font_size_css("Monospace 17px", 1.5), "25.5px");
        assert_eq!(effective_font_size_css("Monospace 12", 0.75), "9pt");
    }

    #[test]
    fn font_updates_reach_window_css_and_every_terminal() {
        let settings = include_str!("settings_ops.rs");
        let stage = settings
            .split("pub(crate) fn stage_font_scale")
            .nth(1)
            .unwrap()
            .split("pub(crate) fn apply_font_scale_step")
            .next()
            .unwrap();
        assert!(stage.contains("default_font_scale = scale"));
        assert!(stage.contains("self.set_font_scale_all(scale)"));
        assert!(stage.contains("self.apply_dynamic_css()"));
        let startup = include_str!("main.rs");
        assert!(startup.contains("add_css_class: \"anvil-font-sync\""));
        assert!(startup.contains("model.apply_dynamic_css()"));
    }

    #[test]
    #[ignore = "requires DISPLAY"]
    fn window_text_tracks_terminal_size_on_startup_and_live_updates() {
        use relm4::prelude::*;
        use vte4::TerminalExt;
        gtk::init().unwrap();
        crate::startup_ui::install_static_css();
        let provider = crate::startup_ui::install_dynamic_css_provider();
        let errors = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let caught = errors.clone();
        provider
            .connect_parsing_error(move |_, _, error| caught.borrow_mut().push(error.to_string()));
        let window = gtk::Window::new();
        window.add_css_class("anvil-font-sync");
        window.set_default_size(1000, 700);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        window.set_child(Some(&root));
        let (_, tab_scroll) = crate::startup_ui::build_tab_scrolls();
        let top_label = gtk::Label::new(Some("Top tab"));
        tab_scroll.set_child(Some(&top_label));
        tab_scroll.set_visible(true);
        let top = crate::top_bar::TopBarModel::builder()
            .launch(tab_scroll.clone())
            .detach();
        root.append(top.widget());
        let labels: Vec<_> = [
            "top-bar",
            "tab-strip",
            "pane-header",
            "bottom-bar",
            "block-cmd",
            "chat-status",
        ]
        .iter()
        .map(|class| {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            row.add_css_class(class);
            let label = gtk::Label::new(Some("Text size stays aligned"));
            row.append(&label);
            root.append(&row);
            label
        })
        .collect();
        let entry = gtk::Entry::new();
        root.append(&entry);
        let icon = gtk::Image::from_icon_name("window-close-symbolic");
        icon.set_pixel_size(16);
        root.append(&icon);
        let outside = gtk::Label::new(Some("Unscoped text"));
        let outside_before = outside.pango_context().font_description().unwrap().size();
        let mut config = Config::safe_defaults();
        config.font_desc = "Monospace 16px".into();
        config.default_font_scale = 0.75;
        let terminals: Vec<_> = (0..2)
            .map(|_| {
                let terminal = crate::terminal::vte::create_terminal(&config);
                terminal.set_vexpand(false);
                terminal.set_size(40, 3);
                root.append(&terminal);
                terminal
            })
            .collect();
        // Install before mapping, then change the same live provider twice.
        provider.load_from_string(&crate::config_ops::dynamic_css(&config));
        window.present();
        for (desc, scale) in [
            ("Monospace 16px", 0.75),
            ("Monospace 16px", 1.5),
            ("Monospace 40px", 1.5),
        ] {
            config.font_desc = desc.into();
            config.default_font_scale = scale;
            for terminal in &terminals {
                terminal.set_font(Some(&terminal_font_description(desc, &config)));
                terminal.set_font_scale(scale);
            }
            crate::block_view::install_block_css(&config);
            provider.load_from_string(&crate::config_ops::dynamic_css(&config));
            let main = gtk::glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(150);
            while std::time::Instant::now() < deadline {
                while main.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            let expected =
                (FontDescription::from_string(desc).size() as f64 * scale).round() as i32;
            for label in &labels {
                assert_eq!(
                    label.pango_context().font_description().unwrap().size(),
                    expected
                );
                let (_, height) = label.layout().pixel_size();
                assert!(
                    label.height() >= height,
                    "text must not be vertically clipped"
                );
            }
            assert_eq!(
                entry.pango_context().font_description().unwrap().size(),
                expected
            );
            assert_eq!(
                top_label.pango_context().font_description().unwrap().size(),
                expected
            );
            assert!(
                tab_scroll.height() >= top_label.layout().pixel_size().1,
                "real overlay tab row must grow with its font"
            );
            assert_eq!(icon.pixel_size(), 16);
            assert_eq!(
                outside.pango_context().font_description().unwrap().size(),
                outside_before
            );
            for terminal in &terminals {
                assert_eq!(terminal.font_scale(), scale);
                assert_eq!(
                    terminal.font().unwrap().size(),
                    FontDescription::from_string(desc).size()
                );
            }
        }
        assert!(
            errors.borrow().is_empty(),
            "CSS errors: {:?}",
            errors.borrow()
        );
        window.destroy();
        gtk::style_context_remove_provider_for_display(
            &gtk::gdk::Display::default().unwrap(),
            &provider,
        );
    }

    fn default_description() -> String {
        Config::safe_defaults().font_desc
    }

    #[test]
    fn default_fallback_is_narrow_and_keeps_the_saved_default() {
        let configured = FontDescription::from_string(&default_description());
        assert_eq!(configured.family().as_deref(), Some(DEFAULT_FONT_FAMILY));
        assert_eq!(
            default_family_fallback(DEFAULT_FONT_FAMILY, false),
            "Monospace"
        );
        assert_eq!(
            default_family_fallback(DEFAULT_FONT_FAMILY, true),
            DEFAULT_FONT_FAMILY
        );
        assert_eq!(
            default_family_fallback("jetbrainsmono nerd font mono", false),
            "Monospace"
        );
        assert_eq!(
            default_family_fallback("jetbrainsmono nerd font mono", true),
            "jetbrainsmono nerd font mono"
        );
        for custom in [
            "",
            "Monospace",
            "User Font",
            "JetBrainsMono Nerd Font",
            "JetBrainsMono Nerd Font Mono, Monospace",
        ] {
            assert_eq!(default_family_fallback(custom, false), custom);
        }
    }

    #[test]
    #[ignore = "requires a GTK display"]
    fn default_font_resolution_preserves_grid_and_configuration() {
        use vte4::TerminalExt;
        // A pre-GTK lookup must not poison the runtime availability cache.
        let _ = font_description(&default_description(), None);
        gtk::init().expect("GTK display");
        let config = Config::safe_defaults();
        let before = config.font_desc.clone();
        let context = gtk::Label::new(None).pango_context();
        let installed = context
            .list_families()
            .iter()
            .any(|family| family.name().eq_ignore_ascii_case(DEFAULT_FONT_FAMILY));
        let expected = default_family_fallback(DEFAULT_FONT_FAMILY, installed);
        for custom in [
            "DejaVu Sans Mono Bold Italic 13.5",
            "JetBrainsMono Nerd Font Mono,Monospace Bold 15",
            "Monospace,JetBrainsMono Nerd Font Mono Italic 12",
            "User Chosen Missing Family 14",
        ] {
            assert_eq!(
                font_description(custom, None),
                FontDescription::from_string(custom),
                "custom family lists, style and size remain exactly as parsed"
            );
        }
        let actual = font_description("JetBrainsMono Nerd Font Mono Bold Italic 13.5", None);
        assert_eq!(actual.family().as_deref(), Some(expected));
        assert_eq!(actual.size(), (13.5 * gtk::pango::SCALE as f64) as i32);
        assert_eq!(actual.weight(), gtk::pango::Weight::Bold);
        assert_eq!(actual.style(), gtk::pango::Style::Italic);
        assert_eq!(
            css_font_stack(DEFAULT_FONT_FAMILY, None),
            format!("\"{expected}\"")
        );
        let terminal = crate::terminal::vte::create_terminal(&config);
        let reference = vte4::Terminal::new();
        reference.set_font(Some(&font_description(
            &format!("{expected} 14"),
            icon_family(&config),
        )));
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        for surface in [&terminal, &reference] {
            surface.set_size(32, 6);
            surface.set_hexpand(true);
            surface.set_vexpand(true);
            row.append(surface);
        }
        let window = gtk::Window::builder()
            .default_width(1000)
            .default_height(240)
            .child(&row)
            .build();
        window.present();
        let pump = || {
            for _ in 0..20 {
                for _ in 0..64 {
                    if !gtk::glib::MainContext::default().iteration(false) {
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        pump();
        for surface in [&terminal, &reference] {
            surface.feed("iiii WWWW\r\n你好 😀 e\u{301}\r\nA中😀e\u{301}".as_bytes());
        }
        pump();
        assert_eq!(terminal.char_width(), reference.char_width());
        assert_eq!(terminal.char_height(), reference.char_height());
        assert_eq!(terminal.cursor_position(), (6, 2));
        assert_eq!(terminal.cursor_position(), reference.cursor_position());
        assert_eq!(
            terminal.text_format(vte4::Format::Text),
            reference.text_format(vte4::Format::Text)
        );
        window.close();
        assert_eq!(config.font_desc, before);
        assert_eq!(before, default_description());
    }

    #[test]
    #[ignore = "requires a GTK display"]
    fn an_appended_font_family_resolves_in_the_native_font_map() {
        gtk::init().expect("GTK display");
        let context = gtk::Label::new(None).pango_context();
        let missing = "Anvil QA Unavailable Family";
        assert!(!context
            .list_families()
            .iter()
            .any(|family| family.name() == missing));
        let description =
            font_description(&format!("{missing} Bold Italic 13.5"), Some("Monospace"));
        let expected = FontDescription::from_string("Monospace Bold Italic 13.5");
        let resolve =
            |font: &FontDescription| context.load_font(font).expect("native font").describe();
        assert_eq!(resolve(&description).family(), resolve(&expected).family());
        assert_eq!(description.size(), expected.size());
        assert_eq!(description.weight(), expected.weight());
        assert_eq!(description.style(), expected.style());
    }

    #[test]
    fn mono_cuts_outrank_proportional_and_symbols_outrank_text_fonts() {
        assert!(
            icon_family_rank("Symbols Nerd Font Mono") < icon_family_rank("Hack Nerd Font Mono")
        );
        assert!(icon_family_rank("Hack Nerd Font Mono") < icon_family_rank("Symbols Nerd Font"));
        assert!(icon_family_rank("Hack Nerd Font") < icon_family_rank("Hack Nerd Font Propo"));
        assert_eq!(icon_family_rank("DejaVu Sans Mono"), None);
        assert_eq!(icon_family_rank("AR PL UMing HK"), None);
    }

    #[test]
    fn selection_prefers_the_mono_cut_and_breaks_ties_by_name() {
        let installed = [
            "AR PL UMing HK",
            "JetBrainsMono Nerd Font Propo",
            "SauceCodePro Nerd Font",
            "JetBrainsMono Nerd Font Mono",
            "DejaVuSansM Nerd Font Mono",
        ]
        .map(str::to_string);
        assert_eq!(
            select_icon_family(installed),
            Some("DejaVuSansM Nerd Font Mono".to_string())
        );
    }

    #[test]
    fn selection_is_none_without_an_icon_font() {
        let installed = ["DejaVu Sans Mono", "Arial"].map(str::to_string);
        assert_eq!(select_icon_family(installed), None);
    }

    #[test]
    fn a_font_that_already_draws_icons_is_left_alone() {
        assert_eq!(
            family_list_with_icon("JetBrainsMono Nerd Font", "Symbols Nerd Font Mono"),
            None
        );
        assert_eq!(family_list_with_icon("", "Symbols Nerd Font Mono"), None);
        assert_eq!(
            family_list_with_icon("Symbols Nerd Font Mono", "Symbols Nerd Font Mono"),
            None
        );
    }

    #[test]
    fn the_icon_family_lands_behind_the_configured_one() {
        assert_eq!(
            family_list_with_icon("Monospace", "Symbols Nerd Font Mono"),
            Some("Monospace,Symbols Nerd Font Mono".to_string())
        );
    }

    #[test]
    fn size_and_style_survive_the_rewrite() {
        let font = font_description("Monospace Bold Italic 14", Some("Symbols Nerd Font Mono"));
        assert_eq!(
            font.family().map(|f| f.to_string()).unwrap_or_default(),
            "Monospace,Symbols Nerd Font Mono"
        );
        assert_eq!(font.size() / gtk::pango::SCALE, 14);
        assert_eq!(font.weight(), gtk::pango::Weight::Bold);
        assert_eq!(font.style(), gtk::pango::Style::Italic);
    }

    #[test]
    fn no_icon_family_leaves_the_description_untouched() {
        let font = font_description("Monospace 14", None);
        assert_eq!(
            font.family().map(|f| f.to_string()).unwrap_or_default(),
            "Monospace"
        );
    }

    /// The wiring, not just the arithmetic: a live VTE built from a config
    /// whose font covers no icons must be handed the fallback family too.
    /// Needs a display, so it runs from `make test-display`.
    #[test]
    #[ignore = "requires a GTK display"]
    fn a_real_vte_is_given_the_icon_family_behind_the_configured_one() {
        use vte4::TerminalExt;

        gtk::init().expect("GTK display");
        let mut config = Config::safe_defaults();
        config.font_desc = "Monospace 14".to_string();
        let configured = FontDescription::from_string(&config.font_desc)
            .family()
            .map(|family| family.to_string())
            .expect("the default font names a family");
        let terminal = crate::terminal::vte::create_terminal(&config);
        let families = terminal
            .font()
            .and_then(|font| font.family())
            .map(|family| family.to_string())
            .expect("the terminal carries a font");

        match icon_family(&config) {
            Some(icon) => assert_eq!(families, format!("{configured},{icon}")),
            // A box with no Nerd Font installed has nothing to fall back to,
            // and the description must then be exactly what the user asked for.
            None => assert_eq!(families, configured),
        }
    }

    #[test]
    #[ignore = "requires DISPLAY"]
    fn context_menu_tracks_live_theme_and_font_without_parent_emphasis() {
        gtk::init().expect("GTK display");
        struct AnimationPreference(gtk::Settings, bool);
        impl Drop for AnimationPreference {
            fn drop(&mut self) {
                self.0.set_gtk_enable_animations(self.1);
            }
        }
        let settings = gtk::Settings::default().unwrap();
        let _animations =
            AnimationPreference(settings.clone(), settings.is_gtk_enable_animations());
        settings.set_gtk_enable_animations(false);
        let provider = gtk::CssProvider::new();
        let errors = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let caught = errors.clone();
        provider
            .connect_parsing_error(move |_, _, error| caught.borrow_mut().push(error.to_string()));
        let display = gtk::gdk::Display::default().unwrap();
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );
        let window = gtk::Window::new();
        // Leave room for the parent tab when its emphasis style is applied.
        window.set_default_size(640, 480);
        let tab = gtk::ToggleButton::with_label("Selected tab");
        tab.add_css_class("tab-strip-btn");
        tab.set_active(true);
        window.set_child(Some(&tab));
        let popover = gtk::Popover::new();
        popover.add_css_class("terminal-context-menu");
        popover.set_has_arrow(false);
        popover.set_parent(&tab);
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let item = gtk::Button::with_label("Rename tab");
        item.set_has_frame(false);
        item.add_css_class("flat");
        menu.append(&item);
        let scroll = crate::context_menu::scrolled_content(&menu, &tab);
        popover.set_child(Some(&scroll));
        window.present();
        popover.popup();
        let main = gtk::glib::MainContext::default();
        for (name, bg, fg, desc, size) in [
            (
                "dark",
                "#16161e",
                "#c0caf5",
                "Monospace Bold Italic 18px",
                18,
            ),
            ("light", "#f6f8fa", "#24292f", "Sans Italic 22px", 22),
            ("custom", "#221122", "#ffddee", "Monospace Bold 16px", 16),
        ] {
            let bg = gtk::gdk::RGBA::parse(bg).unwrap();
            let fg = gtk::gdk::RGBA::parse(fg).unwrap();
            let css = format!(
                ".tab-strip-btn {{ font-weight: bold; font-style: italic; }} {}",
                menu_theme_css(desc, &bg, &fg)
            );
            provider.load_from_string(&css);
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
            while std::time::Instant::now() < deadline {
                while main.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            let label = item.child().unwrap().downcast::<gtk::Label>().unwrap();
            let actual = label.pango_context().font_description().unwrap();
            assert_eq!(
                actual.family(),
                FontDescription::from_string(desc).family(),
                "{name}"
            );
            assert_eq!(actual.weight(), gtk::pango::Weight::Normal, "{name}");
            assert_eq!(actual.style(), gtk::pango::Style::Normal, "{name}");
            assert_eq!(actual.size(), size * gtk::pango::SCALE, "{name}");
            let actual_fg = label.color();
            for (actual, expected) in [
                (actual_fg.red(), fg.red()),
                (actual_fg.green(), fg.green()),
                (actual_fg.blue(), fg.blue()),
            ] {
                assert!((actual - expected).abs() < 0.01, "{name}");
            }
            let snapshot = gtk::Snapshot::new();
            gtk::WidgetPaintable::new(Some(&popover)).snapshot(
                &snapshot,
                f64::from(popover.width()),
                f64::from(popover.height()),
            );
            let node = snapshot.to_node().expect("mapped popup snapshot");
            let renderer = gtk::gsk::Renderer::for_surface(&window.surface().unwrap()).unwrap();
            let texture = renderer.render_texture(&node, None);
            let stride = texture.width() as usize * 4;
            let mut pixels = vec![0; stride * texture.height() as usize];
            texture.download(&mut pixels, stride);
            let rgb = [
                (bg.red() * 255.0) as u8,
                (bg.green() * 255.0) as u8,
                (bg.blue() * 255.0) as u8,
            ];
            let matching = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| {
                    let argb = u32::from_ne_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]);
                    [
                        ((argb >> 16) & 255) as u8,
                        ((argb >> 8) & 255) as u8,
                        (argb & 255) as u8,
                    ] == rgb
                })
                .count();
            assert!(
                matching > 100,
                "{name}: popup background must use the configured color"
            );
            if let Some(directory) = std::env::var_os("JTERM_MENU_SCREENSHOTS") {
                std::fs::create_dir_all(&directory).unwrap();
                let path = std::path::Path::new(&directory)
                    .join(format!("{}-{name}.png", env!("CARGO_PKG_NAME")));
                texture.save_to_png(path).unwrap();
            }
            renderer.unrealize();
        }
        assert!(
            scroll.height() < scroll.max_content_height(),
            "small menus keep their natural height"
        );
        let mut last = item.clone();
        for index in 1..32 {
            last = gtk::Button::with_label(&format!(
                "Remote host {index}: a complete long label remains accessible"
            ));
            last.set_has_frame(false);
            last.add_css_class("flat");
            menu.append(&last);
        }
        provider.load_from_string(&menu_theme_css(
            "Monospace 48",
            &gtk::gdk::RGBA::BLACK,
            &gtk::gdk::RGBA::WHITE,
        ));
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        while std::time::Instant::now() < deadline {
            while main.iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let vertical = scroll.vadjustment();
        assert!(
            vertical.upper() > vertical.page_size(),
            "large menus must scroll"
        );
        assert!(
            scroll.height() <= scroll.max_content_height() + 24,
            "viewport remains monitor-bounded including scrollbar chrome"
        );
        vertical.set_value(vertical.upper() - vertical.page_size());
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        while std::time::Instant::now() < deadline {
            while main.iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let last_bounds = last.compute_bounds(&scroll).unwrap();
        assert!(
            last_bounds.y() >= 0.0
                && last_bounds.y() + last_bounds.height() <= scroll.height() as f32 + 1.0,
            "last item is reachable by scrolling: {last_bounds:?}"
        );
        let horizontal = scroll.hadjustment();
        assert!(
            horizontal.upper() > horizontal.page_size(),
            "long labels scroll horizontally instead of being truncated"
        );
        assert!(errors.borrow().is_empty(), "{:?}", errors.borrow());
        popover.popdown();
        popover.unparent();
        window.close();
        gtk::style_context_remove_provider_for_display(&display, &provider);
    }

    #[test]
    fn popup_font_uses_pango_family_and_fractional_size_without_tab_emphasis() {
        let css = menu_font_css("Monospace Bold Italic 13.5");
        assert!(css.contains("font-family: \"Monospace\";"));
        assert!(css.contains("font-size: 13.5pt;"));
        assert!(css.contains("font-weight: normal;"));
        assert!(css.contains("font-style: normal;"));
        assert!(menu_font_css("Monospace 17px").contains("font-size: 17px;"));
        assert!(
            menu_font_css("Monospace, Serif 12").contains("font-family: \"Monospace\", \"Serif\";")
        );
    }

    #[test]
    fn the_css_stack_quotes_and_escapes_both_families() {
        assert_eq!(
            css_font_stack("Monospace", Some("Symbols Nerd Font Mono")),
            "\"Monospace\", \"Symbols Nerd Font Mono\""
        );
        assert_eq!(css_font_stack("Monospace", None), "\"Monospace\"");
        assert_eq!(
            css_font_stack("Hack Nerd Font", Some("Symbols Nerd Font Mono")),
            "\"Hack Nerd Font\""
        );
        assert_eq!(
            css_font_stack("ev\"il", Some("Symbols Nerd Font Mono")),
            "\"ev\\\"il\", \"Symbols Nerd Font Mono\""
        );
    }
}
