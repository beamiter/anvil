//! Toolkit-independent Pango markup. Shared by the GTK label and native offscreen QA.
use eva_character_atlas as atlas;
pub fn markup(frame: &atlas::Frame, skin: atlas::Skin, high_contrast: bool) -> String {
    let mut out = String::from("<span font_family=\"monospace\" size=\"7000\">");
    let palette = skin.palette(high_contrast);
    for (row, line) in frame.cells.chunks(frame.width).enumerate() {
        if row > 0 {
            out.push('\n');
        }
        for c in line {
            let [r, g, b] = palette.color(c.ink);
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
