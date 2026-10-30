use relm4::gtk;
// Exact native allocation and snapshot clip for an admitted Eva rectangle.
use gtk::prelude::*;
use gtk::glib;
mod imp {
    use super::*;
    use gtk::subclass::prelude::*;
    use std::cell::{Cell, OnceCell};
    #[derive(Default)]
    pub struct BodyClip {
        pub child: OnceCell<gtk::Label>,
        pub width: Cell<i32>,
        pub height: Cell<i32>,
    }
    #[glib::object_subclass]
    impl ObjectSubclass for BodyClip {
        const NAME: &'static str = "EvaAnvilClip";
        type Type = super::BodyClip;
        type ParentType = gtk::Widget;
    }
    impl ObjectImpl for BodyClip {
        fn dispose(&self) {
            if let Some(child) = self.child.get() {
                child.unparent();
            }
        }
    }
    impl WidgetImpl for BodyClip {
        fn measure(&self, o: gtk::Orientation, _: i32) -> (i32, i32, i32, i32) {
            let v = if o == gtk::Orientation::Horizontal {
                self.width.get()
            } else {
                self.height.get()
            };
            (v, v, -1, -1)
        }
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            if let Some(child) = self.child.get() {
                child.allocate(width, height, baseline, None);
            }
        }
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            snapshot.push_clip(&gtk::graphene::Rect::new(
                0.,
                0.,
                self.width.get() as f32,
                self.height.get() as f32,
            ));
            if let Some(child) = self.child.get() {
                obj.snapshot_child(child, snapshot);
            }
            snapshot.pop();
        }
    }
}
gtk::glib::wrapper! {pub struct BodyClip(ObjectSubclass<imp::BodyClip>) @extends gtk::Widget,@implements gtk::Accessible,gtk::Buildable,gtk::ConstraintTarget;}
impl BodyClip {
    pub fn new(child: &gtk::Label) -> Self {
        use gtk::subclass::prelude::*;
        let obj: Self = gtk::glib::Object::new();
        obj.imp().child.set(child.clone()).unwrap();
        child.set_parent(&obj);
        obj.set_can_target(false);
        obj.set_focusable(false);
        obj.set_halign(gtk::Align::Start);
        obj.set_valign(gtk::Align::Start);
        obj
    }
    pub fn set_bounds(&self, width: i32, height: i32) {
        use gtk::subclass::prelude::*;
        let imp = self.imp();
        let width_changed = imp.width.replace(width) != width;
        let height_changed = imp.height.replace(height) != height;
        if width_changed || height_changed {
            self.queue_resize();
            self.queue_draw();
        }
    }
}
