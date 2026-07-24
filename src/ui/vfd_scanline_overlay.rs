//! `VfdScanlineOverlay` — a decorative, click-through widget that paints
//! RustyWiiM Wood's "every other line" VFD scanline pattern as a semi-
//! transparent dimming layer *on top* of whatever it's overlaid onto
//! (`gtk::Overlay::add_overlay()`), sized to match automatically since
//! that's how a `gtk::Overlay`'s own overlay children work.
//!
//! Exists because gtk4-rs doesn't support subclassing `GtkLabel`/`GtkImage`
//! at all (see `BrandIcon`'s own doc comment) — those widgets can't get a
//! real GSK alpha mask applied around their own rendered content the way
//! `ScrollFadeLabel` can for its glow text. Painting dimming stripes over
//! the *result* instead reads the same visually, without needing access to
//! the wrapped widget's own render tree — `wrap()` below is the one place
//! that actually builds the `gtk::Overlay` pairing.

pub mod imp {
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;

    #[derive(Default)]
    pub struct VfdScanlineOverlay;

    #[glib::object_subclass]
    impl ObjectSubclass for VfdScanlineOverlay {
        const NAME: &'static str = "VfdScanlineOverlay";
        type Type = super::VfdScanlineOverlay;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for VfdScanlineOverlay {
        fn constructed(&self) {
            self.parent_constructed();
            // Purely decorative — never steals input meant for whatever
            // this is overlaid on top of.
            self.obj().set_can_target(false);
        }
    }

    impl WidgetImpl for VfdScanlineOverlay {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let w = widget.width() as f32;
            let h = widget.height() as f32;
            if w <= 0.0 || h <= 0.0 { return; }
            crate::ui::vfd_scanline::paint_scanline_dim(snapshot, w, h);
        }
    }
}

use gtk::glib;
use gtk::prelude::*;

glib::wrapper! {
    pub struct VfdScanlineOverlay(ObjectSubclass<imp::VfdScanlineOverlay>)
        @extends gtk::Widget;
}

impl VfdScanlineOverlay {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// Wraps `child` in a `gtk::Overlay` with this widget laid on top —
    /// the one place this pairing is actually built, so every call site
    /// (pos/status/dur's `time_row`, `service_group`) shares the exact
    /// same construction rather than each repeating the two-line dance.
    /// Returns the `Overlay` itself, usable anywhere `child` would have
    /// been (it's a real `gtk::Widget`).
    ///
    /// The returned `Overlay` itself is `can_target(false)` — confirmed
    /// live this is needed, not just on the decorative overlay child: a
    /// plain `gtk::Overlay` defaults to `halign`/`valign` `Fill`, so once
    /// this wrapper becomes an overlay child of some *outer* `Overlay`
    /// (e.g. `service_group` here sitting inside `controls_overlay_boxed`
    /// alongside the transport/volume/EQ buttons), it gets allocated that
    /// outer overlay's *entire* area, not just `child`'s own natural-size
    /// corner the way `child` alone would have. Sitting on top in z-order
    /// with its default `can_target(true)`, it silently ate clicks meant
    /// for those buttons everywhere else in that shared space, even though
    /// nothing wrapped here (labels/icons) is itself interactive. Safe to
    /// disable targeting on the whole wrapper for exactly that reason —
    /// every current caller only ever wraps non-interactive content.
    pub fn wrap(child: &impl IsA<gtk::Widget>) -> gtk::Overlay {
        let overlay = gtk::Overlay::new();
        overlay.set_can_target(false);
        overlay.set_child(Some(child));
        overlay.add_overlay(&Self::new());
        overlay
    }
}

impl Default for VfdScanlineOverlay {
    fn default() -> Self { Self::new() }
}
