//! Bounded, display-only last-line previews of already normalized output.
const TAIL_BYTES: usize = 8 * 1024;
const PREVIEW_CHARS: usize = 120;

/// A preview never becomes clipboard or terminal input. Scan only a bounded
/// tail, and mark both a partial source line and a shortened display line.
pub(super) fn from_plain(output: &str) -> String {
    let mut start = output.len().saturating_sub(TAIL_BYTES);
    while !output.is_char_boundary(start) {
        start += 1;
    }
    let tail = &output[start..];
    for line in tail.rsplit('\n') {
        let line_trimmed = line.trim();
        if line_trimmed.is_empty() {
            continue;
        }
        let partial =
            start > 0 && line.as_ptr() == tail.as_ptr() && output.as_bytes()[start - 1] != b'\n';
        let mut preview = String::new();
        if partial {
            preview.push('…');
        }
        let mut chars = line_trimmed.chars();
        for ch in chars.by_ref().take(PREVIEW_CHARS) {
            // Do not let terminal controls or directional/invisible format
            // controls change the surrounding native UI. Preserve literal markup.
            preview.push(if ch == '\t' {
                ' '
            } else if ch.is_control()
                || matches!(ch, '\u{061c}' | '\u{200b}'..='\u{200f}'
                    | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}')
            {
                '�'
            } else {
                ch
            });
        }
        if chars.next().is_some() {
            preview.push('…');
        }
        return preview;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_uses_last_nonempty_line_and_preserves_literal_text() {
        assert_eq!(
            from_plain("building\n  All tests passed  \n\n"),
            "All tests passed"
        );
        assert_eq!(from_plain("\n \t\r\n"), "");
        assert_eq!(from_plain("<b>literal</b>\tvalue"), "<b>literal</b> value");
    }

    #[test]
    fn preview_neutralizes_controls_and_bounds_unicode() {
        assert_eq!(from_plain("ok\u{202e}evil\u{2069}\0"), "ok�evil��");
        assert_eq!(from_plain("one\u{2028}two\u{2029}three"), "one�two�three");
        assert_eq!(
            from_plain(&"界".repeat(121)),
            format!("{}…", "界".repeat(120))
        );
        let large = format!("hidden prefix\n{}", "界".repeat(10_000));
        let result = from_plain(&large);
        assert!(result.starts_with('…') && result.ends_with('…'));
        assert!(result.chars().count() <= PREVIEW_CHARS + 2);
        // A bounded empty tail never reaches backwards into older output.
        assert_eq!(
            from_plain(&format!("old secret\n{}", "\n".repeat(TAIL_BYTES))),
            ""
        );
    }

    #[test]
    fn filtered_preview_never_falls_back_to_unfiltered_output() {
        assert_eq!(from_plain("visible match\n"), "visible match");
        assert_eq!(from_plain(""), "");
    }
}
