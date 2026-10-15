//! A bounded paint surface for the inline companion's native label subtree.
//!
//! GTK's text-node damage follows font ink extents, which can disagree with
//! hinted glyph coverage. Recording this small, infrequently changed subtree
//! as one Cairo node makes its actual paint clip and old/new damage envelope
//! agree. This does not redraw the window or introduce an animation source.

use gtk::{glib, prelude::*, subclass::prelude::*};
use relm4::gtk;

// BEGIN PURE PAINT BOUNDS
/// Union the real local CSS border box and child ink, then round outward.
/// The guard covers the current inset outline; arbitrary outer CSS shadows
/// are not claimed to be part of compute_bounds or this paint contract.
fn paint_bounds(card: [f32; 4], child: [f32; 4]) -> Option<[f32; 4]> {
    if card[2] <= 0.0 || card[3] <= 0.0 || child[2] < 0.0 || child[3] < 0.0 {
        return None;
    }
    if !card
        .iter()
        .chain(child.iter())
        .all(|value| value.is_finite())
    {
        return None;
    }
    let card_right = card[0] + card[2];
    let card_bottom = card[1] + card[3];
    let child_right = child[0] + child[2];
    let child_bottom = child[1] + child[3];
    if ![card_right, card_bottom, child_right, child_bottom]
        .iter()
        .all(|value| value.is_finite())
    {
        return None;
    }
    let left = card[0].min(child[0]).floor() - 1.0;
    let top = card[1].min(child[1]).floor() - 1.0;
    let right = card_right.max(child_right).ceil() + 1.0;
    let bottom = card_bottom.max(child_bottom).ceil() + 1.0;
    let result = [left, top, right - left, bottom - top];
    result
        .iter()
        .all(|value| value.is_finite())
        .then_some(result)
}
// END PURE PAINT BOUNDS

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct InlineCard {
        #[cfg(test)]
        pub snapshots: std::cell::Cell<u64>,
        #[cfg(test)]
        pub last_envelope: std::cell::Cell<Option<[f32; 4]>>,
        #[cfg(test)]
        pub last_children: std::cell::Cell<Option<[f32; 4]>>,
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
            if widget.width() <= 0 || widget.height() <= 0 {
                return;
            }
            // width/height are content-box dimensions. Native bounds include
            // asymmetric CSS border/padding and can begin at negative offsets.
            let Some(card) = widget.compute_bounds(&*widget) else {
                return;
            };
            let Some([x, y, width, height]) = paint_bounds(
                [card.x(), card.y(), card.width(), card.height()],
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
            {
                self.snapshots.set(self.snapshots.get() + 1);
                self.last_envelope.set(Some([x, y, width, height]));
                self.last_children
                    .set(Some([child.x(), child.y(), child.width(), child.height()]));
            }
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
    let border = card.compute_bounds(card).expect("mapped local border box");
    assert!(bounds.x() <= border.x() && bounds.y() <= border.y());
    assert!(bounds.x() + bounds.width() >= border.x() + border.width());
    assert!(bounds.y() + bounds.height() >= border.y() + border.height());
    let native = original.bounds();
    assert!(bounds.x() <= native.x() && bounds.y() <= native.y());
    assert!(bounds.x() + bounds.width() >= native.x() + native.width());
    assert!(bounds.y() + bounds.height() >= native.y() + native.height());

    // A direct draw to an image is not a complete-ink oracle: GSK's
    // per-node image clipping can discard hinted glyph overhang. Record the
    // native subtree without extents first, independently of our bounded
    // production node, then compare both recordings at each device scale.
    let reference = gtk::cairo::RecordingSurface::create(gtk::cairo::Content::ColorAlpha, None)
        .expect("unbounded native ink reference");
    let reference_cr = gtk::cairo::Context::new(&reference).unwrap();
    original.draw(&reference_cr);
    reference_cr.status().expect("native reference recording");
    drop(reference_cr);

    let pixels = |paint: &dyn Fn(&gtk::cairo::Context), scale: f64| {
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
        paint(&cr);
        cr.status().expect("native subtree draw");
        drop(cr);
        surface.flush();
        let bytes = surface.data().unwrap().to_vec();
        bytes
    };
    for scale in [1.0, 1.25, 1.5, 2.0] {
        let native = pixels(
            &|cr| {
                cr.set_source_surface(&reference, 0.0, 0.0).unwrap();
                cr.paint().unwrap();
            },
            scale,
        );
        let recorded = pixels(&|cr| painted.draw(cr), scale);
        let changed = native.iter().zip(&recorded).filter(|(a, b)| a != b).count();
        assert_eq!(native.len(), recorded.len());
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

/// Observes the last real snapshot without recording or invalidating a node.
#[cfg(test)]
pub(super) fn recorded_geometry(card: &gtk::Widget) -> (Option<[f32; 4]>, Option<[f32; 4]>) {
    let imp = card
        .downcast_ref::<InlineCard>()
        .expect("owned paint boundary")
        .imp();
    (imp.last_envelope.get(), imp.last_children.get())
}

// BEGIN PURE TESTS
#[cfg(test)]
mod tests {
    use super::paint_bounds;

    #[test]
    fn paint_envelope_covers_whole_card_not_only_changed_glyphs() {
        let first = paint_bounds([0.0, 0.0, 640.0, 70.0], [130.0, 28.0, 250.0, 16.0]);
        let next = paint_bounds([0.0, 0.0, 640.0, 70.0], [130.0, 28.0, 40.0, 16.0]);
        assert_eq!(first, Some([-1.0, -1.0, 642.0, 72.0]));
        assert_eq!(first, next);
        let [x, y, w, h] = first.unwrap();
        // The row just outside reported glyph ink still belongs to damage.
        assert!(x <= 131.0 && y <= 27.0 && x + w >= 381.0 && y + h >= 45.0);
    }

    #[test]
    fn paint_envelope_retains_fractional_overflow_and_negative_offsets() {
        assert_eq!(
            paint_bounds([0.0, 0.0, 100.0, 30.0], [-2.25, -3.25, 105.0, 40.0]),
            Some([-4.0, -5.0, 108.0, 43.0])
        );
        for fraction in [0.0, 0.125, 0.5, 0.875] {
            let child = [-fraction, -fraction, 101.0, 31.0];
            let [x, y, w, h] = paint_bounds([0.0, 0.0, 100.0, 30.0], child).unwrap();
            assert!(x <= child[0] && y <= child[1]);
            assert!(x + w >= child[0] + child[2]);
            assert!(y + h >= child[1] + child[3]);
        }
    }

    #[test]
    fn paint_envelope_covers_asymmetric_native_border_box() {
        let border = [-3.0, -1.0, 104.0, 32.0];
        let child = [12.0, 8.0, 50.0, 14.0];
        assert_eq!(paint_bounds(border, child), Some([-4.0, -2.0, 106.0, 34.0]));
        // A content-only envelope began at -1 and missed this left stripe.
        let [x, y, w, h] = paint_bounds(border, child).unwrap();
        assert!(x < border[0] && y < border[1]);
        assert!(x + w > border[0] + border[2]);
        assert!(y + h > border[1] + border[3]);
        for invalid in [
            [f32::NAN, 0.0, 104.0, 32.0],
            [f32::MAX, 0.0, f32::MAX, 32.0],
            [0.0, 0.0, -1.0, 32.0],
        ] {
            assert_eq!(paint_bounds(invalid, child), None);
        }
    }

    #[test]
    fn paint_envelope_rejects_unallocated_and_invalid_nodes() {
        for (width, height) in [(0, 30), (100, 0), (-1, 30)] {
            assert_eq!(
                paint_bounds(
                    [0.0, 0.0, width as f32, height as f32],
                    [0.0, 0.0, 100.0, 30.0]
                ),
                None
            );
        }
        for child in [
            [f32::NAN, 0.0, 100.0, 30.0],
            [0.0, f32::INFINITY, 100.0, 30.0],
            [0.0, 0.0, -1.0, 30.0],
            [f32::MAX, 0.0, f32::MAX, 30.0],
        ] {
            assert_eq!(paint_bounds([0.0, 0.0, 100.0, 30.0], child), None);
        }
    }
}
