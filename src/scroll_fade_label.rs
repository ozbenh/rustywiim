#![allow(deprecated)] // glib clone! old-style @weak syntax

pub mod imp {
    use std::cell::{Cell, RefCell};
    use std::sync::OnceLock;
    use std::time::Duration;

    use glib::{ParamSpec, Value};
    use gtk::glib;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use gtk::{
        Box as GtkBox, EventControllerMotion, Label, Orientation,
        Overlay, PolicyType, ScrolledWindow, Viewport,
    };

    pub struct ScrollFadeLabel {
        // Top-level layout: an Overlay containing the ScrolledWindow + fade layer.
        pub overlay:         Overlay,
        // Gradient overlay drawn on top to fade text at both edges.
        // Toggled visible only when text overflows (hidden when it fits).
        pub fade_layer:      GtkBox,
        pub scrolled_window: ScrolledWindow,
        pub viewport:        Viewport,
        pub container_box:   GtkBox,
        pub label1:          Label,
        pub label2:          Label, // duplicate for seamless infinite loop
        pub timeout_id:      Cell<Option<glib::SourceId>>,
        pub speed:           Cell<f64>,
        pub fade_width:      Cell<i32>,
        pub is_hovered:      Cell<bool>,
        pub text:            RefCell<String>,
        pub is_masked:       Cell<bool>,
    }

    impl Default for ScrollFadeLabel {
        fn default() -> Self {
            Self {
                overlay:         Overlay::new(),
                fade_layer:      GtkBox::new(Orientation::Horizontal, 0),
                scrolled_window: ScrolledWindow::default(),
                viewport:        Viewport::default(),
                // 50 px gap between the two copies so the loop feels continuous
                container_box:   GtkBox::new(Orientation::Horizontal, 50),
                label1:          Label::default(),
                label2:          Label::default(),
                timeout_id:      Cell::new(None),
                speed:           Cell::new(0.33), // ~20 px/s at 60 fps
                fade_width:      Cell::new(10),
                is_hovered:      Cell::new(false),
                text:            RefCell::new(String::new()),
                is_masked:       Cell::new(false),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ScrollFadeLabel {
        const NAME: &'static str = "ScrollFadeLabel";
        type Type = super::ScrollFadeLabel;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for ScrollFadeLabel {
        fn properties() -> &'static [ParamSpec] {
            static PROPS: OnceLock<Vec<ParamSpec>> = OnceLock::new();
            PROPS.get_or_init(|| {
                vec![
                    glib::ParamSpecString::builder("text")
                        .nick("Text")
                        .build(),
                    glib::ParamSpecDouble::builder("speed")
                        .nick("Speed")
                        .minimum(0.1)
                        .maximum(20.0)
                        .default_value(0.33)
                        .build(),
                    glib::ParamSpecInt::builder("fade-width")
                        .nick("Fade Width")
                        .minimum(0)
                        .maximum(45)
                        .default_value(10)
                        .build(),
                ]
            })
        }

        fn set_property(&self, _id: usize, value: &Value, pspec: &ParamSpec) {
            match pspec.name() {
                "text" => {
                    let s = value.get::<String>().unwrap_or_default();
                    self.label1.set_text(&s);
                    self.label2.set_text(&s);
                    self.text.replace(s);
                    self.is_masked.set(false);
                }
                "speed"      => self.speed.set(value.get::<f64>().unwrap_or(0.33)),
                "fade-width" => {
                    self.fade_width.set(value.get::<i32>().unwrap_or(10));
                    self.is_masked.set(false);
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &ParamSpec) -> Value {
            match pspec.name() {
                "text"       => self.text.borrow().to_value(),
                "speed"      => self.speed.get().to_value(),
                "fade-width" => self.fade_width.get().to_value(),
                _ => unimplemented!(),
            }
        }

        fn constructed(&self) {
            let obj = self.obj();

            // Left-align text so the marquee starts cleanly from the left edge.
            self.label1.set_xalign(0.0);
            self.label2.set_xalign(0.0);
            // label2 is hidden at rest; shown only when text overflows.
            self.label2.set_visible(false);

            // Scrolled content: [label1] [50 px gap] [label2]
            self.container_box.append(&self.label1);
            self.container_box.append(&self.label2);

            self.viewport.set_child(Some(&self.container_box));
            self.viewport.set_scroll_to_focus(false);

            self.scrolled_window.set_child(Some(&self.viewport));
            self.scrolled_window.set_hscrollbar_policy(PolicyType::External);
            self.scrolled_window.set_vscrollbar_policy(PolicyType::Never);
            self.scrolled_window.set_has_frame(false);

            // Fade overlay: a transparent box drawn on top of the scroll area.
            // The CSS class carries a linear-gradient background-image that fades
            // the edges to the parent background colour.  Not a pointer target.
            self.fade_layer.add_css_class("marquee-fade");
            self.fade_layer.set_halign(gtk::Align::Fill);
            self.fade_layer.set_valign(gtk::Align::Fill);
            self.fade_layer.set_hexpand(true);
            self.fade_layer.set_vexpand(true);
            self.fade_layer.set_can_target(false);
            self.fade_layer.set_visible(false); // shown only when scrolling

            // Stack them: scrolled window as main child, fade box as overlay.
            self.overlay.set_child(Some(&self.scrolled_window));
            self.overlay.add_overlay(&self.fade_layer);
            self.overlay.set_parent(&*obj);

            // Pause scrolling while the pointer is over this widget.
            let motion = EventControllerMotion::new();
            motion.connect_enter(glib::clone!(@weak obj => move |_, _, _| {
                obj.imp().is_hovered.set(true);
            }));
            motion.connect_leave(glib::clone!(@weak obj => move |_| {
                obj.imp().is_hovered.set(false);
            }));
            obj.add_controller(motion);

            // Drive the marquee at ~60 fps (16 ms tick).
            let hadjustment = self.viewport.hadjustment();
            let id = glib::timeout_add_local(
                Duration::from_millis(16),
                glib::clone!(@weak obj => @default-panic, move || {
                    let Some(ref adj) = hadjustment else {
                        return glib::ControlFlow::Break;
                    };
                    let imp = obj.imp();

                    let visible_width  = adj.page_size();
                    let label_w        = imp.label1.width_request()
                                            .max(imp.label1.allocated_width());
                    let spacing        = imp.container_box.spacing();
                    // Wrap point: after one full label + the separator gap.
                    let loop_threshold = (label_w + spacing) as f64;

                    if (label_w as f64) > visible_width {
                        // Text overflows: enable fade overlay and start scrolling.
                        if !imp.is_masked.get() {
                            imp.fade_layer.set_visible(true);
                            imp.label2.set_visible(true);
                            imp.is_masked.set(true);
                        }
                        if !imp.is_hovered.get() {
                            let mut next = adj.value() + imp.speed.get();
                            // Seamless loop: once past label1, wrap back silently.
                            if next >= loop_threshold { next -= loop_threshold; }
                            adj.set_value(next);
                        }
                    } else {
                        // Text fits: hide fade overlay and reset scroll position.
                        if imp.is_masked.get() {
                            imp.fade_layer.set_visible(false);
                            imp.label2.set_visible(false);
                            imp.is_masked.set(false);
                        }
                        adj.set_value(0.0);
                    }
                    glib::ControlFlow::Continue
                }),
            );
            self.timeout_id.set(Some(id));
        }

        fn dispose(&self) {
            if let Some(id) = self.timeout_id.take() { id.remove(); }
            self.overlay.unparent();
        }
    }

    impl WidgetImpl for ScrollFadeLabel {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            self.overlay.measure(orientation, for_size)
        }
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.overlay.allocate(width, height, baseline, None);
        }
    }
}

// ── Public wrapper ─────────────────────────────────────────────────────────────

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

glib::wrapper! {
    pub struct ScrollFadeLabel(ObjectSubclass<imp::ScrollFadeLabel>)
        @extends gtk::Widget;
}

impl ScrollFadeLabel {
    pub fn new(text: &str) -> Self {
        glib::Object::builder().property("text", text).build()
    }

    pub fn set_text(&self, text: &str) {
        let imp = self.imp();
        imp.label1.set_text(text);
        imp.label2.set_text(text);
        imp.text.replace(text.to_string());
        imp.is_masked.set(false);
    }

    pub fn text(&self) -> glib::GString {
        self.imp().label1.text()
    }

    /// Add a CSS class to both inner labels so font/colour rules reach them.
    pub fn add_label_css_class(&self, class: &str) {
        let imp = self.imp();
        imp.label1.add_css_class(class);
        imp.label2.add_css_class(class);
    }
}

impl Default for ScrollFadeLabel {
    fn default() -> Self { glib::Object::new() }
}
