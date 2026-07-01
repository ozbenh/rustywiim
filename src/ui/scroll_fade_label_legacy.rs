/* LEGACY IMPLEMENTATION — NOT COMPILED
 *
 * This is the original multi-widget ScrollFadeLabel using:
 *   GtkOverlay → GtkScrolledWindow → GtkViewport → GtkBox [Label, Label]
 * with a GtkBox overlay carrying a CSS `background-image: linear-gradient(…)`
 * fade layer.
 *
 * WHY IT WAS REPLACED:
 * The GTK4 NGL (and later Vulkan) renderer composites each layer into its own
 * GPU texture and merges them.  When the ScrolledWindow advances its hadjustment
 * on a timer tick, the renderer only dirtied the scrolled content rectangle;
 * the Overlay's merge pass then re-read a stale strip from the previous frame at
 * the top of the clip region, producing a persistent horizontal line artifact.
 * Adding an opaque `.marquee-bg` on the Overlay reduced the artifact but could
 * not fully eliminate it because the clip-region boundary bookkeeping inside the
 * NGL renderer is fundamentally imprecise for asynchronously-updated child nodes.
 *
 * The new implementation in `scroll_fade_label.rs` overrides `WidgetImpl::snapshot`
 * and emits a single, self-contained GSK render-node tree:
 *   push_clip → push_mask(Alpha) / append_linear_gradient / pop
 *              / save+translate+append_layout×2+restore / pop
 *              → pop
 * Because the entire widget renders atomically in one pass there is no stale-buffer
 * boundary and the layer-merge issue disappears entirely.  It also eliminates the
 * seven-or-so GObject child widgets and hadjustment signal chain per label.
 *
 * This file is intentionally not listed in any `mod` declaration and is kept only
 * as a reference in case the new implementation needs to be debugged against the
 * old behaviour.
 */

//! # ScrollFadeLabel (legacy)
//!
//! A GTK4 widget that displays a single line of text and automatically scrolls
//! it horizontally (marquee style) when the text is too wide to fit, fading
//! both edges into the background colour.  When the text fits, it is displayed
//! statically — centred or left-aligned depending on configuration.
//!
//! ## Scroll behaviour
//!
//! - When `label1` overflows the viewport, `label2` (an identical copy) is
//!   revealed 50 px to its right.  Scrolling advances both until `label1` is
//!   fully off-screen, at which point the position wraps back silently, giving
//!   the illusion of an infinite loop.
//! - Scrolling pauses while the pointer hovers over the widget.
//! - The scroll timer runs **only while text is actually overflowing**; it is
//!   started and stopped by `update_overflow_state()`, which is wired to the
//!   viewport hadjustment `changed` signal.  That signal fires automatically on
//!   both text changes (content reflow) and window resizes (page-size change),
//!   so no polling is needed.
//! - Timer interval is derived from `speed` to fire roughly once per pixel of
//!   movement, reducing CPU wakeups at low speeds.
//!
//! ## CSS nodes
//!
//! | Class           | Widget           | Purpose                                       |
//! |-----------------|------------------|-----------------------------------------------|
//! | `.marquee-bg`   | GtkOverlay       | Opaque background — prevents NGL stale-buffer |
//! |                 |                  | artefacts behind the ScrolledWindow clip.     |
//! | `.marquee-fade` | GtkBox (overlay) | `background-image: linear-gradient(…)` fades  |
//! |                 |                  | the left and right edges into the background. |
//! |                 |                  | Must use same-hue zero-alpha stops, not       |
//! |                 |                  | `transparent` (which is black at α=0).        |
//!
//! ## GLib properties
//!
//! | Property           | Type    | Default | Description                              |
//! |--------------------|---------|---------|------------------------------------------|
//! | `text`             | String  | `""`    | The string to display.                   |
//! | `speed`            | f64     | `0.33`  | Pixels advanced per timer tick (~20 px/s |
//! |                    |         |         | at default interval).                    |
//! | `fade-width`       | i32     | `10`    | Width of each fade zone (percent of      |
//! |                    |         |         | widget width) — set via CSS gradient.    |
//! | `center-when-fits` | bool    | `true`  | Centre text when it fits (main window).  |
//! |                    |         |         | Set to `false` for left-aligned display  |
//! |                    |         |         | (mini window).                           |
//!
//! ## Usage
//!
//! ```rust,ignore
//! let label = ScrollFadeLabel::new("My track title");
//! label.add_label_css_class("track-title");  // font/colour via CSS
//! label.set_hexpand(true);
//! label.set_center_when_fits(false);         // left-align in mini window
//! // later:
//! label.set_text("New title");
//! ```

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

    // Property defaults — single source of truth used by the ParamSpec
    // declarations, the struct Default impl, and the set_property fallbacks.
    const SPEED_DEFAULT:            f64  = 0.33; // px/tick → ~20 px/s at default interval
    const FADE_WIDTH_DEFAULT:       i32  = 10;   // percent of widget width per fade zone
    const CENTER_WHEN_FITS_DEFAULT: bool = true;

    pub struct ScrollFadeLabel {
        // Top-level layout: an Overlay containing the ScrolledWindow + fade layer.
        pub overlay:         Overlay,
        // Gradient overlay drawn on top to fade text at both edges.
        // Shown only when text overflows.
        pub fade_layer:      GtkBox,
        pub scrolled_window: ScrolledWindow,
        pub viewport:        Viewport,
        pub container_box:   GtkBox,
        pub label1:          Label,
        pub label2:          Label,   // duplicate for seamless infinite loop
        pub scroll_timer_id: RefCell<Option<glib::SourceId>>, // None when idle
        pub speed:           Cell<f64>,
        pub fade_width:      Cell<i32>,
        pub is_hovered:      Cell<bool>,
        pub text:            RefCell<String>,
        pub is_scrolling:    Cell<bool>, // true while scroll timer is running
        // When true, centre the text when it fits (main window).
        // When false, left-align it (mini window).
        pub center_when_fits: Cell<bool>,
    }

    impl Default for ScrollFadeLabel {
        fn default() -> Self {
            Self {
                overlay:          Overlay::new(),
                fade_layer:       GtkBox::new(Orientation::Horizontal, 0),
                scrolled_window:  ScrolledWindow::default(),
                viewport:         Viewport::default(),
                // 50 px gap between the two copies so the loop feels continuous
                container_box:    GtkBox::new(Orientation::Horizontal, 50),
                label1:           Label::default(),
                label2:           Label::default(),
                scroll_timer_id:  RefCell::new(None),
                speed:            Cell::new(SPEED_DEFAULT),
                fade_width:       Cell::new(FADE_WIDTH_DEFAULT),
                is_hovered:       Cell::new(false),
                text:             RefCell::new(String::new()),
                is_scrolling:     Cell::new(false),
                center_when_fits: Cell::new(CENTER_WHEN_FITS_DEFAULT),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ScrollFadeLabel {
        const NAME: &'static str = "ScrollFadeLabelLegacy";
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
                    // adj "changed" fires automatically after the layout pass and
                    // calls update_overflow_state() — no manual reset needed here.
                }
                "speed"      => self.speed.set(value.get::<f64>().unwrap_or(SPEED_DEFAULT)),
                "fade-width" => self.fade_width.set(value.get::<i32>().unwrap_or(FADE_WIDTH_DEFAULT)),
                "center-when-fits" => {
                    let center = value.get::<bool>().unwrap_or(CENTER_WHEN_FITS_DEFAULT);
                    self.center_when_fits.set(center);
                    // Apply immediately if not scrolling (no adj signal needed).
                    if !self.is_scrolling.get() {
                        self.label1.set_xalign(if center { 0.5 } else { 0.0 });
                    }
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &ParamSpec) -> Value {
            match pspec.name() {
                "text"             => self.text.borrow().to_value(),
                "speed"            => self.speed.get().to_value(),
                "fade-width"       => self.fade_width.get().to_value(),
                "center-when-fits" => self.center_when_fits.get().to_value(),
                _ => unimplemented!(),
            }
        }

        fn constructed(&self) {
            let obj = self.obj();

            // Initial idle state: label1 fills the container; text is centred
            // (or left-aligned once center_when_fits is set to false externally).
            self.label1.set_hexpand(true);
            self.label1.set_xalign(0.5);
            // label2 is a scrolling duplicate; always left-aligned and hidden at rest.
            self.label2.set_xalign(0.0);
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
            self.fade_layer.set_visible(false);

            // Stack them: scrolled window as main child, fade box as overlay.
            // marquee-bg sets an opaque background matching the window colour so
            // the NGL renderer never exposes stale GPU buffer content behind
            // the ScrolledWindow's clipping region.
            self.overlay.add_css_class("marquee-bg");
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

            // React to content or viewport size changes.  This signal fires when
            // the label is reflowed after a set_text() call (adj.upper changes) or
            // when the window is resized (adj.page_size changes).  The 60 fps timer
            // runs only while actually scrolling; this signal starts and stops it.
            if let Some(adj) = self.viewport.hadjustment() {
                adj.connect_changed(glib::clone!(@weak obj => move |a| {
                    obj.imp().update_overflow_state(a);
                }));
            }
        }

        fn dispose(&self) {
            self.stop_scroll_timer();
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

    impl ScrollFadeLabel {
        /// Re-evaluate whether text overflows based on current sizes.
        /// Called from the hadjustment "changed" signal, which fires whenever
        /// content is reflowed (text change) or the viewport is resized.
        /// Starts or stops the scroll timer as needed.
        pub fn update_overflow_state(&self, adj: &gtk::Adjustment) {
            let visible_width = adj.page_size();
            let label_w       = self.label1.allocated_width();

            // Skip until the widget has been laid out at least once.
            if visible_width == 0.0 { return; }

            if (label_w as f64) > visible_width {
                // Text overflows — enter scroll mode if not already in it.
                if !self.is_scrolling.get() {
                    self.label1.set_hexpand(false);
                    self.label1.set_xalign(0.0);
                    self.fade_layer.set_visible(true);
                    self.label2.set_visible(true);
                    self.is_scrolling.set(true);
                }
                self.start_scroll_timer();
            } else {
                // Text fits — enforce idle state and stop the timer.
                let xalign = if self.center_when_fits.get() { 0.5 } else { 0.0 };
                self.label1.set_hexpand(true);
                self.label1.set_xalign(xalign);
                self.fade_layer.set_visible(false);
                self.label2.set_visible(false);
                self.is_scrolling.set(false);
                adj.set_value(0.0);
                self.stop_scroll_timer();
            }
        }

        /// Advance the scroll position by one timer tick.  Called from the scroll
        /// timer.  Only handles position arithmetic — all state management is in
        /// update_overflow_state().
        pub fn scroll_tick(&self, adj: &gtk::Adjustment) {
            if self.is_hovered.get() { return; }
            let label_w        = self.label1.allocated_width();
            let loop_threshold = (label_w + self.container_box.spacing()) as f64;
            // Step size equals speed so the visual velocity is always speed px/tick
            // regardless of the timer interval chosen in start_scroll_timer().
            let mut next = adj.value() + self.speed.get();
            if next >= loop_threshold { next -= loop_threshold; }
            adj.set_value(next);
        }

        fn start_scroll_timer(&self) {
            if self.scroll_timer_id.borrow().is_some() { return; } // already running
            let Some(adj) = self.viewport.hadjustment() else { return; };

            // Fire at a rate that moves the text by ~1 px per tick.  This keeps
            // motion smooth while reducing wakeups at low speeds.
            // At speed=0.33 px/tick → ~33 ms interval (~30 fps).
            // At speed=1.0 px/tick → ~16 ms interval (~60 fps).
            // Minimum interval 16 ms (capped at 60 fps regardless of speed).
            let interval_ms = (1000.0 / (self.speed.get() * 60.0)).round().max(16.0) as u64;

            let obj = self.obj();
            let id = glib::timeout_add_local(
                Duration::from_millis(interval_ms),
                glib::clone!(@weak obj => @default-return glib::ControlFlow::Break, move || {
                    obj.imp().scroll_tick(&adj);
                    glib::ControlFlow::Continue
                }),
            );
            *self.scroll_timer_id.borrow_mut() = Some(id);
        }

        fn stop_scroll_timer(&self) {
            if let Some(id) = self.scroll_timer_id.borrow_mut().take() { id.remove(); }
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
        // The adj "changed" signal fires after the layout pass and calls
        // update_overflow_state() — no manual state reset needed here.
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

    /// Control whether text is centred (true, default) or left-aligned (false)
    /// when it fits within the available width without scrolling.
    pub fn set_center_when_fits(&self, center: bool) {
        let imp = self.imp();
        imp.center_when_fits.set(center);
        if !imp.is_scrolling.get() {
            imp.label1.set_xalign(if center { 0.5 } else { 0.0 });
        }
    }
}

impl Default for ScrollFadeLabel {
    fn default() -> Self { glib::Object::new() }
}
