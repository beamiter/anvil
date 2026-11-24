mod tests {
    use jterm_core::link::{detect_delimited_references, DelimitedReference};

    fn detect(rows: &[(&str, bool)]) -> Vec<DelimitedReference> {
        let lines: Vec<_> = rows.iter().map(|&(line, _)| line).collect();
        detect_delimited_references(&lines)
    }

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
