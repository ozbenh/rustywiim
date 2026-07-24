//! Shared "every other line" scanline effect for RustyWiiM Wood's Kiosk VFD
//! look. Two forms:
//!
//! - `push_scanline_mask()` — a true GSK alpha mask, for a widget that
//!   already renders its own content by hand (`ScrollFadeLabel`'s glow
//!   text): wrap the content in `push_mask`/this/pop, and the mask
//!   multiplies directly into its alpha.
//! - `paint_scanline_dim()` — semi-transparent dark stripes painted as
//!   ordinary content, for `VfdScanlineOverlay` (`vfd_scanline_overlay.rs`)
//!   to lay *on top of* an arbitrary existing widget (`gtk::Label`,
//!   `gtk::Image`, `BrandIcon`, ...) via `gtk::Overlay` — gtk4-rs doesn't
//!   support subclassing `GtkLabel`/`GtkImage` at all (see `BrandIcon`'s
//!   own doc comment), so those can't get a real alpha mask over their own
//!   rendering the way `ScrollFadeLabel` can; dimming stripes composited
//!  *over* the result read the same visually without needing access to
//!   the wrapped widget's own render tree.
//!
//! Both read as the same interlaced pattern since they share the same
//! pitch/dim-alpha constants. Pure GSK (`append_repeating_linear_gradient`),
//! not CSS: GTK's CSS engine has no `mask-image` property to reach for here
//! (see `.vfd-panel`'s own comment in `wood.css` for the CSS-side scanline
//! technique a flat background rectangle *can* use instead — a plain
//! `repeating-linear-gradient()`).

use gtk::prelude::*;

/// Vertical scanline pitch (px) the mask alternates at — matches
/// `wood.css`'s own `.vfd-panel` `repeating-linear-gradient` period (2px
/// "on" + 2px "off" = 4px), so anything using this mask reads with the
/// same interlaced look as the panel background behind it.
const SCANLINE_PERIOD_PX: f32 = 4.0;
/// The "off" band's alpha multiplier — not fully transparent (that broke
/// legibility on `ScrollFadeLabel`'s glow text at smaller font sizes), just
/// dimmed relative to the "on" band.
const SCANLINE_DIM_ALPHA: f32 = 0.7;

/// Pushes a `MaskMode::Alpha` layer whose mask is a vertical alternating-
/// alpha stripe pattern (`SCANLINE_PERIOD_PX` pitch, full alpha vs
/// `SCANLINE_DIM_ALPHA`), covering `(0, 0)..(width, height)` in the
/// snapshot's current (already-translated) coordinate space — built from
/// `append_repeating_linear_gradient` (one hard-edged bright/dim period,
/// `start_point`→`end_point` spanning exactly `SCANLINE_PERIOD_PX`, tiled
/// by GSK itself across the rest of `bounds`), not a hand-rolled loop of
/// many stops. Caller must `pop()` once more after drawing the content
/// this masks — mirrors `push_mask`'s own "push mask node, pop, push
/// content, pop" contract.
pub(crate) fn push_scanline_mask(snapshot: &gtk::Snapshot, width: f32, height: f32) {
    snapshot.push_mask(gtk::gsk::MaskMode::Alpha);
    let bright = gtk::gdk::RGBA::new(1.0, 1.0, 1.0, 1.0);
    let dim    = gtk::gdk::RGBA::new(1.0, 1.0, 1.0, SCANLINE_DIM_ALPHA);
    let stops = [
        gtk::gsk::ColorStop::new(0.0, bright),
        gtk::gsk::ColorStop::new(0.5, bright),
        gtk::gsk::ColorStop::new(0.5, dim),
        gtk::gsk::ColorStop::new(1.0, dim),
    ];
    snapshot.append_repeating_linear_gradient(
        &gtk::graphene::Rect::new(0.0, 0.0, width, height),
        &gtk::graphene::Point::new(0.0, 0.0),
        &gtk::graphene::Point::new(0.0, SCANLINE_PERIOD_PX),
        &stops,
    );
    snapshot.pop(); // end mask, begin content
}

/// Paints semi-transparent black stripes (`SCANLINE_PERIOD_PX` pitch, dark
/// band alpha `1.0 - SCANLINE_DIM_ALPHA`) covering `(0, 0)..(width,
/// height)` as ordinary content — meant to be the *entire* rendering of a
/// small widget overlaid on top of whatever it should dim (see
/// `VfdScanlineOverlay`), not a mask around something else in the same
/// snapshot.
pub(crate) fn paint_scanline_dim(snapshot: &gtk::Snapshot, width: f32, height: f32) {
    let clear = gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0);
    let dark  = gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 1.0 - SCANLINE_DIM_ALPHA);
    let stops = [
        gtk::gsk::ColorStop::new(0.0, clear),
        gtk::gsk::ColorStop::new(0.5, clear),
        gtk::gsk::ColorStop::new(0.5, dark),
        gtk::gsk::ColorStop::new(1.0, dark),
    ];
    snapshot.append_repeating_linear_gradient(
        &gtk::graphene::Rect::new(0.0, 0.0, width, height),
        &gtk::graphene::Point::new(0.0, 0.0),
        &gtk::graphene::Point::new(0.0, SCANLINE_PERIOD_PX),
        &stops,
    );
}
