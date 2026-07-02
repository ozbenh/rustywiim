//! # ScrollFadeLabel
//!
//! A GTK4 widget that displays a single line of text and automatically scrolls
//! it horizontally (marquee style) when the text is too wide to fit, fading
//! both edges using a GSK alpha-mask gradient.  When the text fits, it is
//! displayed statically — centred or left-aligned depending on configuration.
//!
//! ## Rendering approach
//!
//! The widget overrides `WidgetImpl::snapshot()` and emits a single, atomic GSK
//! render-node tree:
//!
//! ```text
//! push_clip(widget_bounds)
//!   push_mask(Alpha)          ← mask layer:
//!     append_linear_gradient  ←   alpha=0 at edges, alpha=1 in centre
//!   pop()                     ← end mask; begin content layer:
//!     save / translate / append_layout / restore   ← first text copy
//!     save / translate / append_layout / restore   ← second text copy (loop)
//!   pop()                     ← apply mask to content
//! pop()                       ← end clip
//! ```
//!
//! There are no child widgets, no hadjustment signal chain, and no multi-layer
//! compositing.  The entire frame is rendered in one pass, eliminating the
//! stale-strip artifacts produced by the NGL renderer when asynchronously
//! scrolling a child ScrolledWindow.
//!
//! ## GLib properties
//!
//! | Property           | Type    | Default | Description                              |
//! |--------------------|---------|---------|------------------------------------------|
//! | `text`             | String  | `""`    | The string to display.                   |
//! | `speed`            | f64     | `0.33`  | Pixels advanced per timer tick.          |
//! | `fade-width`       | i32     | `15`    | Width of each fade zone (percent of      |
//! |                    |         |         | widget width).                           |
//! | `center-when-fits` | bool    | `true`  | Centre text when it fits (main window).  |
//! |                    |         |         | Set to `false` for left-aligned display  |
//! |                    |         |         | (mini window).                           |
//!
//! ## Usage
//!
//! ```rust,ignore
//! let label = ScrollFadeLabel::new("My track title");
//! label.add_label_css_class("track-title");  // font/colour resolved from CSS
//! label.set_hexpand(true);
//! label.set_center_when_fits(false);         // left-align in mini window
//! // later:
//! label.set_text("New title");
//! ```

#![allow(deprecated)] // glib::clone! old-style @weak syntax

pub mod imp {
    use std::cell::{Cell, RefCell};
    use std::sync::OnceLock;
    use std::time::Duration;

    use glib::{ParamSpec, Value};
    use gtk::glib;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;

    const SPEED_DEFAULT:            f64  = 0.33;
    const FADE_WIDTH_DEFAULT:       i32  = 15;   // percent of widget width per fade zone
    const CENTER_WHEN_FITS_DEFAULT: bool = true;
    const GAP: f32 = 50.0; // px gap between the two text copies in loop mode

    pub struct ScrollFadeLabel {
        pub scroll_timer_id:  RefCell<Option<glib::SourceId>>,
        pub speed:            Cell<f64>,
        pub fade_pct:         Cell<i32>,
        pub is_hovered:       Cell<bool>,
        pub is_scrolling:     Cell<bool>,
        pub text:             RefCell<String>,
        pub center_when_fits: Cell<bool>,
        // Optional drop shadow behind the text — off by default
        pub drop_shadow:      Cell<bool>,
        // Set in size_allocate: width of one text copy + GAP gap.
        pub loop_width:       Cell<f32>,
        // Current horizontal scroll position in pixels.
        pub scroll_offset:    Cell<f32>,
        // Cached pango layout — rebuilt in size_allocate, reused in snapshot.
        // Cleared on text change and system font/DPI change.
        pub layout_cache:     RefCell<Option<gtk::pango::Layout>>,
    }

    impl Default for ScrollFadeLabel {
        fn default() -> Self {
            Self {
                scroll_timer_id:  RefCell::new(None),
                speed:            Cell::new(SPEED_DEFAULT),
                fade_pct:         Cell::new(FADE_WIDTH_DEFAULT),
                is_hovered:       Cell::new(false),
                is_scrolling:     Cell::new(false),
                text:             RefCell::new(String::new()),
                center_when_fits: Cell::new(CENTER_WHEN_FITS_DEFAULT),
                drop_shadow:      Cell::new(false),
                loop_width:       Cell::new(0.0),
                scroll_offset:    Cell::new(0.0),
                layout_cache:     RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ScrollFadeLabel {
        const NAME: &'static str = "ScrollFadeLabel";
        type Type = super::ScrollFadeLabel;
        type ParentType = gtk::Widget;
        // No layout manager — no child widgets.
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
                        .default_value(SPEED_DEFAULT)
                        .build(),
                    glib::ParamSpecInt::builder("fade-width")
                        .nick("Fade Width")
                        .minimum(0)
                        .maximum(45)
                        .default_value(FADE_WIDTH_DEFAULT)
                        .build(),
                    glib::ParamSpecBoolean::builder("center-when-fits")
                        .nick("Center When Fits")
                        .default_value(CENTER_WHEN_FITS_DEFAULT)
                        .build(),
                    glib::ParamSpecBoolean::builder("drop-shadow")
                        .nick("Drop Shadow")
                        .default_value(false)
                        .build(),
                ]
            })
        }

        fn set_property(&self, _id: usize, value: &Value, pspec: &ParamSpec) {
            match pspec.name() {
                "text" => {
                    let s = value.get::<String>().unwrap_or_default();
                    self.text.replace(s);
                    self.scroll_offset.set(0.0);
                    *self.layout_cache.borrow_mut() = None;
                    self.obj().queue_resize();
                }
                "speed"      => self.speed.set(value.get::<f64>().unwrap_or(SPEED_DEFAULT)),
                "fade-width" => self.fade_pct.set(value.get::<i32>().unwrap_or(FADE_WIDTH_DEFAULT)),
                "center-when-fits" => {
                    self.center_when_fits.set(value.get::<bool>().unwrap_or(CENTER_WHEN_FITS_DEFAULT));
                    self.obj().queue_draw();
                }
                "drop-shadow" => {
                    self.drop_shadow.set(value.get::<bool>().unwrap_or(false));
                    self.obj().queue_draw();
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &ParamSpec) -> Value {
            match pspec.name() {
                "text"             => self.text.borrow().to_value(),
                "speed"            => self.speed.get().to_value(),
                "fade-width"       => self.fade_pct.get().to_value(),
                "center-when-fits" => self.center_when_fits.get().to_value(),
                "drop-shadow"      => self.drop_shadow.get().to_value(),
                _ => unimplemented!(),
            }
        }

        fn constructed(&self) {
            let obj = self.obj();
            let motion = gtk::EventControllerMotion::new();
            motion.connect_enter(glib::clone!(@weak obj => move |_, _, _| {
                obj.imp().is_hovered.set(true);
            }));
            motion.connect_leave(glib::clone!(@weak obj => move |_| {
                obj.imp().is_hovered.set(false);
            }));
            obj.add_controller(motion);
        }

        fn dispose(&self) {
            self.stop_scroll_timer();
        }
    }

    impl WidgetImpl for ScrollFadeLabel {
        fn map(&self) {
            self.parent_map();
            if self.is_scrolling.get() {
                self.start_scroll_timer();
            }
        }

        fn unmap(&self) {
            self.stop_scroll_timer();
            self.parent_unmap();
        }

        fn system_setting_changed(&self, settings: &gtk::SystemSetting) {
            self.parent_system_setting_changed(settings);
            *self.layout_cache.borrow_mut() = None;
            self.obj().queue_resize();
        }

        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let layout = self.create_layout();
            let (tw, th) = layout.pixel_size();
            if orientation == gtk::Orientation::Vertical {
                (th, th, -1, -1)
            } else {
                (0, tw, -1, -1) // minimum=0 (clips), natural=full text width
            }
        }

        fn size_allocate(&self, width: i32, _height: i32, _baseline: i32) {
            // Always rebuild on allocation so CSS font changes (via queue_resize) take effect.
            *self.layout_cache.borrow_mut() = None;
            let layout = self.create_layout();
            let (text_w, _) = layout.pixel_size();
            if (text_w as f32) > (width as f32) {
                self.loop_width.set(text_w as f32 + GAP);
                if !self.is_scrolling.get() {
                    self.is_scrolling.set(true);
                    self.scroll_offset.set(0.0);
                    self.start_scroll_timer();
                }
            } else {
                let was_scrolling = self.is_scrolling.get();
                self.is_scrolling.set(false);
                if was_scrolling {
                    self.scroll_offset.set(0.0);
                    self.stop_scroll_timer();
                }
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let width  = widget.width()  as f32;
            let height = widget.height() as f32;
            if width <= 0.0 || height <= 0.0 { return; }

            let layout = self.create_layout();
            let (text_pw, text_ph) = layout.pixel_size();
            let text_h = text_ph as f32;
            let text_w = text_pw as f32;
            let color  = widget.color();
            let y = ((height - text_h) / 2.0).floor();

            let bounds = gtk::graphene::Rect::new(0.0, 0.0, width, height);
            snapshot.push_clip(&bounds);

            if self.is_scrolling.get() {
                let fade_f = (self.fade_pct.get() as f32 / 100.0).clamp(0.0, 0.45);
                let clear  = gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0);
                let opaque = gtk::gdk::RGBA::new(1.0, 1.0, 1.0, 1.0);

                // Mask layer: alpha gradient, transparent at edges, opaque in centre.
                snapshot.push_mask(gtk::gsk::MaskMode::Alpha);
                snapshot.append_linear_gradient(
                    &bounds,
                    &gtk::graphene::Point::new(0.0, 0.0),
                    &gtk::graphene::Point::new(width, 0.0),
                    &[
                        gtk::gsk::ColorStop::new(0.0,          clear),
                        gtk::gsk::ColorStop::new(fade_f,       opaque),
                        gtk::gsk::ColorStop::new(1.0 - fade_f, opaque),
                        gtk::gsk::ColorStop::new(1.0,          clear),
                    ],
                );
                snapshot.pop(); // end mask, begin content

                let offset  = self.scroll_offset.get();
                let loop_w  = self.loop_width.get();

                snapshot.save();
                snapshot.translate(&gtk::graphene::Point::new(-offset, y));
                self.append_layout_shadowed(snapshot, &layout, &color);
                snapshot.restore();

                snapshot.save();
                snapshot.translate(&gtk::graphene::Point::new(loop_w - offset, y));
                self.append_layout_shadowed(snapshot, &layout, &color);
                snapshot.restore();

                snapshot.pop(); // apply mask to content
            } else {
                let x = if self.center_when_fits.get() {
                    ((width - text_w) / 2.0).max(0.0).floor()
                } else {
                    0.0
                };
                snapshot.save();
                snapshot.translate(&gtk::graphene::Point::new(x, y));
                self.append_layout_shadowed(snapshot, &layout, &color);
                snapshot.restore();
            }

            snapshot.pop(); // end clip
        }
    }

    impl ScrollFadeLabel {
        /// Draw `layout` at the snapshot's current (already-translated)
        /// origin, with an optional soft drop shadow behind it when
        /// `drop_shadow` is set. A plain gtk::Label gets CSS text-shadow for
        /// free; this custom-rendered widget doesn't, so the shadow is drawn
        /// manually as a blurred, offset dark copy underneath the real text.
        fn append_layout_shadowed(
            &self,
            snapshot: &gtk::Snapshot,
            layout: &gtk::pango::Layout,
            color: &gtk::gdk::RGBA,
        ) {
            if self.drop_shadow.get() {
                let shadow = gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.75);
                snapshot.push_blur(2.0);
                snapshot.save();
                snapshot.translate(&gtk::graphene::Point::new(0.0, 1.0));
                snapshot.append_layout(layout, &shadow);
                snapshot.restore();
                snapshot.pop();
            }
            snapshot.append_layout(layout, color);
        }

        fn create_layout(&self) -> gtk::pango::Layout {
            let mut cache = self.layout_cache.borrow_mut();
            if let Some(ref layout) = *cache {
                return layout.clone(); // GObject clone — cheap refcount bump
            }
            let layout = gtk::pango::Layout::new(&self.obj().pango_context());
            layout.set_text(&self.text.borrow());
            layout.set_single_paragraph_mode(true);
            layout.set_width(-1);
            *cache = Some(layout.clone());
            layout
        }

        fn scroll_tick(&self) {
            if self.is_hovered.get() { return; }
            let loop_w = self.loop_width.get();
            let next = (self.scroll_offset.get() + self.speed.get() as f32) % loop_w;
            self.scroll_offset.set(next);
            self.obj().queue_draw();
        }

        fn start_scroll_timer(&self) {
            if self.scroll_timer_id.borrow().is_some() { return; }
            let interval_ms =
                (1000.0 / (self.speed.get() * 60.0)).round().max(16.0) as u64;
            let obj = self.obj();
            let id = glib::timeout_add_local(
                Duration::from_millis(interval_ms),
                glib::clone!(@weak obj => @default-return glib::ControlFlow::Break, move || {
                    obj.imp().scroll_tick();
                    glib::ControlFlow::Continue
                }),
            );
            *self.scroll_timer_id.borrow_mut() = Some(id);
        }

        fn stop_scroll_timer(&self) {
            if let Some(id) = self.scroll_timer_id.borrow_mut().take() {
                id.remove();
            }
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
        imp.text.replace(text.to_string());
        imp.scroll_offset.set(0.0);
        *imp.layout_cache.borrow_mut() = None;
        self.queue_resize();
    }

    pub fn text(&self) -> glib::GString {
        glib::GString::from(self.imp().text.borrow().as_str())
    }

    /// Apply a CSS class directly to this widget so font/colour rules reach it.
    pub fn add_label_css_class(&self, class: &str) {
        self.add_css_class(class);
    }

    pub fn set_center_when_fits(&self, center: bool) {
        let imp = self.imp();
        imp.center_when_fits.set(center);
        self.queue_draw();
    }

    /// Toggle the manual drop shadow — off by default. Experimental knob for
    /// legibility on busy/blurred backgrounds; flip freely to compare.
    pub fn set_drop_shadow(&self, enabled: bool) {
        let imp = self.imp();
        imp.drop_shadow.set(enabled);
        self.queue_draw();
    }
}

impl Default for ScrollFadeLabel {
    fn default() -> Self { glib::Object::new() }
}
