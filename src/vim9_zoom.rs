//! GTK-independent Vim font shortcut and input-target policy.

pub(crate) fn key_step(
    character: Option<char>,
    control: bool,
    alternate: bool,
) -> Option<Option<f64>> {
    if !control || alternate {
        return None;
    }
    match character? {
        '+' | '=' => Some(Some(1.0)),
        '-' => Some(Some(-1.0)),
        '0' => Some(None),
        _ => None,
    }
}

pub(crate) fn owns_keyboard(is_zoom_key: bool, editor_focused: bool) -> bool {
    is_zoom_key && editor_focused
}

pub(crate) fn owns_wheel(control: bool, editor_under_pointer: bool) -> bool {
    control && editor_under_pointer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_zoom_keys_do_not_change_shell_or_native_field_fonts() {
        for (key, expected) in [
            ('+', Some(1.0)),
            ('=', Some(1.0)),
            ('-', Some(-1.0)),
            ('0', None),
        ] {
            let step = key_step(Some(key), true, false);
            assert_eq!(step, Some(expected));
            assert!(owns_keyboard(step.is_some(), true));
            assert!(!owns_keyboard(step.is_some(), false));
            assert_eq!(key_step(Some(key), false, false), None);
            assert_eq!(key_step(Some(key), true, true), None);
        }
        assert!(!owns_keyboard(
            key_step(Some('p'), true, false).is_some(),
            true
        ));
        assert_eq!(key_step(None, true, false), None);
    }

    #[test]
    fn mouse_zoom_uses_hover_instead_of_keyboard_focus() {
        for editor_focused in [false, true] {
            for editor_hovered in [false, true] {
                assert_eq!(owns_keyboard(true, editor_focused), editor_focused);
                assert_eq!(owns_wheel(true, editor_hovered), editor_hovered);
                assert!(!owns_wheel(false, editor_hovered));
            }
        }
    }
}
