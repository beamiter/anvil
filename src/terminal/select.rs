//! Semantic ("smart") double-click selection for finished-block text views.
//!
//! GTK's default double-click selects a plain alnum word. This detects the
//! semantic token under the cursor — URL, path, file:line:col, IPv4, IPv6,
//! MAC, ISO-8601 timestamp, semver, duration, percent, git SHA, key=value, quoted string, … — so one double-click grabs the whole unit.
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
        p(
            r#"(\$\{[A-Za-z_][A-Za-z0-9_]*\}|\$[A-Za-z_][A-Za-z0-9_]*|\$[0-9]+)"#,
            1,
        ),
        p(
            r#"(\b\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2}(?:\.\d{1,9})?)?(?:Z|[+-]\d{2}:?\d{2})?)?)"#,
            1,
        ),
        p(r#"(\[[0-9a-fA-F:.]{2,}\](?::\d+)?)"#, 1),
        p(
            r#"((?:[0-9a-fA-F]{1,4}:){7}[0-9a-fA-F]{1,4}|[0-9a-fA-F]{0,4}(?::[0-9a-fA-F]{0,4})*::(?:[0-9a-fA-F]{1,4}:)*[0-9a-fA-F]{0,4})"#,
            1,
        ),
        p(
            r#"(\b[0-9a-fA-F]{2}(?::[0-9a-fA-F]{2}){5}\b|\b[0-9a-fA-F]{2}(?:-[0-9a-fA-F]{2}){5}\b)"#,
            1,
        ),
        p(r#"((?:[~.]?[\w./+-]*\w):\d+(?::\d+)?)"#, 1),
        p(r#"([\w./-]+:[A-Za-z0-9][\w.-]*)"#, 1),
        p(
            r#"((?:~|\.{1,2})?(?:/[\w.+@~-]+)+/?|(?:[\w.+-]+/)+[\w.+-]*)"#,
            1,
        ),
        p(r#"(\b\d{1,3}(?:\.\d{1,3}){3}(?::\d+)?)"#, 1),
        p(
            r#"(\bv?\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?)"#,
            1,
        ),
        p(r#"([\w.-]+=[^\s'"]+)"#, 1),
        p(r#"(#[0-9a-fA-F]{3,8})\b"#, 1),
        p(
            r#"(\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b)"#,
            1,
        ),
        p(r#"(\b[0-9a-f]{7,40}\b)"#, 1),
        p(r#"(\b0x[0-9a-fA-F]+\b)"#, 1),
        p(r#"(\b\d+(?:\.\d+)?(?:ns|us|µs|ms|s)\b)"#, 1),
        p(r#"(\b\d+(?:\.\d+)?%)"#, 1),
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
                    if file_line_is_semver_prefix(line, m.start(), m.end()) {
                        continue;
                    }
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

/// Sentence punctuation and wrapping closers are not part of a URL or email,
/// even when the greedy URI regex swallowed them. A port stays:
/// `https://host:8443` does not end with `:`.
pub(crate) fn trim_link_trail(text: &str) -> &str {
    text.trim_end_matches(|c| {
        matches!(
            c,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '>' | '\'' | '"'
        )
    })
}

/// Opening wrappers around a whitespace-delimited URL token (`(https://…)`,
/// `"https://…"`). Closers are handled by [`trim_link_trail`].
pub(crate) fn strip_link_wrappers(text: &str) -> &str {
    text.trim_start_matches(|c| matches!(c, '(' | '[' | '{' | '<' | '"' | '\''))
}

/// Sentence punctuation is not part of a path, `file:line`, or `key=value`
/// token. Colon stays: `src/main.rs:12` is one token.
pub(crate) fn trim_path_trail(text: &str) -> &str {
    text.trim_end_matches(|c| matches!(c, '.' | ',' | ';' | '!' | '?'))
}

fn trim_semantic_end(line: &str, start: usize, end: usize) -> usize {
    let token = &line[start..end];
    if token.contains("://") || token.contains('@') {
        return start + trim_link_trail(token).len();
    }
    if token.contains('/')
        || token.contains('.')
        || token.contains('=')
        || token.matches(':').count() >= 2
        || token.matches('-').count() >= 2
    {
        return start + trim_path_trail(token).len();
    }
    end
}

/// `nginx:1.27` matches `file:line` as `nginx:1` because the line number
/// stops at digits. A following `.digit` means this is a version tag, not a
/// location.
fn file_line_is_semver_prefix(line: &str, start: usize, end: usize) -> bool {
    let token = &line[start..end];
    let Some((_, line_no)) = token.rsplit_once(':') else {
        return false;
    };
    if !line_no.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    matches!(line.as_bytes().get(end), Some(b'.'))
        && matches!(line.as_bytes().get(end + 1), Some(b'0'..=b'9'))
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
        assert_eq!(
            token("color #ff00aa in css", 8).as_deref(),
            Some("#ff00aa")
        );
        assert_eq!(token("short #fff end", 8).as_deref(), Some("#fff"));
        assert_eq!(
            token("alpha #11223344 done", 8).as_deref(),
            Some("#11223344")
        );
        assert_eq!(token("echo $HOME/bin", 6).as_deref(), Some("$HOME"));
        assert_eq!(token("use ${PATH} here", 6).as_deref(), Some("${PATH}"));
        assert_eq!(token("arg $1 remaining", 5).as_deref(), Some("$1"));
        assert_eq!(
            token("id 550e8400-e29b-41d4-a716-446655440000 ok", 8).as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            token("image nginx:1.27-alpine pull", 8).as_deref(),
            Some("nginx:1.27-alpine")
        );
        assert_eq!(
            token("run ghcr.io/org/app:v2.1.0 now", 10).as_deref(),
            Some("ghcr.io/org/app:v2.1.0")
        );
        assert_eq!(token("ping ::1 ok", 6).as_deref(), Some("::1"));
        assert_eq!(
            token("addr 2001:db8::1 here", 8).as_deref(),
            Some("2001:db8::1")
        );
        assert_eq!(
            token("listen [::1]:8080 now", 8).as_deref(),
            Some("[::1]:8080")
        );
        assert_eq!(
            token("full 2001:0db8:85a3:0000:0000:8a2e:0370:7334 x", 7).as_deref(),
            Some("2001:0db8:85a3:0000:0000:8a2e:0370:7334")
        );
        assert_eq!(
            token("ether aa:bb:cc:dd:ee:ff up", 6).as_deref(),
            Some("aa:bb:cc:dd:ee:ff")
        );
        assert_eq!(
            token("hw 00-1A-2B-3C-4D-5E nic", 4).as_deref(),
            Some("00-1A-2B-3C-4D-5E")
        );
        assert_eq!(
            token("at 2026-10-04T10:59:48Z x", 4).as_deref(),
            Some("2026-10-04T10:59:48Z")
        );
        assert_eq!(
            token("day 2026-10-04 end", 5).as_deref(),
            Some("2026-10-04")
        );
        assert_eq!(
            token("ts 2026-10-04 10:59:48 x", 4).as_deref(),
            Some("2026-10-04 10:59:48")
        );
        assert_eq!(
            token("off 2026-10-04T10:59:48+08:00 x", 5).as_deref(),
            Some("2026-10-04T10:59:48+08:00")
        );
        assert_eq!(token("ver v1.2.3 ok", 4).as_deref(), Some("v1.2.3"));
        assert_eq!(
            token("rel 1.2.3-rc.1 x", 4).as_deref(),
            Some("1.2.3-rc.1")
        );
        assert_eq!(
            token("build 1.2.3+meta.4 x", 7).as_deref(),
            Some("1.2.3+meta.4")
        );
        assert_eq!(token("ip 10.0.0.8:22 ok", 4).as_deref(), Some("10.0.0.8:22"));
        assert_eq!(token("cpu 80% idle", 4).as_deref(), Some("80%"));
        assert_eq!(token("load 12.5% now", 6).as_deref(), Some("12.5%"));
        assert_eq!(token("took 12ms later", 6).as_deref(), Some("12ms"));
        assert_eq!(token("wait 1.5s end", 6).as_deref(), Some("1.5s"));
        assert_eq!(token("spin 100ns x", 6).as_deref(), Some("100ns"));
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
        assert_eq!(
            token("see https://example.com:", 6).as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            token("https://example.com:8443:", 2).as_deref(),
            Some("https://example.com:8443")
        );
        assert_eq!(
            token("see src/main.rs.", 6).as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            token("open ./foo/bar, please", 6).as_deref(),
            Some("./foo/bar")
        );
        assert_eq!(
            token("log src/main.rs:12:3.", 6).as_deref(),
            Some("src/main.rs:12:3")
        );
        assert_eq!(
            token("export FOO=bar, please", 10).as_deref(),
            Some("FOO=bar")
        );
        assert_eq!(
            token("retry COUNT=3;", 8).as_deref(),
            Some("COUNT=3")
        );
        assert_eq!(token("ping ::1.", 6).as_deref(), Some("::1"));
        assert_eq!(
            token("ether aa:bb:cc:dd:ee:ff.", 6).as_deref(),
            Some("aa:bb:cc:dd:ee:ff")
        );
        assert_eq!(
            token("hw 00-1A-2B-3C-4D-5E.", 4).as_deref(),
            Some("00-1A-2B-3C-4D-5E")
        );
        assert_eq!(
            token("at 2026-10-04T10:59:48Z.", 4).as_deref(),
            Some("2026-10-04T10:59:48Z")
        );
        assert_eq!(token("ver v1.2.3.", 4).as_deref(), Some("v1.2.3"));
        assert_eq!(token("cpu 80%.", 4).as_deref(), Some("80%"));
        assert_eq!(token("took 12ms.", 6).as_deref(), Some("12ms"));
    }
}
