//! # FlipCover
//!
//! A GTK4 widget showing the "cover" area: album artwork, or a fallback
//! source icon when there's none. Two transition styles, chosen
//! automatically based on what's transitioning to what:
//!
//! - **Flip** — a 3D card-flip (`gsk::Transform` `perspective` + `rotate_3d`)
//!   between two pieces of real album art (an actual track change).
//! - **Fade** — a plain cross-dissolve, used whenever either side of the
//!   transition is the fallback icon (or there's nothing to flip from yet).
//!   This is what makes source switches — which often show the icon
//!   briefly before/after real art — look like a smooth cross-fade instead
//!   of an abrupt cut.
//!
//! Both textures and icon `gdk::Paintable`s are drawn via
//! `Paintable::snapshot()`, so the same code path handles either; no
//! offscreen "pixmap" render step is needed.

pub mod imp {
    use std::cell::{Cell, RefCell};

    use adw::prelude::*;
    use gtk::glib;
    use gtk::subclass::prelude::*;
    use gtk::{gdk, graphene, gsk};

    const FLIP_DURATION_MS: u32 = 450;
    const FADE_DURATION_MS: u32 = 300;

    /// A shown paintable plus its display policy: `None` = "contain"-fit to
    /// the widget's full box (real art); `Some(px)` = fixed square size,
    /// centred (the fallback icon, matching its old fixed `pixel_size`).
    type Content = (gdk::Paintable, Option<f32>);

    #[derive(Clone, Copy, PartialEq)]
    enum Transition { Flip, Fade }

    pub struct FlipCover {
        front:    RefCell<Option<Content>>, // currently shown
        back:     RefCell<Option<Content>>, // incoming, during a transition
        mode:     Cell<Transition>,
        progress: Cell<f32>,                // 0..1, meaning depends on `mode`
        last_key: RefCell<String>,          // de-dupe key (see set_content)
        anim:     RefCell<Option<adw::TimedAnimation>>,
        /// Opt-in for the theme-drawn frame (see `draw_theme_frame()`) —
        /// off by default, `set_frame_enabled(true)` on the two "hero"
        /// instances (main window, mini window) but left off for
        /// `devlist.rs`'s small rounded `.devlist-art` thumbnails, which a
        /// square physical-frame bevel would look wrong on. Instance-level
        /// opt-in, separate from whether the active theme actually draws
        /// anything (see that method) — the two gates are independent:
        /// this one is "does this instance want a frame at all", the
        /// other is "does the current theme have one to give it".
        frame_enabled: Cell<bool>,
    }

    impl Default for FlipCover {
        fn default() -> Self {
            Self {
                front:    RefCell::new(None),
                back:     RefCell::new(None),
                mode:     Cell::new(Transition::Fade),
                progress: Cell::new(0.0),
                last_key: RefCell::new(String::new()),
                anim:     RefCell::new(None),
                frame_enabled: Cell::new(false),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FlipCover {
        const NAME: &'static str = "FlipCover";
        type Type = super::FlipCover;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for FlipCover {
        fn dispose(&self) {
            // Split from `if let Some(a) = self.anim.borrow_mut().take() { a.skip(); }`:
            // the RefMut temporary from borrow_mut() stays alive for the whole
            // if-let block (its temporary scope extends past take()), so
            // self.anim would still be borrowed while skip() runs — and
            // skip() synchronously fires connect_done, which borrows
            // self.anim again and panics. Ending the borrow_mut() at this
            // statement's semicolon, before skip() runs, avoids that.
            let old_anim = self.anim.borrow_mut().take();
            if let Some(a) = old_anim {
                a.skip();
            }
        }
    }

    impl WidgetImpl for FlipCover {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            // No intrinsic size — the caller sizes this widget via
            // hexpand/vexpand and it "contain"-fits/centres its own content.
            let _ = orientation;
            (0, 0, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let w = self.obj().width()  as f32;
            let h = self.obj().height() as f32;
            if w <= 0.0 || h <= 0.0 { return; }

            let t = self.progress.get().clamp(0.0, 1.0);

            match self.mode.get() {
                Transition::Flip => {
                    let angle = t * 180.0;
                    let showing_back = angle > 90.0;
                    let side = if showing_back { self.back.borrow() } else { self.front.borrow() };
                    let Some((paintable, fixed_size)) = side.as_ref() else { return };

                    // perspective(depth): larger depth = weaker foreshortening.
                    let depth = 2.0 * w.max(h);
                    let cx = w / 2.0;
                    let cy = h / 2.0;
                    let transform = gsk::Transform::new()
                        .translate(&graphene::Point::new(cx, cy))
                        .perspective(depth)
                        .rotate_3d(angle, &graphene::Vec3::new(0.0, 1.0, 0.0))
                        // Past 90° the quad faces away, so its texture reads
                        // mirrored; pre-mirror the back face on X rather than
                        // flipping the image bytes.
                        .scale(if showing_back { -1.0 } else { 1.0 }, 1.0)
                        .translate(&graphene::Point::new(-cx, -cy));

                    snapshot.save();
                    snapshot.transform(Some(&transform));
                    draw_content(snapshot, paintable, *fixed_size, w, h);
                    // Drawn inside the same save()/transform()/restore() as
                    // the content, not after it (see draw_theme_frame()'s
                    // doc comment) — it then inherits the identical
                    // perspective/rotation *and* the showing_back mirror
                    // correction the content gets, for free: no separate
                    // "which side is highlighted" math needed to keep the
                    // frame's lit edge visually consistent with the
                    // (already correctly un-mirrored) card as it turns.
                    self.draw_theme_frame(snapshot, paintable, *fixed_size, w, h);
                    snapshot.restore();
                }
                Transition::Fade => {
                    if t < 1.0 {
                        if let Some((paintable, fixed_size)) = self.front.borrow().as_ref() {
                            snapshot.push_opacity((1.0 - t) as f64);
                            draw_content(snapshot, paintable, *fixed_size, w, h);
                            snapshot.pop();
                        }
                    }
                    if t > 0.0 {
                        if let Some((paintable, fixed_size)) = self.back.borrow().as_ref() {
                            snapshot.push_opacity(t as f64);
                            draw_content(snapshot, paintable, *fixed_size, w, h);
                            snapshot.pop();
                        }
                    }
                    // Fade has no rotation to follow — a plain, un-transformed
                    // draw is already correct, same as before this widget had
                    // a Flip mode at all. front, falling back to back, same
                    // "they're the same size in practice" reasoning as
                    // draw_theme_frame()'s own doc comment.
                    let frame_content = self.front.borrow().clone().or_else(|| self.back.borrow().clone());
                    if let Some((paintable, fixed_size)) = frame_content.as_ref() {
                        self.draw_theme_frame(snapshot, paintable, *fixed_size, w, h);
                    }
                }
            }
        }
    }

    /// The exact rect `paintable` draws into within `box_w × box_h`:
    /// "contain"-fit (preserve aspect, centred) if `fixed_size` is `None`,
    /// or a fixed centred square otherwise. Shared by `draw_content()` (the
    /// actual paint) and `draw_theme_frame()` (which needs this same rect,
    /// not `box_w × box_h` itself — see that method's doc comment for why
    /// framing the widget's own box was the original bug).
    fn content_rect(paintable: &gdk::Paintable, fixed_size: Option<f32>, box_w: f32, box_h: f32) -> graphene::Rect {
        let (w, h) = match fixed_size {
            Some(sz) => (sz, sz),
            None => {
                let iw = paintable.intrinsic_width() as f32;
                let ih = paintable.intrinsic_height() as f32;
                if iw > 0.0 && ih > 0.0 {
                    let scale = (box_w / iw).min(box_h / ih);
                    (iw * scale, ih * scale)
                } else {
                    (box_w, box_h)
                }
            }
        };
        graphene::Rect::new((box_w - w) / 2.0, (box_h - h) / 2.0, w, h)
    }

    /// Draw `paintable` into `box_w × box_h` at its `content_rect()`.
    fn draw_content(
        snapshot: &gtk::Snapshot,
        paintable: &gdk::Paintable,
        fixed_size: Option<f32>,
        box_w: f32,
        box_h: f32,
    ) {
        let rect = content_rect(paintable, fixed_size, box_w, box_h);
        snapshot.save();
        snapshot.translate(&graphene::Point::new(rect.x(), rect.y()));
        paintable.snapshot(snapshot, rect.width() as f64, rect.height() as f64);
        snapshot.restore();
    }

    impl FlipCover {
        pub(super) fn set_content(&self, content: Option<Content>, is_art: bool, key: &str) {
            if *self.last_key.borrow() == key { return; }
            self.last_key.replace(key.to_owned());

            let prev_is_art = self.front.borrow().as_ref().is_some_and(|(_, sz)| sz.is_none());
            let have_prev   = self.front.borrow().is_some();
            // Only flip between two real-art states; anything touching the
            // icon (in either direction) fades instead.
            let use_flip = is_art && prev_is_art && have_prev && content.is_some();
            let animate  = have_prev && content.is_some()
                && crate::config::with(|cfg| cfg.animations)
                && gtk::Settings::default().is_some_and(|s| s.is_gtk_enable_animations());

            // A transition already running (rapid changes): finish it
            // instantly first so we always start fresh from a clean state.
            // (See the comment on dispose() above re: why this must be two
            // statements, not `if let Some(a) = ...borrow_mut().take() { }`.)
            let old_anim = self.anim.borrow_mut().take();
            if let Some(a) = old_anim {
                a.skip();
            }

            if !animate {
                self.front.replace(content);
                self.back.replace(None);
                self.progress.set(0.0);
                self.obj().queue_draw();
                return;
            }

            self.back.replace(content);
            self.mode.set(if use_flip { Transition::Flip } else { Transition::Fade });
            let (duration, easing) = if use_flip {
                (FLIP_DURATION_MS, adw::Easing::EaseInOutCubic)
            } else {
                (FADE_DURATION_MS, adw::Easing::EaseInOutQuad)
            };

            let obj = self.obj();
            let target = adw::CallbackAnimationTarget::new(glib::clone!(
                #[weak] obj, move |v| {
                    obj.imp().progress.set(v as f32);
                    obj.queue_draw();
                }
            ));
            let anim = adw::TimedAnimation::new(&*obj, 0.0, 1.0, duration, target);
            anim.set_easing(easing);
            anim.connect_done(glib::clone!(
                #[weak] obj, move |_| {
                    let imp = obj.imp();
                    let new_front = imp.back.borrow_mut().take();
                    imp.front.replace(new_front);
                    imp.progress.set(0.0);
                    imp.anim.replace(None);
                    obj.queue_draw();
                }
            ));
            anim.play();
            self.anim.replace(Some(anim));
        }

        pub(super) fn clear(&self) {
            let old_anim = self.anim.borrow_mut().take();
            if let Some(a) = old_anim { a.skip(); }
            self.front.replace(None);
            self.back.replace(None);
            self.progress.set(0.0);
            self.last_key.replace(String::new());
            self.obj().queue_draw();
        }

        pub(super) fn set_frame_enabled(&self, enabled: bool) {
            self.frame_enabled.set(enabled);
            self.obj().queue_draw();
        }

        /// Frame drawn snug to the *content* rect (`content_rect()`), not the
        /// widget's own — possibly much wider — allocated box: this is what
        /// actually fixes the original bug (a CSS `box-shadow` on an overlay
        /// the size of the whole widget framed empty letterbox space, not
        /// the square artwork sitting inside it). Skipped entirely unless
        /// both this instance opted in (`frame_enabled`, see its own doc
        /// comment) and the active theme actually defines the three named
        /// colors below via `@define-color` — undefined (every non-Wood
        /// theme today) means `lookup_color()` returns `None` and nothing
        /// is drawn, the same "inert unless a theme opts in" convention
        /// every CSS-only Wood rule already follows, just expressed through
        /// a color lookup instead of a selector match, since this is Rust-
        /// drawn rather than CSS-drawn.
        ///
        /// Uses `front` (falling back to `back`) regardless of which side
        /// is actually mid-transition, and skips the fallback-icon case
        /// (`fixed_size.is_some()`) entirely — a physical picture-frame
        /// bevel around a generic source icon doesn't make sense the way it
        /// does around real album art. Not applied inside the 3D flip
        /// transform either: the frame stays flat/static during a Flip
        /// transition instead of rotating with the card — a deliberate
        /// simplification, not an oversight.
        /// `paintable`/`fixed_size` identify whichever content the caller
        /// is *actually drawing right now* (not necessarily `front` — see
        /// each call site) so this always frames exactly what's on screen.
        /// Callers in `snapshot()`'s `Transition::Flip` branch call this
        /// from *inside* their own `save()`/`transform()`/`restore()`
        /// block, around the same `draw_content()` call, specifically so
        /// the frame inherits the identical 3D perspective/rotation (and
        /// mirror correction) the card itself gets — drawing it afterward,
        /// outside that block, was the original version, and it looked
        /// static/pasted-on rather than part of the card as it turned.
        fn draw_theme_frame(&self, snapshot: &gtk::Snapshot, paintable: &gdk::Paintable, fixed_size: Option<f32>, w: f32, h: f32) {
            if !self.frame_enabled.get() { return; }
            if fixed_size.is_some() { return; }

            // Colors come from the active theme's tunables (theme.yaml),
            // not GTK CSS's `@define-color` + `StyleContext::lookup_color()`
            // — that mechanism is deprecated since GTK 4.10 (this crate
            // targets 4.12) and would be a build warning. See
            // `ThemeTunables::frame_highlight`'s doc comment for the fuller
            // reasoning. `current_tunables()` is a cheap cached clone, not a
            // fresh YAML parse — safe to call every frame.
            let tunables = crate::ui::current_tunables();
            let Some(highlight) = tunables.frame_highlight.as_deref().and_then(|s| gdk::RGBA::parse(s).ok()) else { return };
            let Some(shadow)    = tunables.frame_shadow.as_deref().and_then(|s| gdk::RGBA::parse(s).ok())    else { return };
            let Some(glow)      = tunables.frame_glow.as_deref().and_then(|s| gdk::RGBA::parse(s).ok())      else { return };

            let rect = content_rect(paintable, fixed_size, w, h);
            let outline = gsk::RoundedRect::from_rect(rect, 0.0);
            // Inset highlight (light catching the top/left raised edge),
            // inset shadow (bottom/right), outer glow (lifts the whole
            // frame off whatever's behind it).
            //
            // History, for reverting: shipped first at a plain hard-edged
            // 1px band (offset 1.0/1.0 and -1.0/-1.0, 0 blur). Ben asked to
            // try it bigger ("~doubled the offset, be ready to revert") —
            // that made a real, pre-existing problem much more visible
            // rather than just "too heavy": an inset shadow composites
            // *over* the artwork itself, so a flat, hard-edged, fairly
            // opaque band reads as an ugly smudge wherever the album art is
            // already light near that edge — thin (1px) it was easy to
              // miss, thick (2.5px) it wasn't. A true gradient-colored edge
            // (a stroked-path + linear/conic gradient overlay, GSK supports
            // it via push_stroke()/append_conic_gradient()) would need real
            // visual tuning this sandbox can't do — used *blur* on the
            // existing inset shadows instead, which is a much simpler
            // change and addresses both complaints at once: a blurred inset
            // shadow falls off smoothly from the edge rather than a hard
            // band, which both reads as "gradient-like" and is far gentler
            // wherever it lands over light artwork, since there's no hard
            // edge for a brightness mismatch to be obvious against.
            snapshot.append_inset_shadow(&outline, &highlight, 1.0, 1.0, 0.0, 4.0);
            snapshot.append_inset_shadow(&outline, &shadow, -1.0, -1.0, 0.0, 4.0);
            snapshot.append_outset_shadow(&outline, &glow, 0.0, 6.0, 0.0, 14.0);
        }
    }
}

// ── Public wrapper ─────────────────────────────────────────────────────────────

use gtk::glib;
use gtk::prelude::Cast;
use gtk::subclass::prelude::*;

glib::wrapper! {
    pub struct FlipCover(ObjectSubclass<imp::FlipCover>)
        @extends gtk::Widget;
}

impl FlipCover {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// Show real album art `tex` (or clear to nothing if `None`), flipping
    /// in from whatever was last shown if that was also real art,
    /// otherwise fading. `key` identifies *which* artwork this is (e.g. the
    /// art URL) — repeated calls with the same key are a no-op.
    pub fn set_art(&self, tex: Option<&gtk::gdk::Texture>, key: &str) {
        if crate::ui::DEBUG_UI.load(std::sync::atomic::Ordering::Relaxed) {
            println!("{} [ui] FlipCover::set_art key={key:?} some={}", crate::timestamp(), tex.is_some());
        }
        self.imp().set_content(
            tex.map(|t| (t.clone().upcast::<gtk::gdk::Paintable>(), None)),
            true,
            key,
        );
    }

    /// Show a fallback icon (source/input icon when no art is available) at
    /// a fixed `size_px` square, centred. Always fades, never flips. `key`
    /// should change whenever the icon itself changes (e.g. include the
    /// source id) so switching sources fades between icons too.
    pub fn set_icon(&self, icon: &gtk::gdk::Paintable, size_px: f32, key: &str) {
        if crate::ui::DEBUG_UI.load(std::sync::atomic::Ordering::Relaxed) {
            println!("{} [ui] FlipCover::set_icon key={key:?}", crate::timestamp());
        }
        self.imp().set_content(Some((icon.clone(), Some(size_px))), false, key);
    }

    /// Hard reset to empty, bypassing the de-dupe key and any in-flight
    /// transition — for device disconnect/reset, not a content change.
    pub fn clear(&self) {
        if crate::ui::DEBUG_UI.load(std::sync::atomic::Ordering::Relaxed) {
            println!("{} [ui] FlipCover::clear", crate::timestamp());
        }
        self.imp().clear();
    }

    /// Opt in to a theme-drawn raised-edge frame around the artwork (see
    /// `imp::FlipCover::draw_theme_frame()`) — off by default. The main and
    /// mini windows' "hero" artwork call this with `true`; `devlist.rs`'s
    /// small rounded thumbnails leave it off. Whether anything actually
    /// gets drawn additionally depends on the active theme defining the
    /// frame's named colors at all (inert under every theme but Wood today).
    pub fn set_frame_enabled(&self, enabled: bool) {
        self.imp().set_frame_enabled(enabled);
    }
}

impl Default for FlipCover {
    fn default() -> Self { Self::new() }
}
