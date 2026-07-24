//! # EngravedLabel
//!
//! A small GTK4 widget rendering a short, static line of text "engraved
//! into the wood" — RustyWiiM Wood's VFD-panel captions ("Title"/"Artist"/
//! "Album" above each backlit readout in Kiosk mode). Ported from a
//! previous branch experiment (`wood-engrave-preview.rs`'s standalone
//! prototype, and `ScrollFadeLabel`'s own once-production "engraved" mode
//! on that same branch) — kept as its own widget here rather than folded
//! back into `ScrollFadeLabel`: these captions are short, static, and never
//! scroll/swipe, so none of that widget's marquee machinery (loop timer,
//! hover-pause, fade mask) applies, and `ScrollFadeLabel` itself stays
//! scoped to the VFD glow/drop-shadow looks it already has.
//!
//! The glyph interior is left fully transparent — whatever sits behind this
//! widget (Wood's wood-grain window background) shows straight through,
//! delineated only by a thin two-tone groove line tracing the true glyph
//! outline (a light rim on one side, a dark shadow on the other) plus a
//! faint interior tone wash, the way a real router-carved wood sign reads.
//! Not reachable via GSK's `append_layout` (which only fills) or CSS
//! `text-shadow` (this widget renders through its own `snapshot()`, not
//! GTK's normal label/style pipeline) — built by rasterizing through
//! Cairo/Pango instead, see `engraved_texture()` below.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

pub mod imp {
    use std::cell::RefCell;

    use gtk::cairo;
    use gtk::glib;
    use gtk::glib::translate::IntoGlib;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;

    #[derive(Default)]
    pub struct EngravedLabel {
        pub text: RefCell<String>,
        // Cached pango layout — rebuilt on text/font change.
        pub layout_cache: RefCell<Option<gtk::pango::Layout>>,
        // Cached rasterized groove texture + its placement rect (relative
        // to this widget's own origin) — see `engraved_texture()`; real
        // work (a Cairo surface plus mask compositing and a manual blur),
        // so cached like `layout_cache` and cleared at the same sites.
        pub texture_cache: RefCell<Option<(gtk::gdk::Texture, gtk::graphene::Rect)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EngravedLabel {
        const NAME: &'static str = "EngravedLabel";
        type Type = super::EngravedLabel;
        type ParentType = gtk::Widget;
        // No layout manager — no child widgets.
    }

    impl ObjectImpl for EngravedLabel {}

    impl WidgetImpl for EngravedLabel {
        fn system_setting_changed(&self, settings: &gtk::SystemSetting) {
            self.parent_system_setting_changed(settings);
            *self.layout_cache.borrow_mut() = None;
            *self.texture_cache.borrow_mut() = None;
            self.obj().queue_resize();
        }

        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let layout = self.create_layout();
            let (tw, th) = layout.pixel_size();
            if orientation == gtk::Orientation::Vertical {
                (th, th, -1, -1)
            } else {
                (tw, tw, -1, -1)
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let width  = widget.width()  as f32;
            let height = widget.height() as f32;
            if width <= 0.0 || height <= 0.0 { return; }

            let layout = self.create_layout();
            let Some((texture, rect)) = self.engraved_texture(&layout) else { return };
            // Left-aligned (this widget always sits in a left-justified
            // column) at `rect`'s own x — a plain `append_layout` call at
            // the origin would place the glyphs' ink starting there too,
            // same convention — only re-centered vertically within
            // whatever extra height the allocation gives beyond the
            // texture's own.
            let y = ((height - rect.height()) / 2.0).floor();
            let placed = gtk::graphene::Rect::new(rect.x(), y, rect.width(), rect.height());
            snapshot.append_texture(&texture, &placed);
        }
    }

    /// Disables hinting on `cr` and returns a *fresh* `pango::Layout`,
    /// natively created for (and only ever used with) this exact context —
    /// copying `source`'s text/font description/attributes over rather
    /// than reusing `source` itself across contexts, which is its own,
    /// separately-diagnosed failure mode (glyphs rendering as raw bounding
    /// boxes — see `ScrollFadeLabel`'s git history on the `theming-wood-
    /// experiment` branch for the full story if this ever needs
    /// revisiting). `source`'s attributes already carry the named-static-
    /// instance substitution `create_layout()` applies below, so this
    /// doesn't need to re-derive it.
    fn raster_layout(cr: &cairo::Context, source: &gtk::pango::Layout) -> Option<gtk::pango::Layout> {
        let mut options = cairo::FontOptions::new().ok()?;
        options.set_hint_style(cairo::HintStyle::None);
        cr.set_font_options(&options);
        let layout = pangocairo::functions::create_layout(cr);
        layout.set_text(&source.text());
        layout.set_font_description(source.context().font_description().as_ref());
        layout.set_attributes(source.attributes().as_ref());
        layout.set_single_paragraph_mode(true);
        layout.set_width(-1);
        Some(layout)
    }

    /// If the context's resolved font family has a named static instance
    /// matching its resolved weight (`weight_name()`), returns a Pango
    /// attribute list overriding *just* the family for the whole text —
    /// an attribute rather than replacing the whole `FontDescription` since
    /// only this one field should ever be pinned; size/weight/everything
    /// else stays live, inherited from `context` exactly as it would
    /// without this at all. `None` if the family has no such instance (the
    /// common case) or the context has no resolved description yet.
    ///
    /// This exists because some variable fonts (confirmed live: Adwaita
    /// Sans) have a genuine rendering bug in their runtime variable-axis
    /// interpolation that shows up specifically when tracing glyph
    /// *outlines* (`pangocairo::functions::layout_path`, which
    /// `engraved_texture()` below needs) — some glyphs come out as their
    /// raw bounding box instead of their real shape. Requesting the font's
    /// separately-registered **named static instance** instead (fontconfig
    /// exposes each of a variable font's named instances as its own family
    /// string, e.g. `"Adwaita Sans Regular"` alongside the continuous base
    /// family `"Adwaita Sans"`) resolves straight to pre-instanced outline
    /// data with no runtime interpolation, sidestepping the bug entirely.
    /// Not a general "variable fonts are broken" issue — other variable
    /// fonts (e.g. Cantarell) render outlines correctly; this only kicks in
    /// for a font that actually has the bug, harmlessly falling back to the
    /// original family for every other font.
    fn named_instance_attrs(context: &gtk::pango::Context) -> Option<gtk::pango::AttrList> {
        let desc = context.font_description()?;
        let family = desc.family()?;
        let instance = weight_name(desc.weight());
        use gtk::pango::prelude::FontFamilyExt;
        let exists = context
            .list_families()
            .iter()
            .find(|f| f.name().eq_ignore_ascii_case(&family))
            .and_then(|f| f.face(Some(instance)))
            .is_some();
        if !exists {
            return None;
        }
        let mut family_only = gtk::pango::FontDescription::new();
        family_only.set_family(&format!("{family} {instance}"));
        let mut attr = gtk::pango::AttrFontDesc::new(&family_only).upcast();
        attr.set_start_index(0);
        attr.set_end_index(gtk::pango::ATTR_INDEX_TO_TEXT_END);
        let attrs = gtk::pango::AttrList::new();
        attrs.insert(attr);
        Some(attrs)
    }

    /// Standard OpenType/CSS weight-axis instance names, matching what
    /// variable fonts commonly register their named static instances as
    /// (e.g. Adwaita Sans's own `fc-list` output: Thin/ExtraLight/Light/
    /// Regular/Medium/SemiBold/Bold/ExtraBold/Black).
    fn weight_name(weight: gtk::pango::Weight) -> &'static str {
        match weight.into_glib() {
            ..=149 => "Thin",
            150..=249 => "ExtraLight",
            250..=349 => "Light",
            350..=449 => "Regular",
            450..=549 => "Medium",
            550..=649 => "SemiBold",
            650..=749 => "Bold",
            750..=849 => "ExtraBold",
            850.. => "Black",
        }
    }

    // ---- "engraved into the wood" look — tuning knobs ----
    // Everything here is a matter of taste rather than correctness, kept in
    // one place so it can be adjusted without hunting through the
    // rendering pipeline. Colours are (r, g, b, a) in 0.0..=1.0, tuned live
    // on the originating branch against Wood's actual wood-grain
    // background.

    /// Padding (px) around the glyph ink rect in the raster surface — the
    /// stroked highlight outline extends slightly past the glyphs' own
    /// fill bounds, so this needs to be at least that much or it clips at
    /// the surface edge.
    const ENGRAVED_PAD: f64 = 3.0;

    /// How far (px) the shadow's "cut edge" is shifted toward the light
    /// direction before blurring. Bigger = the dark shading reaches
    /// further into each letter's interior before fading out.
    const ENGRAVED_SHADOW_OFFSET: f64 = 2.0;

    /// Blur radius (px) applied to the shifted shadow mask. Bigger = a
    /// softer, more gradual falloff; smaller = a crisper, more defined
    /// shadow edge.
    const ENGRAVED_SHADOW_BLUR_RADIUS: i32 = 3;

    /// Shadow colour. Kept near-opaque deliberately (not a softer partial
    /// blend) — it needs to fully mask the highlight outline underneath
    /// wherever it covers, not just tint it (see step 2/3's comments in
    /// `engraved_texture()`).
    const ENGRAVED_SHADOW_COLOR: (f64, f64, f64, f64) = (0.0, 0.0, 0.0, 0.92);

    /// Interior tone wash across the whole glyph interior. Lighter than
    /// the surrounding wood, not darker — a freshly-routed groove exposes
    /// less weathered wood, reading lighter than the aged surface around
    /// it.
    const ENGRAVED_INTERIOR_COLOR: (f64, f64, f64, f64) = (0.92, 0.78, 0.63, 0.3);

    /// Highlight outline colour, drawn underneath the shadow layer so the
    /// shadow's own coverage determines which side of each letter it ends
    /// up visible on.
    const ENGRAVED_HIGHLIGHT_COLOR: (f64, f64, f64, f64) = (1.0, 0.95, 0.85, 0.30);

    /// Highlight outline stroke width (px).
    const ENGRAVED_HIGHLIGHT_STROKE_WIDTH: f64 = 1.5;

    /// Fraction of `ENGRAVED_SHADOW_OFFSET` the highlight outline itself is
    /// nudged toward the highlight corner, biasing its own footprint away
    /// from the shadow's coverage to begin with, rather than relying
    /// solely on the shadow's opacity to hide it.
    const ENGRAVED_HIGHLIGHT_OFFSET_FRACTION: f64 = 0.3;

    /// A white-filled silhouette of `layout`'s glyphs, translated by `(tx,
    /// ty)`, sized `w`x`h` (the same size/coordinate-space as the final
    /// output surface these helpers all feed into).
    fn render_text_mask(
        w: i32,
        h: i32,
        layout: &gtk::pango::Layout,
        tx: f64,
        ty: f64,
    ) -> Option<cairo::ImageSurface> {
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).ok()?;
        let cr = cairo::Context::new(&surface).ok()?;
        let layout = raster_layout(&cr, layout)?;
        cr.translate(tx, ty);
        cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        pangocairo::functions::show_layout(&cr, &layout);
        drop(cr);
        Some(surface)
    }

    /// Opaque white everywhere *except* the glyph silhouette (left
    /// transparent), with the glyphs positioned at `(tx + dx, ty + dy)` —
    /// i.e. the inverted mask of the glyphs as they'd sit if shifted by
    /// `(dx, dy)`, not of their true position. This is "the surrounding
    /// material's cut edge, shifted toward the light source," blurred into
    /// a shadow by the caller next.
    fn render_inverted_mask_shifted(
        w: i32,
        h: i32,
        layout: &gtk::pango::Layout,
        tx: f64,
        ty: f64,
        dx: f64,
        dy: f64,
    ) -> Option<cairo::ImageSurface> {
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).ok()?;
        let cr = cairo::Context::new(&surface).ok()?;
        let layout = raster_layout(&cr, layout)?;
        cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        cr.paint().ok()?;
        cr.set_operator(cairo::Operator::Clear);
        cr.save().ok()?;
        cr.translate(tx + dx, ty + dy);
        pangocairo::functions::show_layout(&cr, &layout);
        cr.restore().ok()?;
        drop(cr);
        Some(surface)
    }

    /// Separable box blur over all 4 premultiplied ARGB32 channels — Cairo
    /// has no built-in blur (unlike GSK's `push_blur`, which can't be used
    /// here since the result needs to be baked once into the cached
    /// texture bytes below, not applied live as a snapshot render-node
    /// every frame).
    fn box_blur(surface: &mut cairo::ImageSurface, radius: i32) -> Option<cairo::ImageSurface> {
        let w = surface.width();
        let h = surface.height();
        let stride = surface.stride();
        let src: Vec<u8> = surface.data().ok()?.to_vec();

        let mut tmp = vec![0u8; src.len()];
        for y in 0..h {
            for x in 0..w {
                let mut sum = [0u32; 4];
                let mut count = 0u32;
                for dx in -radius..=radius {
                    let xx = x + dx;
                    if xx < 0 || xx >= w { continue; }
                    let idx = (y * stride + xx * 4) as usize;
                    for c in 0..4 { sum[c] += src[idx + c] as u32; }
                    count += 1;
                }
                let idx = (y * stride + x * 4) as usize;
                for c in 0..4 { tmp[idx + c] = (sum[c] / count) as u8; }
            }
        }
        let mut out = vec![0u8; src.len()];
        for y in 0..h {
            for x in 0..w {
                let mut sum = [0u32; 4];
                let mut count = 0u32;
                for dy in -radius..=radius {
                    let yy = y + dy;
                    if yy < 0 || yy >= h { continue; }
                    let idx = (yy * stride + x * 4) as usize;
                    for c in 0..4 { sum[c] += tmp[idx + c] as u32; }
                    count += 1;
                }
                let idx = (y * stride + x * 4) as usize;
                for c in 0..4 { out[idx + c] = (sum[c] / count) as u8; }
            }
        }

        let mut result = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).ok()?;
        {
            let mut data = result.data().ok()?;
            data.copy_from_slice(&out);
        }
        Some(result)
    }

    /// Per-pixel alpha product of two same-sized ARGB32 surfaces — used to
    /// restrict the blurred shadow to only the part landing inside the
    /// original (unshifted) glyph shape; the part outside doesn't matter
    /// and is discarded (alpha forced to 0) here.
    fn multiply_alpha(
        a: &mut cairo::ImageSurface,
        b: &mut cairo::ImageSurface,
    ) -> Option<cairo::ImageSurface> {
        let w = a.width();
        let h = a.height();
        let stride_a = a.stride();
        let stride_b = b.stride();
        let da: Vec<u8> = a.data().ok()?.to_vec();
        let db: Vec<u8> = b.data().ok()?.to_vec();
        let mut result = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).ok()?;
        let result_stride = result.stride();
        let mut out = vec![0u8; (result_stride * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let ia = (y * stride_a + x * 4) as usize;
                let ib = (y * stride_b + x * 4) as usize;
                let io = (y * result_stride + x * 4) as usize;
                let alpha_a = da[ia + 3] as u32;
                let alpha_b = db[ib + 3] as u32;
                let alpha = (alpha_a * alpha_b) / 255;
                // Premultiplied white * alpha == (alpha, alpha, alpha, alpha).
                out[io] = alpha as u8;
                out[io + 1] = alpha as u8;
                out[io + 2] = alpha as u8;
                out[io + 3] = alpha as u8;
            }
        }
        {
            let mut data = result.data().ok()?;
            data.copy_from_slice(&out);
        }
        Some(result)
    }

    impl EngravedLabel {
        /// Rasterizes `layout`'s glyph *outlines* (not fill) as a thin
        /// two-tone groove line — a dark stroke offset down-right (the
        /// shadow) plus a light stroke offset up-left (the highlight),
        /// composited over a light interior tone wash — into a small
        /// transparent Cairo surface, wrapped as a `gdk::Texture` plus the
        /// rect (in this widget's own coordinate space) to place it at.
        /// Cached in `texture_cache`: unlike a plain per-frame GSK call,
        /// this does real work (a Cairo surface plus mask compositing and
        /// a manual blur), and the result only changes when the text or
        /// font does.
        fn engraved_texture(
            &self,
            layout: &gtk::pango::Layout,
        ) -> Option<(gtk::gdk::Texture, gtk::graphene::Rect)> {
            if let Some((texture, rect)) = self.texture_cache.borrow().as_ref() {
                return Some((texture.clone(), rect.clone()));
            }

            // Measure using the *same* (possibly font-instance-substituted
            // — see `create_layout()`/`named_instance_attrs()`) layout,
            // bound to an actual Cairo context the way it's about to
            // actually be drawn — `pixel_extents()` on a layout that's
            // never been synced to any real target context is a bit of a
            // fiction, so measure the same way the drawing below does
            // rather than calling `layout.pixel_extents()` directly.
            let probe_surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 1, 1).ok()?;
            let probe_cr = cairo::Context::new(&probe_surface).ok()?;
            let measured_layout = raster_layout(&probe_cr, layout)?;
            let (ink, _logical) = measured_layout.pixel_extents();
            if ink.width() <= 0 || ink.height() <= 0 {
                return None;
            }
            let surface_w = (ink.width() as f64 + ENGRAVED_PAD * 2.0).ceil() as i32;
            let surface_h = (ink.height() as f64 + ENGRAVED_PAD * 2.0).ceil() as i32;

            let mut surface =
                cairo::ImageSurface::create(cairo::Format::ARgb32, surface_w, surface_h).ok()?;
            let cr = cairo::Context::new(&surface).ok()?;
            // Where the ink rect's own top-left corner (which can be
            // offset from the layout's logical origin, e.g. italic
            // overhang) lands in surface space — baked explicitly into
            // each draw call below rather than one global `cr.translate`,
            // since this technique mixes drawing straight onto `cr` with
            // compositing in separately-built mask surfaces that need the
            // same offset baked into *their* own rendering.
            let tx = ENGRAVED_PAD - ink.x() as f64;
            let ty = ENGRAVED_PAD - ink.y() as f64;

            let mut original_mask = render_text_mask(surface_w, surface_h, layout, tx, ty)?;

            // 1. Interior tone wash across the whole glyph interior —
            //    lighter than the surrounding wood, not darker (a
            //    freshly-routed groove exposes less weathered wood). The
            //    shadow step below darkens part of this back down near
            //    the top-left edge, so the overall grade still runs
            //    dark-to-light, just starting from a lighter baseline
            //    than the background rather than a flat/transparent one.
            let (r, g, b, a) = ENGRAVED_INTERIOR_COLOR;
            cr.set_source_rgba(r, g, b, a);
            cr.mask_surface(&original_mask, 0.0, 0.0).ok()?;

            // 2. Faint highlight outline, on top of the interior tone but
            //    underneath the shadow (step 3) — the shadow is meant to
            //    fully swallow this on its own (top-left) side, leaving
            //    only the bottom-right side exposed as a highlight rim.
            //    Nudged toward the highlight corner by a fraction of the
            //    shadow's own offset so its footprint is biased away from
            //    the shadow's coverage to begin with, rather than relying
            //    on the shadow's opacity alone to hide it.
            cr.save().ok()?;
            let highlight_offset = ENGRAVED_SHADOW_OFFSET * ENGRAVED_HIGHLIGHT_OFFSET_FRACTION;
            cr.translate(tx + highlight_offset, ty + highlight_offset);
            let outline_layout = raster_layout(&cr, layout)?;
            pangocairo::functions::layout_path(&cr, &outline_layout);
            let (r, g, b, a) = ENGRAVED_HIGHLIGHT_COLOR;
            cr.set_source_rgba(r, g, b, a);
            cr.set_line_width(ENGRAVED_HIGHLIGHT_STROKE_WIDTH);
            cr.set_line_join(cairo::LineJoin::Round);
            cr.stroke().ok()?;
            cr.restore().ok()?;

            // 3. Inverted mask of the glyphs (opaque everywhere *except*
            //    the glyph shape), shifted toward the shadow direction and
            //    blurred — the surrounding material's cut edge, moved
            //    toward where its shadow should fall, then softened.
            //    Restricted to the part landing inside the *original*
            //    unshifted glyph shape (the part outside doesn't matter —
            //    it'd just be shadow on the plain background — and is
            //    discarded via `multiply_alpha`). Painted near-opaque (not
            //    a partial blend) specifically so it actually hides the
            //    highlight outline underneath wherever it covers, rather
            //    than letting some of that outline's own colour still show
            //    through the "over" compositing math.
            let mut shifted_inverted = render_inverted_mask_shifted(
                surface_w, surface_h, layout, tx, ty, ENGRAVED_SHADOW_OFFSET, ENGRAVED_SHADOW_OFFSET,
            )?;
            let mut blurred = box_blur(&mut shifted_inverted, ENGRAVED_SHADOW_BLUR_RADIUS)?;
            let combined = multiply_alpha(&mut blurred, &mut original_mask)?;
            let (r, g, b, a) = ENGRAVED_SHADOW_COLOR;
            cr.set_source_rgba(r, g, b, a);
            cr.mask_surface(&combined, 0.0, 0.0).ok()?;

            drop(cr);

            let stride = surface.stride();
            let width  = surface.width();
            let height = surface.height();
            let bytes = {
                let data = surface.data().ok()?;
                glib::Bytes::from(&*data)
            };

            let texture = gtk::gdk::MemoryTexture::new(
                width,
                height,
                gtk::gdk::MemoryFormat::B8g8r8a8Premultiplied,
                &bytes,
                stride as usize,
            );
            let texture: gtk::gdk::Texture = texture.upcast();

            let rect = gtk::graphene::Rect::new(
                ink.x() as f32 - ENGRAVED_PAD as f32,
                ink.y() as f32 - ENGRAVED_PAD as f32,
                width as f32,
                height as f32,
            );

            *self.texture_cache.borrow_mut() = Some((texture.clone(), rect.clone()));
            Some((texture, rect))
        }

        fn create_layout(&self) -> gtk::pango::Layout {
            let mut cache = self.layout_cache.borrow_mut();
            if let Some(ref layout) = *cache {
                return layout.clone(); // GObject clone — cheap refcount bump
            }
            let context = self.obj().pango_context();
            let layout = gtk::pango::Layout::new(&context);
            layout.set_text(&self.text.borrow());
            layout.set_single_paragraph_mode(true);
            layout.set_width(-1);
            // See `named_instance_attrs()`'s doc comment: the one and only
            // place the named-static-instance substitution (for fonts that
            // need it, e.g. Adwaita Sans) is decided, as an attribute that
            // overrides just the family — every measurement and rendering
            // path this widget has reads from this same layout, so nothing
            // could independently disagree with it.
            if let Some(attrs) = named_instance_attrs(&context) {
                layout.set_attributes(Some(&attrs));
            }
            *cache = Some(layout.clone());
            layout
        }
    }
}

glib::wrapper! {
    pub struct EngravedLabel(ObjectSubclass<imp::EngravedLabel>)
        @extends gtk::Widget;
}

impl EngravedLabel {
    pub fn new(text: &str) -> Self {
        let obj: Self = glib::Object::new();
        obj.set_text(text);
        obj
    }

    pub fn set_text(&self, text: &str) {
        let imp = self.imp();
        imp.text.replace(text.to_string());
        *imp.layout_cache.borrow_mut() = None;
        *imp.texture_cache.borrow_mut() = None;
        self.queue_resize();
    }

    /// Apply a CSS class directly to this widget so font rules reach it —
    /// only font family/size/weight matter here (the glyph *fill* colour
    /// from CSS is never used — see this module's own doc comment).
    pub fn add_label_css_class(&self, class: &str) {
        self.add_css_class(class);
    }

    /// Forces a full style-context recompute (not just a layout pass) —
    /// same technique and same reason as `ScrollFadeLabel::force_restyle()`:
    /// a plain CSS-provider reload (e.g. Kiosk's WideRight per-screen-size
    /// rescale, which writes a fresh scoped `.vfd-caption { font-size }`
    /// rule) doesn't reliably make this widget's cached layout notice the
    /// new resolved font on its own; removing and re-adding this widget's
    /// CSS classes forces GTK to recompute style, then the cache clear here
    /// picks up the result.
    pub fn force_restyle(&self) {
        let classes: Vec<String> = self.css_classes().iter().map(|s| s.to_string()).collect();
        self.set_css_classes(&[]);
        self.set_css_classes(&classes.iter().map(String::as_str).collect::<Vec<_>>());
        let imp = self.imp();
        *imp.layout_cache.borrow_mut() = None;
        *imp.texture_cache.borrow_mut() = None;
        self.queue_resize();
    }
}

impl Default for EngravedLabel {
    fn default() -> Self { glib::Object::new() }
}
