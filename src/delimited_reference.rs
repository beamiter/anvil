//! Recover the parenthesized destinations that a TUI may wrap before printing.
use std::ops::Range;
use std::sync::LazyLock;

const MAX_BYTES: usize = 4096;

pub(crate) struct Reference {
    pub target: String,
    pub web: bool,
    /// Reserve the visible region without authorizing a guessed target.
    pub blocked: bool,
    pub spans: Vec<(usize, Range<usize>)>,
}

pub(crate) fn detect(rows: &[(&str, bool)]) -> Vec<Reference> {
    static DESTINATION: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"\(((?:(?i:https?)://|(?:\.{0,2}|~)/|[\p{L}\p{N}_][\p{L}\p{N}\p{M}_.-]*/)[^()\r\n]*(?:\n[^()\r\n]+){0,7})\)",
        )
        .unwrap()
    });
    static FILE_END: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?i-u:\.(?:canvas\.tsx|tsx|jsx|mdx?|txt|rs|py|jsonc?|toml|ya?ml|css|html|svg|png|jpe?g|webp|ts|js|pdf|csv|sh))(?::[0-9]+){0,2}$",
        )
        .unwrap()
    });

    if !rows.iter().any(|(text, _)| text.contains('(')) {
        return Vec::new();
    }
    let mut text = String::new();
    let mut row_starts = Vec::with_capacity(rows.len());
    for (row, _) in rows {
        row_starts.push(text.len());
        text.push_str(row);
        text.push('\n');
    }

    let mut references = Vec::new();
    for matched in DESTINATION.captures_iter(&text) {
        let raw = matched.get(1).unwrap();
        if raw.len() > MAX_BYTES {
            continue;
        }
        let first_row = row_starts.partition_point(|&start| start <= raw.start()) - 1;
        let is_web = raw.as_str().split_once("://").is_some_and(|(scheme, _)| {
            scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
        });
        // A full-looking physical row is not reliable soft-wrap metadata.
        // Preserve raw bytes and reserve every ambiguous region BEFORE format
        // validation, so neither a file nor a URL can fall back to a fragment.
        let spans = raw
            .as_str()
            .split('\n')
            .enumerate()
            .map(|(index, fragment)| {
                let row = first_row + index;
                let start = if index == 0 {
                    raw.start() - row_starts[row]
                } else {
                    0
                };
                (row, start..start + fragment.len())
            })
            .collect::<Vec<_>>();
        let target = raw.as_str().to_owned();
        let blocked = spans.len() > 1
            || target.chars().any(char::is_control)
            || if is_web {
                !jterm_core::link::is_openable_url(&target)
            } else {
                target.contains("://")
                    || target.contains(['\\', '<', '>', '|'])
                    || !FILE_END.is_match(&target)
            };
        // Unambiguous single-row URLs remain owned by the native URL matcher.
        // Only exact single-row local paths with spaces need extra activation.
        if blocked || (!is_web && target.contains(char::is_whitespace)) {
            references.push(Reference {
                target,
                web: is_web,
                blocked,
                spans,
            });
        }
    }
    references
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_delimited_paths_with_wide_text_and_real_spaces() {
        let rows = [
            ("路线图 (/tmp/中 文/PMG-SOTA-", false),
            (
                "    roadmap.canvas.tsx) · 报告 (reports/接触 优化.md)",
                false,
            ),
        ];
        let references = detect(&rows);
        assert_eq!(references.len(), 2);
        assert_eq!(
            references[0].target,
            "/tmp/中 文/PMG-SOTA-\n    roadmap.canvas.tsx"
        );
        assert!(references[0].blocked);
        assert_eq!(references[0].spans[1], (1, 0..22));
        assert!(!references[1].blocked);
        assert_eq!(references[1].target, "reports/接触 优化.md");
    }

    #[test]
    fn ambiguous_local_rows_never_select_the_other_real_file() {
        let dir = std::env::temp_dir().join(format!("anvil-delimited-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exact = dir.join("foo-  bar.md");
        let compact = dir.join("foo-bar.md");
        std::fs::write(&exact, "EXACT SPACES").unwrap();
        std::fs::write(&compact, "WRONG COMPACT").unwrap();
        let first = format!("({}/foo- ", dir.display());
        for fills in [false, true] {
            let references = detect(&[(&first, fills), (" bar.md)", false)]);
            assert_eq!(references.len(), 1);
            assert!(references[0].blocked, "width is not soft-wrap evidence");
            assert_ne!(references[0].target, compact.to_str().unwrap());
            assert!(references[0].target.contains("- \n "));
        }
        let single = format!("({})", exact.display());
        let references = detect(&[(&single, false)]);
        assert!(!references[0].blocked);
        assert_eq!(references[0].target, exact.to_str().unwrap());
        match crate::local_preview_content::read_confirmed(std::path::Path::new(
            &references[0].target,
        ))
        .unwrap()
        {
            crate::local_preview_content::Content::Text(text) => assert_eq!(text, "EXACT SPACES"),
            _ => panic!("expected literal source"),
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_multiline_groups_still_reserve_partial_match_spans() {
        for tail in ["sub/file.bin)", "sub/file.md )", " sub/file.md)"] {
            for full_width in [false, true] {
                let references = detect(&[("(/tmp/dir-", full_width), (tail, false)]);
                assert_eq!(references.len(), 1, "must not lose blocking region: {tail}");
                assert!(references[0].blocked);
                assert_eq!(references[0].spans.len(), 2);
                assert_eq!(references[0].spans[1], (1, 0..tail.len() - 1));
            }
        }
    }

    #[test]
    fn url_host_boundaries_are_never_trimmed_into_another_origin() {
        for full_width in [false, true] {
            for (head, tail) in [
                ("(https://safe. ", " example/path)"),
                ("(https://safe.", "example/path)"),
                ("(https://safe.example/", " https://other.example/x)"),
                ("(https://safe.example/", "file.bin )"),
            ] {
                let refs = detect(&[(head, full_width), (tail, false)]);
                assert_eq!(refs.len(), 1);
                assert!(refs[0].web && refs[0].blocked);
                assert_eq!(
                    refs[0].target,
                    format!("{}\n{}", &head[1..], &tail[..tail.len() - 1])
                );
                assert_eq!(refs[0].spans[1], (1, 0..tail.len() - 1));
            }
        }
        assert!(detect(&[("(https://safe.example/path)", false)]).is_empty());
        assert!(detect(&[("(https://safe. example/path)", false)])[0].blocked);
    }

    #[test]
    fn joins_only_with_a_supported_boundary_and_delimiter() {
        assert!(detect(&[("(/tmp/first", false), ("second.md)", false)])[0].blocked);
        assert!(detect(&[("(/tmp/first-", false), ("second.md", false)]).is_empty());
        let rejected = detect(&[("(https://user@host/a-", false), ("b)", false)]);
        assert!(!jterm_core::link::is_openable_url(&rejected[0].target));
        let links = detect(&[("(https://example.org/one-", false), ("two)", false)]);
        assert!(links[0].web);
        assert!(links[0].blocked);
        assert_eq!(links[0].target, "https://example.org/one-\ntwo");
    }
}
