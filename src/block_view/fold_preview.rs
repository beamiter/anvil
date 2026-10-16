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

/// Normalize only for a display preview. Simple text/color streams retain a
/// bounded tail; terminal editing controls keep the existing replay semantics.
pub(super) fn from_output(output: &str) -> String {
    if let Some(preview) = from_sgr_text(output) {
        return preview;
    }
    let plain = super::ansi::strip_ansi(output);
    from_plain(&plain)
}

// Four extra bytes preserve a complete preceding scalar, even if the ring's
// first retained byte lands in the middle of a four-byte UTF-8 scalar. This
// preserves from_plain's cutoff and its partial-source-line marker.
const RETAINED_BYTES: usize = TAIL_BYTES + 4;

/// Returns the preview of append-only text with SGR removed. Unlike a capped
/// full replay, this retains the actual end of large color-only output.
///
/// Supported escapes are ESC `[` followed by zero or more ASCII digits, `;`,
/// or `:`, followed by `m`. CR is supported only immediately before LF. Every
/// other ESC sequence, malformed/incomplete SGR, standalone CR, and BS returns
/// `None`, including an unsupported sequence outside the retained tail.
///
/// Other characters are literal, matching the existing normalizer, including
/// controls that from_plain sanitizes. This is a display-only helper, never a
/// terminal emulator or a source for copying output or sending terminal input.
/// Time is O(input bytes); additional normalized storage is exactly 8,196 bytes
/// on the stack. Heap allocation is only from the bounded returned preview.
fn from_sgr_text(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let Some(mut special) = memchr::memchr3(0x1b, b'\r', b'\x08', bytes) else {
        return Some(from_plain(input));
    };

    let mut tail = NormalizedTail::new();
    let mut cursor = 0;
    loop {
        tail.extend(&bytes[cursor..special]);
        match bytes[special] {
            0x1b => {
                if bytes.get(special + 1) != Some(&b'[') {
                    return None;
                }
                cursor = special + 2;
                loop {
                    match bytes.get(cursor).copied() {
                        Some(b'0'..=b'9' | b';' | b':') => cursor += 1,
                        Some(b'm') => {
                            cursor += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
            }
            b'\r' if bytes.get(special + 1) == Some(&b'\n') => {
                // Keep the LF, but discard CR exactly as append-only replay
                // does. The next ordinary-text span includes this LF.
                cursor = special + 1;
            }
            _ => return None,
        }

        match memchr::memchr3(0x1b, b'\r', b'\x08', &bytes[cursor..]) {
            Some(offset) => special = cursor + offset,
            None => {
                tail.extend(&bytes[cursor..]);
                return Some(from_plain(tail.as_str()));
            }
        }
    }
}

struct NormalizedTail {
    bytes: [u8; RETAINED_BYTES],
    next: usize,
    len: usize,
}

impl NormalizedTail {
    fn new() -> Self {
        Self {
            bytes: [0; RETAINED_BYTES],
            next: 0,
            len: 0,
        }
    }

    fn extend(&mut self, bytes: &[u8]) {
        if bytes.len() >= RETAINED_BYTES {
            self.bytes
                .copy_from_slice(&bytes[bytes.len() - RETAINED_BYTES..]);
            self.next = 0;
            self.len = RETAINED_BYTES;
            return;
        }
        let first = bytes.len().min(RETAINED_BYTES - self.next);
        self.bytes[self.next..self.next + first].copy_from_slice(&bytes[..first]);
        self.bytes[..bytes.len() - first].copy_from_slice(&bytes[first..]);
        self.next = (self.next + bytes.len()) % RETAINED_BYTES;
        self.len = (self.len + bytes.len()).min(RETAINED_BYTES);
    }

    fn as_str(&mut self) -> &str {
        if self.len == RETAINED_BYTES {
            self.bytes.rotate_left(self.next);
            self.next = 0;
        }
        let bytes = &self.bytes[..self.len];
        // The retained suffix can start inside one scalar; all subsequent
        // bytes are valid UTF-8 because every removed sequence is ASCII and
        // starts and ends on a scalar boundary in the original &str.
        let start = bytes
            .iter()
            .position(|byte| byte & 0xc0 != 0x80)
            .unwrap_or(bytes.len());
        std::str::from_utf8(&bytes[start..]).expect("normalized UTF-8 suffix")
    }
}

#[cfg(test)]
mod ring_tests {
    use super::*;

    #[test]
    fn ring_has_fixed_storage_and_keeps_exact_utf8_suffix() {
        let mut all = String::new();
        let mut ring = NormalizedTail::new();
        for part in ["x".repeat(8193), "界🦀".repeat(2000), "y".repeat(8200)] {
            for text in part.split_inclusive('界') {
                all.push_str(text);
                ring.extend(text.as_bytes());
            }
            let mut start = all.len().saturating_sub(RETAINED_BYTES);
            while !all.is_char_boundary(start) {
                start += 1;
            }
            assert_eq!(ring.as_str(), &all[start..]);
        }
        assert_eq!(std::mem::size_of_val(&ring.bytes), 8196);
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;

    #[test]
    fn colored_tail_matches_plain_unicode_cutoffs_and_crlf() {
        for scalar in ["a", "é", "界", "🦀", "e\u{301}"] {
            for count in [0, 1, 119, 120, 121, 8191, 8192, 8193, 8196, 10_000] {
                for ending in ["", "\n", "\n \t\n"] {
                    let plain = format!("old prefix\n{}{ending}", scalar.repeat(count));
                    let colored =
                        format!("\x1b[38:2::255:0:1m{}\x1b[m", plain.replace('\n', "\r\n"));
                    assert_eq!(from_output(&colored), from_plain(&plain));
                }
            }
        }
        assert_eq!(
            from_output(&format!("secret\x1b[m\n{}", "\n".repeat(TAIL_BYTES))),
            ""
        );
        assert_eq!(
            from_output("\x1b[31m<b>literal</b>\u{202e}\x1b[m"),
            "<b>literal</b>�"
        );
    }

    #[test]
    fn validated_stream_matches_generated_plain_text_without_escape_fragments() {
        let pieces = [
            "a", "界", "🦀", "e\u{301}", " ", "\t", "\n", "\0", "\u{202e}",
        ];
        let mut seed = 0x1234_5678u64;
        for _ in 0..300 {
            let mut raw = String::new();
            let mut plain = String::new();
            for _ in 0..1500 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let text = pieces[(seed as usize) % pieces.len()];
                raw.push_str("\x1b[38:2::1:2:3m");
                raw.push_str(text);
                raw.push_str("\x1b[m");
                plain.push_str(text);
            }
            assert_eq!(from_output(&raw), from_plain(&plain));
        }
        for sequence in [
            "\x1b",
            "\x1b[",
            "\x1b[31",
            "\x1b[1 m",
            "\x1b[+1m",
            "\x1b[1\nm",
            "\x1b[🦀m",
        ] {
            assert!(from_sgr_text(&format!("{sequence}{}", "tail\n".repeat(2000))).is_none());
        }
    }

    #[test]
    fn large_colored_output_preview_uses_the_actual_last_row() {
        use std::fmt::Write;
        let mut output = String::new();
        for row in 0..100_000 {
            writeln!(
                &mut output,
                "\x1b[31mkeep row {row:06} {}\x1b[0m",
                "p".repeat(50)
            )
            .unwrap();
        }
        assert!(output.len() < 8 * 1024 * 1024);
        assert_eq!(
            from_output(&output),
            format!("keep row 099999 {}", "p".repeat(50))
        );
    }

    #[test]
    fn unsupported_controls_use_existing_replay_without_exposing_escape_payloads() {
        for input in [
            "abc\rX",
            "abc\x08X",
            "abc\x1b[2Klast",
            "one\n\x1b[1A\rTWO",
            "before\x1b]0;private title\x07\nlast",
            "\x1b[31",
            "\x1b[?25m",
        ] {
            assert!(from_sgr_text(input).is_none());
            assert_eq!(
                from_output(input),
                from_plain(&super::super::ansi::strip_ansi(input))
            );
        }
        for unsupported in ["\x1b]0;private\x07", "\x1b[2J", "\r", "\x08"] {
            let input = format!("{unsupported}{}", "safe tail\n".repeat(2000));
            assert!(
                from_sgr_text(&input).is_none(),
                "validation must include discarded prefixes"
            );
        }
        assert_eq!(from_output("\x1b]0;private title\x07visible"), "visible");
    }
}
