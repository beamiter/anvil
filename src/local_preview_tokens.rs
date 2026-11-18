//! Whole-token detection only: validation and local confirmation are separate.
pub const PATTERN: &str = r#"(?<![^[:space:]<>"'`()\[\]{}])[^[:space:]<>"'`()\[\]{}]+"#;

#[cfg(test)]
mod tests {
    use super::*;
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
