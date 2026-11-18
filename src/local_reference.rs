//! Pure parsing of an already hit-tested local reference, with no filesystem IO.
//! Hosts must try `exact_path` first; use `source_location` only after NotFound.
//! This is not authorization, token detection, URI decoding, or shell expansion.
use std::num::NonZeroU32;

pub const MAX_REFERENCE_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceLocationCandidate {
    pub path: String,
    pub line: NonZeroU32,
    pub column: Option<NonZeroU32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalReferenceCandidate {
    pub exact_path: String,
    pub source_location: Option<SourceLocationCandidate>,
}

/// Does not trim punctuation or quotes: the hit-testing layer owns exact spans.
/// Bare words are allowed here; detectors should require a plausible file token.
/// URI-like and Windows/UNC forms are intentionally outside this Unix contract.
pub fn parse_candidate(text: &str) -> Option<LocalReferenceCandidate> {
    if text.is_empty()
        || text.len() > MAX_REFERENCE_BYTES
        || text.chars().any(unsafe_character)
        || text.contains("://")
        || text.starts_with("//")
        || text.contains('\\')
    {
        return None;
    }
    if let Some((prefix, rest)) = text.split_once(':') {
        let scheme_like = prefix
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
            && prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
        let numeric_suffix = rest
            .split(':')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
        if scheme_like && !numeric_suffix {
            return None;
        }
    }
    Some(LocalReferenceCandidate {
        exact_path: text.to_owned(),
        source_location: source_location(text),
    })
}

fn source_location(text: &str) -> Option<SourceLocationCandidate> {
    let (prefix, last) = text.rsplit_once(':')?;
    let last = positive_decimal(last)?;
    if prefix.is_empty() {
        return None;
    }
    if let Some((path, line)) = prefix.rsplit_once(':') {
        if !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) {
            if path.is_empty() {
                return None;
            }
            return Some(SourceLocationCandidate {
                path: path.to_owned(),
                line: positive_decimal(line)?,
                column: Some(last),
            });
        }
    }
    Some(SourceLocationCandidate {
        path: prefix.to_owned(),
        line: last,
        column: None,
    })
}

fn positive_decimal(text: &str) -> Option<NonZeroU32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    NonZeroU32::new(text.parse().ok()?)
}

fn unsafe_character(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{00ad}' | '\u{034f}' | '\u{061c}' | '\u{115f}' | '\u{1160}'
            | '\u{17b4}' | '\u{17b5}' | '\u{180e}' | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{3164}'
            | '\u{feff}' | '\u{ffa0}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_local_shapes_without_expansion() {
        for path in [
            "README.md",
            "src/Foo.tsx",
            "./README.md",
            "../image.png",
            "~/notes.md",
            "/tmp/a b.md",
            "目录/文件.md",
        ] {
            let parsed = parse_candidate(path).unwrap();
            assert_eq!(parsed.exact_path, path);
            assert!(parsed.source_location.is_none());
        }
    }

    #[test]
    fn preserves_exact_colon_name_before_location_fallback() {
        let parsed = parse_candidate("src/Foo.tsx:42:7").unwrap();
        assert_eq!(parsed.exact_path, "src/Foo.tsx:42:7");
        let location = parsed.source_location.unwrap();
        assert_eq!(location.path, "src/Foo.tsx");
        assert_eq!(location.line.get(), 42);
        assert_eq!(location.column.unwrap().get(), 7);
        assert_eq!(parse_candidate("notes:12").unwrap().exact_path, "notes:12");
    }

    #[test]
    fn invalid_location_is_not_silently_clamped() {
        for path in [
            "a.md:0",
            "a.md:4294967296",
            "a.md:0:3",
            "a.md:3:0",
            "a.md:4294967296:3",
        ] {
            let parsed = parse_candidate(path).unwrap();
            assert_eq!(parsed.exact_path, path);
            assert!(parsed.source_location.is_none());
        }
    }

    #[test]
    fn rejects_uri_control_spoof_and_oversize_candidates() {
        for path in [
            "",
            "file:///tmp/a",
            "https://example.com/a",
            "ssh://host/a",
            "javascript:alert(1)",
            "data:text/plain,a",
            "//host/a",
            "C:\\a",
            "a\0b",
            "a\nb",
            "a\u{202e}b",
            "a\u{200b}b",
        ] {
            assert!(parse_candidate(path).is_none(), "{path:?}");
        }
        assert!(parse_candidate(&"a".repeat(MAX_REFERENCE_BYTES)).is_some());
        assert!(parse_candidate(&"a".repeat(MAX_REFERENCE_BYTES + 1)).is_none());
    }
}
