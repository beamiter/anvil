//! Toolkit-independent Pango markup. Shared by the GTK label and native offscreen QA.
use eva_character_atlas as atlas;
pub fn markup(frame: &atlas::Frame, skin: atlas::Skin, high_contrast: bool) -> String {
    markup_on_surface(frame, skin, high_contrast, false)
}

pub fn surface_color(color: [u8; 3], high_contrast: bool, light: bool) -> [u8; 3] {
    if light && high_contrast {
        [24, 40, 60]
    } else if light {
        color.map(|value| (u16::from(value) * 2 / 5) as u8)
    } else {
        color
    }
}

pub fn markup_on_surface(
    frame: &atlas::Frame,
    skin: atlas::Skin,
    high_contrast: bool,
    light: bool,
) -> String {
    let mut out = String::from("<span font_family=\"monospace\" size=\"7000\">");
    let palette = skin.palette(high_contrast);
    for (row, line) in frame.cells.chunks(frame.width).enumerate() {
        if row > 0 {
            out.push('\n');
        }
        for c in line {
            let [r, g, b] = surface_color(palette.color(c.ink), high_contrast, light);
            let glyph = match c.glyph {
                '&' => "&amp;".into(),
                '<' => "&lt;".into(),
                '>' => "&gt;".into(),
                '\"' => "&quot;".into(),
                '\'' => "&apos;".into(),
                c => c.to_string(),
            };
            out.push_str(&format!(
                "<span foreground=\"#{r:02x}{g:02x}{b:02x}\">{glyph}</span>"
            ));
        }
    }
    out.push_str("</span>");
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn light_canvas_inks_keep_dark_detail_and_dark_canvas_palette_is_unchanged() {
        for skin in atlas::Skin::ALL {
            for ink in [atlas::Ink::Outline, atlas::Ink::Face, atlas::Ink::Accent] {
                let original = skin.palette(false).color(ink);
                assert_eq!(surface_color(original, false, false), original);
                assert!(surface_color(original, false, true)
                    .iter()
                    .all(|c| *c <= 102));
                assert_eq!(surface_color(original, true, true), [24, 40, 60]);
            }
        }
    }
    #[test]
    fn all_skins_poses_have_native_safe_markup() {
        for skin in atlas::Skin::ALL {
            for pose in atlas::Pose::ALL {
                let f = atlas::sample(atlas::Request::new(skin, pose)).unwrap();
                let m = markup(&f, skin, false);
                assert_eq!(m.matches("foreground=").count(), f.cells.len());
                assert!(!m.contains("<3"));
                assert_eq!(m.lines().count(), f.height);
            }
        }
    }
}
