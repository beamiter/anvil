//! Query preparation shared by terminal presentation backends.
use crate::search::invalid_regex_message;

pub(crate) fn search_pattern(query: &str, use_regex: bool) -> String {
    if use_regex {
        query.to_string()
    } else {
        regex::escape(query)
    }
}

/// Compile the Rust-regex mirror used by Block search and by VTE result
/// counting. VTE installs its native PCRE2 regex first: a PCRE2 feature such as
/// look-around may be uncountable here, but it remains searchable and is shown
/// with an explicitly inexact `+` total instead of being rejected.
pub(crate) fn compile_count_regex(
    pattern: &str,
    case_sensitive: bool,
) -> Result<regex::Regex, String> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        // Native terminal search uses PCRE2_MULTILINE. Keep anchors aligned in
        // the mirror counter; regex totals remain marked approximate because
        // the two engines still differ in other grammar and semantics.
        .multi_line(true)
        .build()
        .map_err(invalid_regex_message)
}
