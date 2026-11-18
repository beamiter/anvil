//! Recover the parenthesized destinations that a TUI may wrap before printing.
use std::ops::Range;
use std::sync::LazyLock;

const MAX_ROWS: usize = 8;
const MAX_BYTES: usize = 4096;

pub(crate) struct Reference {
    pub target: String,
    pub web: bool,
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
        let mut spans = Vec::new();
        let mut target = String::new();
        for (index, fragment) in raw.as_str().split('\n').enumerate() {
            let row = first_row + index;
            let fragment_offset = if index == 0 {
                raw.start() - row_starts[row]
            } else {
                0
            };
            let trimmed = fragment.trim();
            if trimmed.is_empty() || index >= MAX_ROWS {
                break;
            }
            // A full row or an explicit token break is evidence of wrapping.
            // A short row ending in prose must not capture the next line.
            if index != 0 && !rows[row - 1].1 && !target.ends_with(['-', '/', '_', '.']) {
                break;
            }
            let start = fragment_offset + fragment.len() - fragment.trim_start().len();
            spans.push((row, start..start + trimmed.len()));
            target.push_str(trimmed);
        }
        if spans.len() != raw.as_str().bytes().filter(|&b| b == b'\n').count() + 1
            || target.chars().any(char::is_control)
        {
            continue;
        }
        let web = target.split_once("://").is_some_and(|(scheme, _)| {
            scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
        });
        if web {
            if spans.len() == 1 {
                continue;
            }
        } else if target.contains("://")
            || target.contains(['\\', '<', '>', '|'])
            || !FILE_END.is_match(&target)
            || (spans.len() == 1 && !target.contains(char::is_whitespace))
        {
            continue;
        }
        references.push(Reference { target, web, spans });
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
            "/tmp/中 文/PMG-SOTA-roadmap.canvas.tsx"
        );
        assert_eq!(references[0].spans[1], (1, 4..22));
        assert_eq!(references[1].target, "reports/接触 优化.md");
    }

    #[test]
    fn joins_only_with_a_supported_boundary_and_delimiter() {
        assert!(detect(&[("(/tmp/first", false), ("second.md)", false)]).is_empty());
        assert!(detect(&[("(/tmp/first-", false), ("second.md", false)]).is_empty());
        let rejected = detect(&[("(https://user@host/a-", false), ("b)", false)]);
        assert!(!jterm_core::link::is_openable_url(&rejected[0].target));
        let links = detect(&[("(https://example.org/one-", false), ("two)", false)]);
        assert!(links[0].web);
        assert_eq!(links[0].target, "https://example.org/one-two");
    }
}
