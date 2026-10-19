//! A bounded, pane-owned compiled pattern for cooperative history searches.
//!
//! Continuing a scan or refreshing retained records must not compile the same
//! expression again. The cache owns one query, including a failed compilation;
//! it never owns terminal output or changes the scan's cursor/generation rules.

pub(super) const MAX_QUERY_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PatternOptions {
    pub(super) case_sensitive: bool,
    pub(super) regex: bool,
    pub(super) whole_word: bool,
}

struct Entry {
    query: String,
    options: PatternOptions,
    size_limit: usize,
    compiled: Result<regex::Regex, String>,
}

#[derive(Default)]
pub(super) struct PreparedSearch {
    entry: Option<Entry>,
    #[cfg(test)]
    compilations: usize,
}

impl PreparedSearch {
    pub(super) fn prepare(
        &mut self,
        query: &str,
        options: PatternOptions,
        size_limit: usize,
    ) -> Result<&regex::Regex, String> {
        // Apply the admission cap here as well as in the dialog: direct callers
        // and future entry points must never allocate or compile an unbounded
        // pattern. An oversized query is not retained in the cache.
        if query.len() > MAX_QUERY_BYTES {
            return Err("Query is too long (maximum 8 KiB).".into());
        }
        let reusable = self.entry.as_ref().is_some_and(|entry| {
            entry.query == query && entry.options == options && entry.size_limit == size_limit
        });
        if !reusable {
            // Release the old automaton before building the replacement, so
            // rapid edits do not temporarily retain two compiled expressions.
            self.entry = None;
            let pattern = if options.regex {
                query.to_owned()
            } else {
                regex::escape(query)
            };
            let compiled = regex::RegexBuilder::new(&pattern)
                .case_insensitive(!options.case_sensitive)
                .multi_line(true)
                .size_limit(size_limit)
                .build()
                .map_err(|error| error.to_string());
            #[cfg(test)]
            {
                self.compilations += 1;
            }
            self.entry = Some(Entry {
                query: query.to_owned(),
                options,
                size_limit,
                compiled,
            });
        }
        self.entry
            .as_ref()
            .expect("an admitted query always owns one entry")
            .compiled
            .as_ref()
            .map_err(Clone::clone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE_LIMIT: usize = 2 * 1024 * 1024;

    #[test]
    fn resumed_scans_and_refreshes_compile_the_query_once() {
        let mut cache = PreparedSearch::default();
        let options = PatternOptions {
            regex: true,
            ..PatternOptions::default()
        };
        for page in 0..200 {
            let pattern = cache
                .prepare(r"^error\s+\d+$", options, SIZE_LIMIT)
                .unwrap();
            assert!(pattern.is_match(&format!("ignored\nERROR {page}\nignored")));
            assert!(!pattern.is_match("error x"));
        }
        assert_eq!(cache.compilations, 1);
    }

    #[test]
    fn matching_options_and_query_changes_replace_one_entry() {
        let mut cache = PreparedSearch::default();
        let literal = PatternOptions::default();
        assert!(cache
            .prepare("a.b", literal, SIZE_LIMIT)
            .unwrap()
            .is_match("A.B"));
        assert!(!cache
            .prepare("a.b", literal, SIZE_LIMIT)
            .unwrap()
            .is_match("axb"));
        let sensitive = PatternOptions {
            case_sensitive: true,
            ..literal
        };
        assert!(!cache
            .prepare("a.b", sensitive, SIZE_LIMIT)
            .unwrap()
            .is_match("A.B"));
        let regex = PatternOptions {
            regex: true,
            ..sensitive
        };
        assert!(cache
            .prepare("a.b", regex, SIZE_LIMIT)
            .unwrap()
            .is_match("axb"));
        let words = PatternOptions {
            whole_word: true,
            ..regex
        };
        cache.prepare("a.b", words, SIZE_LIMIT).unwrap();
        // Whole-word boundaries are applied by the scanner, but changing that
        // intent must still produce the matching query identity.
        assert_eq!(cache.entry.as_ref().unwrap().options, words);
        cache.prepare("next", words, SIZE_LIMIT).unwrap();
        assert_eq!(cache.entry.as_ref().unwrap().query, "next");
        cache.prepare("a.b", literal, SIZE_LIMIT).unwrap();
        assert_eq!(
            cache.compilations, 6,
            "the old query was evicted, not accumulated"
        );
    }

    #[test]
    fn invalid_patterns_are_cached_and_recover_on_the_next_edit() {
        let mut cache = PreparedSearch::default();
        let options = PatternOptions {
            regex: true,
            ..PatternOptions::default()
        };
        let first = cache.prepare("[", options, SIZE_LIMIT).unwrap_err();
        for _ in 0..100 {
            assert_eq!(cache.prepare("[", options, SIZE_LIMIT).unwrap_err(), first);
        }
        assert_eq!(cache.compilations, 1);
        assert!(cache
            .prepare("[ab]", options, SIZE_LIMIT)
            .unwrap()
            .is_match("b"));
        assert_eq!(cache.compilations, 2);
    }

    #[test]
    fn the_byte_cap_precedes_allocation_and_compilation() {
        let mut cache = PreparedSearch::default();
        let options = PatternOptions::default();
        cache.prepare("ready", options, SIZE_LIMIT).unwrap();
        for query in [
            "x".repeat(MAX_QUERY_BYTES + 1),
            "界".repeat(MAX_QUERY_BYTES / 3 + 1),
        ] {
            assert!(cache
                .prepare(&query, options, SIZE_LIMIT)
                .unwrap_err()
                .contains("8 KiB"));
        }
        assert_eq!(cache.compilations, 1);
        assert_eq!(cache.entry.as_ref().unwrap().query, "ready");
        // Exactly 8 KiB is admitted (the independent regex-size limit may
        // still reject some expressions).
        assert!(cache
            .prepare(&"x".repeat(MAX_QUERY_BYTES), options, SIZE_LIMIT)
            .is_ok());
        assert_eq!(cache.compilations, 2);
    }

    #[test]
    fn a_tighter_regex_budget_cannot_reuse_a_wider_compilation() {
        let mut cache = PreparedSearch::default();
        let options = PatternOptions {
            regex: true,
            ..PatternOptions::default()
        };
        cache.prepare("[a-z]+", options, SIZE_LIMIT).unwrap();
        assert!(cache.prepare("[a-z]+", options, 1).is_err());
        assert_eq!(cache.compilations, 2);
        assert!(cache.prepare("[a-z]+", options, 1).is_err());
        assert_eq!(cache.compilations, 2);
    }
}
