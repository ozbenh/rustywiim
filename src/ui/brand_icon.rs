//! `BrandIcon` — displays a third-party streaming-service/brand-mark SVG
//! (Spotify, TIDAL, ...) as a flat, pure black-or-white silhouette that
//! tracks the active theme — these are governed by each brand's own usage
//! guidelines, so unlike a plain symbolic icon the actual *shape* can't be
//! touched, but a flat monochrome recolor (as opposed to some arbitrary
//! accent tint) is exactly the kind of light/dark adaptation brand kits
//! themselves expect an icon to make.
//!
//! Renders the SVG itself, at display time, at the exact size/color
//! needed, using `resvg` (a self-contained, pure-Rust renderer — no system
//! librsvg/gdk-pixbuf involved, so no dependency on the host's own SVG
//! rendering stack at all). The bundled SVGs already carry the standard
//! GNOME/Adwaita "#2e3436" symbolic recolor-placeholder fill (same
//! convention the real `-symbolic` icons elsewhere in this app use) —
//! that value is irrelevant here, since recoloring is done via
//! `usvg::Options::style_sheet`, a CSS override injected ahead of the
//! document's own styling with `!important`, forcing every path to the
//! requested flat color regardless of whatever the source file itself
//! specifies (attribute, inline `style=`, or an internal `<style>`
//! block) — the same mechanism librsvg's own `set_stylesheet()` exists
//! for, just reimplemented on a renderer we fully control.
//!
//! **Fixed height, variable width** — not a square icon slot: several of
//! these marks are genuinely wide wordmarks, not square/circular marks
//! (confirmed content aspect ratios: Qobuz 2.4:1, TuneIn 2.27:1, Amazon
//! 1.54:1, Tidal 1.5:1), and `gtk::IconTheme`'s own icon lookup — the
//! alternative, real-`-symbolic`-icon approach tried first — bakes every
//! icon as a *square* raster unconditionally (confirmed live:
//! `GtkIconPaintable::intrinsic_aspect_ratio()` reports exactly `1` no
//! matter what the source SVG's own proportions are), which would
//! stretch or letterbox these wordmarks rather than showing them at their
//! true shape. `set_svg()` parses the source once (via `usvg`, cheap —
//! no rendering) purely to read its real `viewBox` aspect ratio, cached
//! and combined with the current height (`set_height()`) into a plain
//! fixed `width_px` (`recompute_width()`) reported from `measure()` as an
//! ordinary constant — *not* `SizeRequestMode::WidthForHeight`, which was
//! tried first and confirmed live to misbehave: GTK's own internal
//! consistency-checking probes `measure(Horizontal, for_size)` with more
//! than one `for_size` (including a large sanity-check value), and a
//! width that's linearly proportional to whatever `for_size` it's asked
//! with fails that check — GTK then distrusts the result and silently
//! keeps whatever it cached from the very first query, ignoring every
//! later `set_height()` entirely.
//!
//! Deliberately **re-renders on every `snapshot()` call for now, no
//! caching** — simple and correct first; a cache keyed on (svg identity,
//! color, size) can be added later if re-rendering every frame ever shows
//! up as a real cost, but these are single-path icons at icon sizes, not
//! a hot path worth optimizing pre-emptively.
//!
//! Not a `gtk::Image` subclass: `GtkImage` is a final GObject class (no
//! subclass support at all, gtk4-rs included), so this reimplements just
//! the small slice of `Image`'s behavior actually needed here.

pub mod imp {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use adw::prelude::*;
    use gtk::glib;
    use gtk::subclass::prelude::*;
    use gtk::{graphene, gsk};

    pub struct BrandIcon {
        svg:       RefCell<Option<Rc<[u8]>>>,
        height_px: Cell<i32>,
        /// Width/height, read from the source SVG's own `viewBox` when
        /// `set_svg()` parses it — *not* derived from any GTK icon
        /// lookup, which would report a meaningless `1.0` (see module
        /// doc comment). Defaults to `1.0` (square) until a real SVG is
        /// set.
        aspect:    Cell<f64>,
        /// `round(height_px * aspect)`, recomputed by `recompute_width()`
        /// whenever either input changes — **not** derived live from
        /// `measure()`'s own `for_size` parameter via `SizeRequestMode::
        /// WidthForHeight`, which was tried first and confirmed live to
        /// misbehave: GTK's own size-request consistency checking probes
        /// `measure(Horizontal, for_size)` with more than one `for_size`
        /// (including a huge sanity-check value — confirmed live via a
        /// real `Gtk-CRITICAL`: "GtkBox ... reports a minimum width of
        /// 150, but minimum width for height of 1048576 is 166"), and a
        /// width that's linearly proportional to whatever `for_size` it's
        /// asked with fails that consistency check — GTK then distrusts
        /// the result and falls back to whatever it cached from the
        /// *first* query, silently ignoring every later `set_height()`
        /// (confirmed live: changing the height had zero visible effect,
        /// always showing the size from construction time). Precomputing
        /// one fixed width ourselves and reporting it as an ordinary
        /// constant avoids the whole negotiation.
        width_px:  Cell<i32>,
    }

    impl Default for BrandIcon {
        fn default() -> Self {
            Self {
                svg: RefCell::default(),
                height_px: Cell::default(),
                aspect: Cell::new(1.0),
                width_px: Cell::default(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BrandIcon {
        const NAME: &'static str = "RustyWiimBrandIcon";
        type Type = super::BrandIcon;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for BrandIcon {}

    impl WidgetImpl for BrandIcon {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            match orientation {
                gtk::Orientation::Vertical => {
                    let h = self.height_px.get().max(0);
                    (h, h, -1, -1)
                }
                _ => {
                    let w = self.width_px.get().max(0);
                    (w, w, -1, -1)
                }
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let Some(svg) = self.svg.borrow().clone() else { return };
            let w = self.obj().width();
            let h = self.obj().height();
            if w <= 0 || h <= 0 { return; }

            // Render at the surface's real device-pixel size, not the
            // logical one, so a HiDPI (scale-2+) display doesn't get a
            // blurry upscale of a logical-size raster.
            //
            // Sharpness at small sizes on a 4K/HiDPI screen was reported
            // as slightly soft (2026-07-22) — tried supersampling above
            // this and letting the scaled-texture filter downscale it
            // (worse: more blurry), then tried a plain bilinear filter
            // instead of Trilinear below (also worse). Neither panned
            // out; left as Trilinear, which is at least no worse than the
            // alternatives tried so far.
            let scale = self.obj().scale_factor().max(1);
            let px_w = (w * scale) as u32;
            let px_h = (h * scale) as u32;

            // Pure black or pure white — no other color is ours to choose,
            // these are third-party brand marks (see module doc comment).
            let color = if adw::StyleManager::default().is_dark() { "white" } else { "black" };

            if let Some(texture) = super::render_svg(&svg, color, px_w, px_h) {
                let bounds = graphene::Rect::new(0.0, 0.0, w as f32, h as f32);
                snapshot.append_scaled_texture(&texture, gsk::ScalingFilter::Trilinear, &bounds);
            }
        }
    }

    impl BrandIcon {
        fn recompute_width(&self) {
            let w = (self.height_px.get() as f64 * self.aspect.get()).round() as i32;
            self.width_px.set(w.max(0));
        }

        pub(super) fn set_svg(&self, svg: Option<Rc<[u8]>>) {
            let aspect = svg.as_deref()
                .and_then(super::svg_aspect_ratio)
                .unwrap_or(1.0);
            self.aspect.set(aspect);
            self.svg.replace(svg);
            self.recompute_width();
            self.obj().queue_resize();
            self.obj().queue_draw();
        }

        pub(super) fn set_height(&self, px: i32) {
            self.height_px.set(px);
            self.recompute_width();
            self.obj().queue_resize();
        }
    }
}

use std::rc::Rc;

use gtk::gdk;
use gtk::glib;
use gtk::subclass::prelude::*;

glib::wrapper! {
    pub struct BrandIcon(ObjectSubclass<imp::BrandIcon>)
        @extends gtk::Widget;
}

impl BrandIcon {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// `svg` is the brand mark's raw SVG source (see
    /// `IconSet::service_svg()`) — re-rendered fresh at the right
    /// color/size on every paint, not a ready-made paintable.
    pub fn set_svg(&self, svg: Option<Rc<[u8]>>) {
        self.imp().set_svg(svg);
    }

    /// Fixed display height in logical pixels — width follows the
    /// source SVG's own aspect ratio (see module doc comment), not a
    /// square slot.
    pub fn set_height(&self, px: i32) {
        self.imp().set_height(px);
    }
}

impl Default for BrandIcon {
    fn default() -> Self { Self::new() }
}

/// The source SVG's own `viewBox` aspect ratio (width/height), read via a
/// throwaway `usvg` parse (no rendering) — `None` if `svg` isn't valid
/// UTF-8/SVG. Deliberately independent of any GTK icon lookup, which
/// would report a meaningless `1.0` regardless of the source's real
/// proportions (see module doc comment).
fn svg_aspect_ratio(svg: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(svg).ok()?;
    let tree = resvg::usvg::Tree::from_str(text, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    if size.width() <= 0.0 || size.height() <= 0.0 { return None; }
    Some(size.width() as f64 / size.height() as f64)
}

/// Parse+rasterize `svg` at `width`×`height` device pixels, with every
/// path forced to `color` (a CSS color keyword/hex, e.g. `"black"`) via an
/// injected `!important` stylesheet rule — overrides whatever fill the
/// source document itself specifies, so the result is always a flat,
/// single-color silhouette regardless of the source's own styling. The
/// source's own aspect ratio is preserved — scaled uniformly to fit
/// inside `width`×`height` and centred (in practice an exact fit, not
/// just "contain": callers size the widget itself to match this same
/// aspect ratio via `svg_aspect_ratio()`, so there's no letterboxing to
/// actually see — this is just defensive against rounding).
fn render_svg(svg: &[u8], color: &str, width: u32, height: u32) -> Option<gdk::MemoryTexture> {
    if width == 0 || height == 0 { return None; }
    let text = std::str::from_utf8(svg).ok()?;

    let opt = resvg::usvg::Options {
        style_sheet: Some(format!("* {{ fill: {color} !important; }}")),
        ..Default::default()
    };
    let tree = resvg::usvg::Tree::from_str(text, &opt).ok()?;

    let size = tree.size();
    if size.width() <= 0.0 || size.height() <= 0.0 { return None; }

    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
    let scale = (width as f32 / size.width()).min(height as f32 / size.height());
    let tx = (width as f32  - size.width()  * scale) / 2.0;
    let ty = (height as f32 - size.height() * scale) / 2.0;
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale).post_translate(tx, ty);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    let stride = (width as usize) * 4;
    let bytes = glib::Bytes::from(pixmap.data());
    Some(gdk::MemoryTexture::new(
        width as i32, height as i32,
        gdk::MemoryFormat::R8g8b8a8Premultiplied,
        &bytes, stride,
    ))
}
