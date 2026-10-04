//! Semantic ("smart") double-click selection for finished-block text views.
//!
//! GTK's default double-click selects a plain alnum word. This detects the
//! semantic token under the cursor — URL, path, file:line:col, IPv4, git SHA,
//! key=value, quoted string, … — so one double-click grabs the whole unit.
//! Ported from forge's `block_view/select.rs`.

use gtk::prelude::*;
use gtk::TextBuffer;
use regex::Regex;
use relm4::gtk;
use std::sync::LazyLock;

struct Pat {
    re: Regex,
    group: usize,
}

static PATTERNS: LazyLock<Vec<Pat>> = LazyLock::new(|| {
    let p = |s: &str, g: usize| Pat {
        re: Regex::new(s).unwrap(),
        group: g,
    };
    vec![
        p(r#""([^"\n]*)""#, 1),
        p(r#"'([^'\n]*)'"#, 1),
        p(r#"`([^`\n]*)`"#, 1),
        p(r#"((?:https?|ftp|file)://[^\s<>"'`)\]}]+)"#, 1),
        p(r#"([\w.+-]+@[\w-]+(?:\.[\w-]+)+)"#, 1),
        p(r#"((?:[~.]?[\w./+-]*\w):\d+(?::\d+)?)"#, 1),
        p(
            r#"((?:~|\.{1,2})?(?:/[\w.+@~-]+)+/?|(?:[\w.+-]+/)+[\w.+-]*)"#,
            1,
        ),
        p(r#"(\b\d{1,3}(?:\.\d{1,3}){3}(?::\d+)?)"#, 1),
        p(r#"([\w.-]+=[^\s'"]+)"#, 1),
        p(r#"(\b[0-9a-f]{7,40}\b)"#, 1),
        p(r#"(\b0x[0-9a-fA-F]+\b)"#, 1),
        p(r#"(\b\d+(?:\.\d+)?\b)"#, 1),
        p(r#"([\w@.+-]+)"#, 1),
    ]
});

fn semantic_span(line: &str, click_char: usize) -> Option<(usize, usize)> {
    let click_byte = line
        .char_indices()
        .nth(click_char)
        .map(|(b, _)| b)
        .unwrap_or(line.len());

    for pat in PATTERNS.iter() {
        for caps in pat.re.captures_iter(line) {
            if let Some(m) = caps.get(pat.group) {
                if m.start() <= click_byte && click_byte < m.end() {
                    let end_byte = trim_semantic_end(line, m.start(), m.end());
                    if click_byte >= end_byte {
                        continue;
                    }
                    let s = line[..m.start()].chars().count();
                    let e = line[..end_byte].chars().count();
                    return Some((s, e));
                }
            }
        }
    }
    None
}

/// Sentence punctuation is not part of a URL or email, even when the greedy
/// URI regex swallowed it. Colon stays: `https://host:8443` is one token.
fn trim_semantic_end(line: &str, start: usize, end: usize) -> usize {
    let token = &line[start..end];
    if !(token.contains("://") || token.contains('@')) {
        return end;
    }
    start
        + token
            .trim_end_matches(|c| matches!(c, '.' | ',' | ';' | '!' | '?'))
            .len()
}

/// Resolve the semantic token at `iter` to a pair of buffer iters to select.
pub fn get_semantic_bounds_at_position(
    buffer: &TextBuffer,
    iter: &gtk::TextIter,
) -> Option<(gtk::TextIter, gtk::TextIter)> {
    let mut line_start = *iter;
    line_start.set_line_offset(0);
    let mut line_end = *iter;
    if !line_end.ends_line() {
        line_end.forward_to_line_end();
    }
    let line_text = buffer.text(&line_start, &line_end, false).to_string();
    let click_char = iter.line_offset() as usize;

    let (s, e) = semantic_span(&line_text, click_char)?;

    let mut sel_start = line_start;
    sel_start.forward_chars(s as i32);
    let mut sel_end = line_start;
    sel_end.forward_chars(e as i32);
    Some((sel_start, sel_end))
}

#[cfg(test)]
mod tests {
    fn token(line: &str, click: usize) -> Option<String> {
        let (start, end) = super::semantic_span(line, click)?;
        Some(line.chars().take(end).skip(start).collect())
    }

    #[test]
    fn double_click_grabs_urls_paths_and_shas() {
        let url = "see https://example.com/a for details";
        assert_eq!(token(url, 6).as_deref(), Some("https://example.com/a"));
        assert_eq!(
            token("log src/main.rs:12:3 here", 6).as_deref(),
            Some("src/main.rs:12:3")
        );
        assert_eq!(
            token("commit deadbeef0123abc", 8).as_deref(),
            Some("deadbeef0123abc")
        );
        assert_eq!(token("ip 10.0.0.8:22 ok", 4).as_deref(), Some("10.0.0.8:22"));
    }

    #[test]
    fn trailing_sentence_punctuation_is_not_part_of_a_url_or_email() {
        assert_eq!(
            token("see https://example.com.", 6).as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            token("mail user@example.com, please", 6).as_deref(),
            Some("user@example.com")
        );
        assert_eq!(
            token("https://example.com:8443/x", 2).as_deref(),
            Some("https://example.com:8443/x")
        );
    }
}
