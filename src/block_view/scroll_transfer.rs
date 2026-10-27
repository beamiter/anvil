//! Preserve gesture distance when a capped VTE hands off to the outer canvas.
//!
//! VTE adjustments use rows; the outer canvas uses pixels (or wheel notches).
//! Compute consumption from the *actual* adjustment after GTK clamps it, not
//! from the requested target. A boundary crossed mid-event must not swallow
//! the rest of that event, nor replay the already-consumed part outside.

pub(super) fn remaining_delta(delta: f64, before: f64, after: f64, rows_per_unit: f64) -> f64 {
    if !delta.is_finite() || delta == 0.0 {
        return 0.0;
    }
    if !before.is_finite()
        || !after.is_finite()
        || !rows_per_unit.is_finite()
        || rows_per_unit <= 0.0
    {
        return delta;
    }
    let consumed = ((after - before) / rows_per_unit).clamp(delta.min(0.0), delta.max(0.0));
    let remaining = delta - consumed;
    if remaining.abs() <= 1.0e-9_f64.max(delta.abs() * 1.0e-12) {
        0.0
    } else {
        remaining
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touchpad_crossing_bottom_preserves_unconsumed_pixels() {
        // Only half a row (10 px) remains, but the gesture moves 50 px.
        assert_eq!(remaining_delta(50.0, 99.5, 100.0, 1.0 / 20.0), 40.0);
    }

    #[test]
    fn touchpad_crossing_top_preserves_sign_and_fraction() {
        assert_eq!(remaining_delta(-12.5, 0.25, 0.0, 1.0 / 20.0), -7.5);
    }

    #[test]
    fn no_inner_range_passes_entire_event_and_full_consumption_passes_none() {
        assert_eq!(remaining_delta(14.25, 0.0, 0.0, 0.05), 14.25);
        assert_eq!(remaining_delta(-14.25, 5.0, 4.2875, 0.05), 0.0);
    }

    #[test]
    fn wheel_boundary_passes_fraction_of_notch_not_rows_or_pixels() {
        assert_eq!(remaining_delta(2.0, 98.0, 100.0, 4.0), 1.5);
        assert_eq!(remaining_delta(-2.0, 2.0, 0.0, 4.0), -1.5);
    }

    #[test]
    fn external_adjustment_changes_cannot_reverse_or_amplify_a_gesture() {
        assert_eq!(remaining_delta(20.0, 10.0, 8.0, 0.05), 20.0);
        assert_eq!(remaining_delta(20.0, 10.0, 50.0, 0.05), 0.0);
        assert_eq!(remaining_delta(-20.0, 10.0, 50.0, 0.05), -20.0);
    }

    #[test]
    fn invalid_input_never_poisons_the_outer_motion() {
        for delta in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0] {
            assert_eq!(remaining_delta(delta, 0.0, 1.0, 1.0), 0.0);
        }
        for rows in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            assert_eq!(remaining_delta(12.0, 0.0, 1.0, rows), 12.0);
        }
    }

    #[test]
    fn repeated_boundary_crossings_conserve_distance_at_different_font_sizes() {
        for height in [12.0_f64, 17.5, 24.0, 32.0] {
            for delta in [-99.25, -3.75, 3.75, 99.25] {
                let before = if delta < 0.0 { 0.125 } else { 99.875 };
                let after = (before + delta / height).clamp(0.0, 100.0);
                let remainder = remaining_delta(delta, before, after, 1.0 / height);
                let consumed_pixels = (after - before) * height;
                assert!((consumed_pixels + remainder - delta).abs() < 1.0e-9);
            }
        }
    }
}
