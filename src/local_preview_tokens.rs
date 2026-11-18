//! Whole-token detection only: validation and local confirmation are separate.
pub const PATTERN: &str = r#"(?<![^[:space:]<>"'`()\[\]{}])[^[:space:]<>"'`()\[\]{}]+"#;

/// Conservative bare source-name hint, not permission or location parsing.
/// Numeric colon suffixes stay part of the exact filename; this does not strip
/// them or turn a missing file into another path.
pub fn is_bare_source_name(token: &str) -> bool {
    let mut parts = token.split(':');
    let name = parts.next().unwrap_or_default();
    if parts.any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return false;
    }
    if matches!(
        name,
        "README"
            | "LICENSE"
            | "Makefile"
            | "Dockerfile"
            | "Cargo.lock"
            | ".gitignore"
            | ".editorconfig"
    ) {
        return true;
    }
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && stem.chars().any(char::is_alphanumeric)
        && matches!(
            extension.to_ascii_lowercase().as_str(),
            "md" | "mdx"
                | "txt"
                | "json"
                | "jsonc"
                | "toml"
                | "yaml"
                | "yml"
                | "rs"
                | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "mjs"
                | "cjs"
                | "html"
                | "css"
                | "scss"
                | "svg"
                | "xml"
                | "py"
                | "sh"
                | "c"
                | "h"
                | "cpp"
                | "hpp"
                | "go"
                | "java"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bare_names_are_conservative_and_colon_suffixes_remain_literal() {
        for name in [
            "README.md",
            "package.json",
            "Cargo.toml",
            "App.tsx",
            "文档.md",
            "README",
            "Makefile",
            "README.md:12",
            "App.tsx:12:3",
        ] {
            assert!(is_bare_source_name(name), "{name}");
        }
        for word in [
            "hello",
            "done.",
            "example.com",
            "1.23",
            "read me.md",
            ".md",
            "../x.md",
            "src/App.tsx",
            "https://host/a.md",
            "mailto:a.md",
            "README.md:note",
            "App.tsx:",
            "App.tsx:12:bad",
            "README.md,",
            "$HOME.md",
        ] {
            assert!(!is_bare_source_name(word), "{word}");
        }
    }
    #[test]
    fn never_reinterprets_a_token_suffix_as_a_different_path() {
        // Exercise the same PCRE2 expression used by VTE, without a display.
        unsafe {
            let mut error = 0;
            let mut offset = 0;
            let code = pcre2_sys::pcre2_compile_8(
                PATTERN.as_ptr(),
                PATTERN.len(),
                0,
                &mut error,
                &mut offset,
                std::ptr::null_mut(),
            );
            assert!(!code.is_null(), "PCRE2 error {error} at {offset}");
            let data =
                pcre2_sys::pcre2_match_data_create_from_pattern_8(code, std::ptr::null_mut());
            assert!(!data.is_null());
            for token in [
                "$HOME/src/App.tsx",
                r"C:\src/App.tsx",
                "日本語/src/App.tsx",
                "src/App.tsx",
                "./README.md",
                "../private.md",
                "https://host/a.md",
                "/tmp/a.md",
            ] {
                let wrapped = format!("({token})");
                assert!(
                    pcre2_sys::pcre2_match_8(
                        code,
                        wrapped.as_ptr(),
                        wrapped.len(),
                        0,
                        0,
                        data,
                        std::ptr::null_mut()
                    ) > 0
                );
                let vector = pcre2_sys::pcre2_get_ovector_pointer_8(data);
                assert_eq!(&wrapped[*vector..*vector.add(1)], token);
            }
            pcre2_sys::pcre2_match_data_free_8(data);
            pcre2_sys::pcre2_code_free_8(code);
        }
    }
}
