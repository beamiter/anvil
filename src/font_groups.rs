//! Pure two-group zoom state. The legacy scale remains the content scale.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FontGroup {
    Ui,
    Content,
}

pub(crate) fn ui_scale(content: f64, ui: Option<f64>) -> f64 {
    ui.unwrap_or(content)
}

/// Snapshot the legacy fallback before changing either group. Otherwise a
/// content-only change would silently change an unset UI scale as well.
pub(crate) fn set_scale(content: &mut f64, ui: &mut Option<f64>, group: FontGroup, scale: f64) {
    if !scale.is_finite() {
        return;
    }
    let scale = scale.clamp(0.1, 10.0);
    let previous_ui = ui_scale(*content, *ui);
    *ui = Some(previous_ui);
    match group {
        FontGroup::Ui => *ui = Some(scale),
        FontGroup::Content => *content = scale,
    }
}

/// Horizontal-only, invalid, zero, and ordinary scrolling retain GTK behavior.
pub(crate) fn wheel_step(control: bool, dy: f64) -> Option<f64> {
    (control && dy.is_finite() && dy != 0.0).then_some(-dy * 0.025)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_content_change_snapshots_ui_and_survives_reload() {
        let (mut content, mut ui) = (1.5, None);
        assert_eq!(ui_scale(content, ui), 1.5);
        set_scale(&mut content, &mut ui, FontGroup::Content, 1.75);
        assert_eq!((content, ui), (1.75, Some(1.5)));
        // Persisted pair is also the complete state used on reload.
        let (mut content, mut ui) = (content, ui);
        set_scale(&mut content, &mut ui, FontGroup::Ui, 2.0);
        assert_eq!((content, ui), (1.75, Some(2.0)));
    }

    #[test]
    fn legacy_ui_change_leaves_content_unchanged() {
        let (mut content, mut ui) = (1.5, None);
        set_scale(&mut content, &mut ui, FontGroup::Ui, 2.0);
        assert_eq!((content, ui), (1.5, Some(2.0)));
        set_scale(&mut content, &mut ui, FontGroup::Content, 1.0);
        assert_eq!((content, ui), (1.0, Some(2.0)));
    }

    #[test]
    fn repeated_steps_and_limits_never_change_other_group() {
        for group in [FontGroup::Ui, FontGroup::Content] {
            let (mut content, mut ui) = (1.0, Some(2.0));
            for scale in [-5.0, 50.0, 0.1, 10.0, 1.25, f64::NAN, f64::INFINITY] {
                set_scale(&mut content, &mut ui, group, scale);
                match group {
                    FontGroup::Ui => assert_eq!(content, 1.0),
                    FontGroup::Content => assert_eq!(ui, Some(2.0)),
                }
                assert!((0.1..=10.0).contains(&content));
                assert!((0.1..=10.0).contains(&ui.unwrap()));
            }
        }
    }

    #[test]
    fn only_finite_control_vertical_scroll_is_consumed() {
        for control in [false, true] {
            for dy in [0.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                assert_eq!(wheel_step(control, dy), None);
            }
        }
        assert_eq!(wheel_step(false, -1.0), None);
        assert_eq!(wheel_step(true, -0.001), Some(0.000025));
        assert!((wheel_step(true, 3.0).unwrap() + 0.075).abs() < 1e-12);
    }
    #[test]
    fn group_wiring_uses_disjoint_capture_roots_and_preserves_native_popup_boundary() {
        let main = include_str!("main.rs");
        let routing = main
            .split("// Disjoint capture roots")
            .nth(1)
            .unwrap()
            .split("// Config file hot reload")
            .next()
            .unwrap();
        assert!(routing.contains("top_bar_handle.clone()"));
        assert!(routing.contains("sidebar_box.clone()"));
        assert!(routing.contains("side_stack.clone()"));
        assert!(routing.contains("model.shell.canvas_header.clone()"));
        let compact: String = routing.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("model.shell.block_composer_chrome.clone()"));
        assert!(routing.contains("model.shell.inspector.clone()"));
        assert!(routing.contains("stack.clone()"));
        assert!(!routing.contains("ai_paned.clone()"));
        assert!(routing.contains("jterm_core::wheel::SURFACE_UNITS_PER_STEP"));
        assert!(routing.contains("gtk::PropagationPhase::Capture"));
        assert!(routing.contains("gtk::PropagationLimit::SameNative"));
        assert!(routing.contains("glib::Propagation::Stop"));
        assert!(!routing.contains(".focus("));
        assert!(!main.contains("root.add_controller(scroll_controller)"));
        let settings = include_str!("settings_ops.rs");
        let group_step = settings
            .split("pub(crate) fn apply_group_font_step")
            .nth(1)
            .unwrap()
            .split("pub(crate) fn apply_settings_ui_font_size")
            .next()
            .unwrap();
        assert!(group_step.contains("if group == font_groups::FontGroup::Content"));
        assert!(group_step.contains("self.set_font_scale_all(scale)"));
        assert_eq!(
            settings
                .matches("font::freeze_ui_font(&mut config)")
                .count(),
            5
        );
        let dialog = include_str!("dialogs/settings.rs");
        assert!(dialog.contains("self.font_syncing.set(true)"));
        assert!(dialog.contains("self.font_syncing.set(false)"));
        assert_eq!(
            dialog
                .matches("syncing = model.font_syncing.clone()")
                .count(),
            5
        );
    }
    #[test]
    fn continuous_interleaved_wheels_remain_independent_at_both_limits() {
        let (mut content, mut ui) = (1.0, None);
        for _ in 0..1000 {
            let next = content + wheel_step(true, -1.0).unwrap();
            set_scale(&mut content, &mut ui, FontGroup::Content, next);
            assert_eq!(ui, Some(1.0));
        }
        assert_eq!(content, 10.0);
        for _ in 0..1000 {
            let next = ui.unwrap() + wheel_step(true, 1.0).unwrap();
            set_scale(&mut content, &mut ui, FontGroup::Ui, next);
            assert_eq!(content, 10.0);
        }
        assert_eq!(ui, Some(0.1));
    }
    #[test]
    fn smooth_and_batched_notches_preserve_magnitude_without_cross_group_residue() {
        let small: f64 = (0..100).map(|_| wheel_step(true, -0.01).unwrap()).sum();
        assert!((small - wheel_step(true, -1.0).unwrap()).abs() < 1e-12);
        assert!((wheel_step(true, -3.0).unwrap() - 3.0 * small).abs() < 1e-12);
        let (mut content, mut ui) = (1.0, Some(1.0));
        for _ in 0..100 {
            let next = content + wheel_step(true, -0.01).unwrap();
            set_scale(&mut content, &mut ui, FontGroup::Content, next);
            let next = ui.unwrap() + wheel_step(true, 0.01).unwrap();
            set_scale(&mut content, &mut ui, FontGroup::Ui, next);
        }
        assert!((content - 1.025).abs() < 1e-12);
        assert!((ui.unwrap() - 0.975).abs() < 1e-12);
    }
}
