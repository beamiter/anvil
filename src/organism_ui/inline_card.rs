//! A bounded paint surface for the inline companion's native label subtree.
//!
//! GTK's text-node damage follows font ink extents, which can disagree with
//! hinted glyph coverage. Recording this small, infrequently changed subtree
//! as one Cairo node makes its actual paint clip and old/new damage envelope
//! agree. This does not redraw the window or introduce an animation source.

use gtk::{glib, prelude::*, subclass::prelude::*};
use relm4::gtk;

// BEGIN PURE PAINT BOUNDS
/// Union the allocated card and child snapshot, round outward, and reserve a
/// one-logical-pixel paint guard. Shadows/negative offsets remain included.
/// Cairo clips to this exact envelope; no painted glyph can escape its damage.
fn paint_bounds(width: i32, height: i32, child: [f32; 4]) -> Option<[f32; 4]> {
    let [x, y, w, h] = child;
    if width <= 0 || height <= 0 || w < 0.0 || h < 0.0 {
        return None;
    }
    if !child.iter().all(|value| value.is_finite()) {
        return None;
    }
    let left = x.min(0.0).floor() - 1.0;
    let top = y.min(0.0).floor() - 1.0;
    let right = (x + w).max(width as f32).ceil() + 1.0;
    let bottom = (y + h).max(height as f32).ceil() + 1.0;
    let result = [left, top, right - left, bottom - top];
    if result.iter().all(|value| value.is_finite()) {
        Some(result)
    } else {
        None
    }
}
// END PURE PAINT BOUNDS

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct InlineCard {
        #[cfg(test)]
        pub snapshots: std::cell::Cell<u64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for InlineCard {
        const NAME: &'static str = "AnvilOrganismInlineCard";
        type Type = super::InlineCard;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for InlineCard {}

    impl WidgetImpl for InlineCard {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let children = gtk::Snapshot::new();
            // Record the existing GtkBox children once. Keep their original
            // Pango layout, widget identities, accessibility and tooltip tree.
            self.parent_snapshot(&children);
            let Some(node) = children.to_node() else {
                return;
            };
            let child = node.bounds();
            let Some([x, y, width, height]) = paint_bounds(
                widget.width(),
                widget.height(),
                [child.x(), child.y(), child.width(), child.height()],
            ) else {
                return;
            };
            // append_cairo uses a resolution-independent recording surface,
            // not a fixed 1x bitmap. Its clip also bounds glyph ink overhang.
            // Never append the original node too: that would paint text twice.
            let cr = snapshot.append_cairo(&gtk::graphene::Rect::new(x, y, width, height));
            node.draw(&cr);
            #[cfg(test)]
            self.snapshots.set(self.snapshots.get() + 1);
        }
    }

    impl BoxImpl for InlineCard {}
}

glib::wrapper! {
    pub struct InlineCard(ObjectSubclass<imp::InlineCard>)
        @extends gtk::Widget, gtk::Box,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl InlineCard {
    pub(super) fn new() -> Self {
        glib::Object::builder()
            .property("orientation", gtk::Orientation::Vertical)
            .property("spacing", 0)
            .build()
    }
}

/// Native-only assertions, called by the existing display fixture, never by
/// the ordinary headless unit suite. Fresh surfaces here test complete glyph
/// coverage only; the fixture separately compares incremental X11 pixels.
#[cfg(test)]
pub(super) fn assert_native_paint_contract(card: &gtk::Widget) {
    let card = card
        .downcast_ref::<InlineCard>()
        .expect("owned paint boundary");
    let original = gtk::Snapshot::new();
    card.imp().parent_snapshot(&original);
    let original = original.to_node().expect("mapped native label subtree");
    let painted = gtk::Snapshot::new();
    card.imp().snapshot(&painted);
    let painted = painted.to_node().expect("recorded native label subtree");
    assert_eq!(painted.node_type(), gtk::gsk::RenderNodeType::CairoNode);
    let bounds = painted.bounds();
    assert!(bounds.x() <= 0.0 && bounds.y() <= 0.0);
    assert!(bounds.x() + bounds.width() >= card.width() as f32);
    assert!(bounds.y() + bounds.height() >= card.height() as f32);
    let native = original.bounds();
    assert!(bounds.x() <= native.x() && bounds.y() <= native.y());
    assert!(bounds.x() + bounds.width() >= native.x() + native.width());
    assert!(bounds.y() + bounds.height() >= native.y() + native.height());

    let pixels = |node: &gtk::gsk::RenderNode, scale: f64| {
        // An additional three-pixel surrounding canvas detects clipping by
        // our declared envelope rather than hiding it in the comparison ROI.
        let width = ((f64::from(bounds.width()) + 6.0) * scale).ceil() as i32;
        let height = ((f64::from(bounds.height()) + 6.0) * scale).ceil() as i32;
        assert!(width > 0 && height > 0 && i64::from(width) * i64::from(height) <= 4_000_000);
        let mut surface =
            gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, width, height)
                .expect("bounded reference surface");
        let cr = gtk::cairo::Context::new(&surface).unwrap();
        cr.scale(scale, scale);
        cr.translate(3.0 - f64::from(bounds.x()), 3.0 - f64::from(bounds.y()));
        node.draw(&cr);
        cr.status().expect("native subtree draw");
        drop(cr);
        surface.flush();
        let bytes = surface.data().unwrap().to_vec();
        bytes
    };
    for scale in [1.0, 1.25, 1.5, 2.0] {
        let reference = pixels(&original, scale);
        let recorded = pixels(&painted, scale);
        let changed = reference
            .iter()
            .zip(&recorded)
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(reference.len(), recorded.len());
        assert_eq!(
            changed, 0,
            "recording must preserve complete native ink at scale {scale}"
        );
    }
}

#[cfg(test)]
pub(super) fn snapshot_count(card: &gtk::Widget) -> u64 {
    card.downcast_ref::<InlineCard>()
        .expect("owned paint boundary")
        .imp()
        .snapshots
        .get()
}

// BEGIN PURE TESTS
#[cfg(test)]
mod tests {
    use super::paint_bounds;

    #[test]
    fn paint_envelope_covers_whole_card_not_only_changed_glyphs() {
        let first = paint_bounds(640, 70, [130.0, 28.0, 250.0, 16.0]);
        let next = paint_bounds(640, 70, [130.0, 28.0, 40.0, 16.0]);
        assert_eq!(first, Some([-1.0, -1.0, 642.0, 72.0]));
        assert_eq!(first, next);
        let [x, y, w, h] = first.unwrap();
        // The row just outside reported glyph ink still belongs to damage.
        assert!(x <= 131.0 && y <= 27.0 && x + w >= 381.0 && y + h >= 45.0);
    }

    #[test]
    fn paint_envelope_retains_fractional_overflow_and_negative_offsets() {
        assert_eq!(
            paint_bounds(100, 30, [-2.25, -3.25, 105.0, 40.0]),
            Some([-4.0, -5.0, 108.0, 43.0])
        );
        for fraction in [0.0, 0.125, 0.5, 0.875] {
            let child = [-fraction, -fraction, 101.0, 31.0];
            let [x, y, w, h] = paint_bounds(100, 30, child).unwrap();
            assert!(x <= child[0] && y <= child[1]);
            assert!(x + w >= child[0] + child[2]);
            assert!(y + h >= child[1] + child[3]);
        }
    }

    #[test]
    fn paint_envelope_rejects_unallocated_and_invalid_nodes() {
        for (width, height) in [(0, 30), (100, 0), (-1, 30)] {
            assert_eq!(paint_bounds(width, height, [0.0, 0.0, 100.0, 30.0]), None);
        }
        for child in [
            [f32::NAN, 0.0, 100.0, 30.0],
            [0.0, f32::INFINITY, 100.0, 30.0],
            [0.0, 0.0, -1.0, 30.0],
            [f32::MAX, 0.0, f32::MAX, 30.0],
        ] {
            assert_eq!(paint_bounds(100, 30, child), None);
        }
    }
}
