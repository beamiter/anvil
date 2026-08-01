//! Local compatibility policy for text that crosses a trusted UI/action
//! boundary while jterm1 still exact-pins the previous jterm_core release.

/// Characters that can make the visible order or apparent contents differ
/// from the string acted upon. Keep this in lockstep with jterm_core's review
/// input policy until the staged core release is pinned.
pub(crate) fn is_visual_spoof(ch: char) -> bool {
    (ch.is_whitespace() && ch != ' ')
        || matches!(
            ch,
            '\u{00ad}'
                | '\u{034f}'
                | '\u{061c}'
                | '\u{115f}'..='\u{1160}'
                | '\u{17b4}'..='\u{17b5}'
                | '\u{180b}'..='\u{180f}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{3164}'
                | '\u{fe00}'..='\u{fe0f}'
                | '\u{feff}'
                | '\u{ffa0}'
                | '\u{1bca0}'..='\u{1bca3}'
                | '\u{1d173}'..='\u{1d17a}'
                | '\u{e0001}'
                | '\u{e0020}'..='\u{e007f}'
                | '\u{e0100}'..='\u{e01ef}'
        )
}

pub(crate) fn contains_visual_spoof(text: &str) -> bool {
    text.chars().any(is_visual_spoof)
}

/// Make untrusted text safe for trusted application chrome. Newline and tab
/// can optionally remain layout characters for multi-line source/output; all
/// other controls and visual-spoof characters become an explicit replacement.
pub(crate) fn bounded_display_text(
    text: &str,
    max_bytes: usize,
    preserve_multiline: bool,
) -> String {
    const SUFFIX: &str = "\u{2026} [truncated]";

    let mut output = String::with_capacity(text.len().min(max_bytes));
    let mut truncated = false;
    for ch in text.chars() {
        let preserved_layout = preserve_multiline && matches!(ch, '\n' | '\t');
        let displayed = if !preserved_layout && (ch.is_control() || is_visual_spoof(ch)) {
            '\u{fffd}'
        } else {
            ch
        };
        if output.len().saturating_add(displayed.len_utf8()) > max_bytes {
            truncated = true;
            break;
        }
        output.push(displayed);
    }
    if truncated {
        while output.len().saturating_add(SUFFIX.len()) > max_bytes {
            if output.pop().is_none() {
                break;
            }
        }
        if SUFFIX.len() <= max_bytes {
            output.push_str(SUFFIX);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_spoof_policy_covers_whitespace_bidi_variants_and_tags() {
        for ch in [
            '\t',
            '\n',
            '\u{00ad}',
            '\u{034f}',
            '\u{061c}',
            '\u{115f}',
            '\u{17b4}',
            '\u{180b}',
            '\u{200b}',
            '\u{202e}',
            '\u{2066}',
            '\u{3164}',
            '\u{fe0f}',
            '\u{feff}',
            '\u{ffa0}',
            '\u{1bca0}',
            '\u{1d173}',
            '\u{e0001}',
            '\u{e0020}',
            '\u{e0100}',
        ] {
            assert!(is_visual_spoof(ch), "U+{:04X} was accepted", ch as u32);
        }
        assert!(!is_visual_spoof(' '));
        assert!(!is_visual_spoof('A'));
        assert!(!is_visual_spoof('\u{4e2d}'));
    }

    #[test]
    fn display_text_is_strictly_bounded_and_can_preserve_source_layout() {
        let display = bounded_display_text("a\n\tb\u{202e}c", 64, true);
        assert_eq!(display, "a\n\tb\u{fffd}c");
        let bounded = bounded_display_text(&"\u{754c}".repeat(100), 24, false);
        assert!(bounded.len() <= 24);
        assert!(bounded.is_char_boundary(bounded.len()));
        assert!(bounded.ends_with("[truncated]"));
    }
}
