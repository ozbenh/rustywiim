//! # ArtBackground
//!
//! A GTK4 widget that fills its allocated area with a blurred, darkened
//! "cover"-fit rendering of the current album art — the ambient full-window
//! wash used by the RustyWiiM Modern theme. Falls back to a static gradient
//! when there's no artwork, and cross-fades between whatever it was
//! previously showing and the new state on every change. Originally speced
//! for a fullscreen "Now Playing" view that was never built; reused here as
//! a main-window theme background instead.
//!
//! GTK4 CSS has no `filter: blur()`, so — same as `FlipCover` and
//! `ScrollFadeLabel` — this is a custom `snapshot()` using a GSK render node
//! (`push_blur`) rather than anything CSS-driven.

pub mod imp {
    use std::cell::{Cell, RefCell};

    use adw::prelude::*;
    use gtk::glib;
    use gtk::subclass::prelude::*;
    use gtk::{gdk, graphene, gsk};

    const FADE_DURATION_MS: u32 = 500;

    pub struct ArtBackground {
        front:    RefCell<Option<gdk::Texture>>, // currently shown; None = gradient
        back:     RefCell<Option<gdk::Texture>>, // incoming, during a fade
        progress: Cell<f32>,                     // 0..1
        last_key: RefCell<String>,
        anim:     RefCell<Option<adw::TimedAnimation>>,
    }

    impl Default for ArtBackground {
        fn default() -> Self {
            Self {
                front:    RefCell::new(None),
                back:     RefCell::new(None),
                progress: Cell::new(0.0),
                last_key: RefCell::new(String::new()),
                anim:     RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ArtBackground {
        const NAME: &'static str = "ArtBackground";
        type Type = super::ArtBackground;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ArtBackground {
        fn dispose(&self) {
            let old_anim = self.anim.borrow_mut().take();
            if let Some(a) = old_anim { a.skip(); }
        }
    }

    impl WidgetImpl for ArtBackground {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            // No intrinsic size — this is a full-bleed background layer,
            // sized by whatever allocates it (an Overlay's base child fills
            // the Overlay's own allocation regardless of its own request).
            let _ = orientation;
            (0, 0, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let w = self.obj().width()  as f32;
            let h = self.obj().height() as f32;
            if w <= 0.0 || h <= 0.0 { return; }
            let bounds = graphene::Rect::new(0.0, 0.0, w, h);
            let t = self.progress.get().clamp(0.0, 1.0);

            snapshot.push_clip(&bounds);
            if t < 1.0 {
                snapshot.push_opacity((1.0 - t) as f64);
                draw_content(snapshot, self.front.borrow().as_ref(), w, h);
                snapshot.pop();
            }
            if t > 0.0 {
                snapshot.push_opacity(t as f64);
                draw_content(snapshot, self.back.borrow().as_ref(), w, h);
                snapshot.pop();
            }
            snapshot.pop(); // end clip
        }
    }

    /// Draw one full "state" of the background: a blurred/darkened texture,
    /// or the no-art gradient if `tex` is `None`. Shared by both the front
    /// and back layers in `snapshot()`, each wrapped in its own opacity for
    /// the cross-fade.
    fn draw_content(snapshot: &gtk::Snapshot, tex: Option<&gdk::Texture>, w: f32, h: f32) {
        let bounds = graphene::Rect::new(0.0, 0.0, w, h);
        match tex {
            Some(tex) => {
                // Opaque backdrop first: some artwork (e.g. a station logo
                // PNG with a transparent background) has an alpha channel,
                // and this widget is the window's bottom-most background
                // layer — libadwaita windows use an alpha-capable surface
                // for CSD shadows/rounded corners, so any hole left in the
                // render tree here is a genuine hole through to the desktop
                // behind the window, not just a visual glitch. Painting a
                // solid fill underneath guarantees the composited result is
                // always fully opaque regardless of the source texture.
                snapshot.append_color(&gdk::RGBA::new(0.039, 0.039, 0.039, 1.0), &bounds);
                // Blur radius scales with window size for a consistent look
                // from a small window up to a large one. Floor is low enough
                // that the mini window (min(w,h) well under 200px) actually
                // gets a subtle blur instead of being clamped up to a radius
                // that's disproportionately large for its size.
                let radius = (w.min(h) * 0.06).clamp(4.0, 80.0);
                snapshot.push_blur(radius as f64);
                // "cover" fit, oversized so the blur's transparent
                // edge-sampling falls outside the clip (the GTK equivalent
                // of wiim-now-playing's transform: scale(1.25)).
                let dst = cover_rect(tex.width() as f32, tex.height() as f32, w, h, 1.15);
                snapshot.save();
                snapshot.translate(&graphene::Point::new(dst.x(), dst.y()));
                tex.snapshot(snapshot, dst.width() as f64, dst.height() as f64);
                snapshot.restore();
                snapshot.pop(); // apply blur
                // Darkening wash for text/control legibility, matching
                // brightness(.6)/opacity(.6) in the web original this look
                // is based on.
                snapshot.append_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.45), &bounds);
            }
            None => {
                // No-art fallback: a diagonal three-stop gradient, dark and
                // moody like the rest of the theme but with enough range and
                // a slight cool tint to actually read as a gradient rather
                // than flat grey (a first version used two very close grey
                // stops top-to-bottom and was indistinguishable from solid
                // colour). top-left corner to bottom-right corner.
                snapshot.append_linear_gradient(
                    &bounds,
                    &graphene::Point::new(0.0, 0.0),
                    &graphene::Point::new(w, h),
                    &[
                        gsk::ColorStop::new(0.0, gdk::RGBA::new(0.039, 0.039, 0.039, 1.0)), // #0a0a0a, matches window bg
                        gsk::ColorStop::new(0.55, gdk::RGBA::new(0.09,  0.11,  0.13,  1.0)), // #171c21, faint cool tint
                        gsk::ColorStop::new(1.0, gdk::RGBA::new(0.14,  0.17,  0.20,  1.0)),  // #232b33
                    ],
                );
            }
        }
    }

    /// "cover"-fit `tex_w × tex_h` to fill `box_w × box_h` (cropping
    /// overflow, preserving aspect), then scale up by `oversize` and
    /// re-centre — the opposite of `FlipCover`'s "contain" fit, since a
    /// background wash should fill the area, not letterbox within it.
    fn cover_rect(tex_w: f32, tex_h: f32, box_w: f32, box_h: f32, oversize: f32) -> graphene::Rect {
        if tex_w <= 0.0 || tex_h <= 0.0 || box_w <= 0.0 || box_h <= 0.0 {
            return graphene::Rect::new(0.0, 0.0, box_w, box_h);
        }
        let scale = (box_w / tex_w).max(box_h / tex_h) * oversize;
        let w = tex_w * scale;
        let h = tex_h * scale;
        graphene::Rect::new((box_w - w) / 2.0, (box_h - h) / 2.0, w, h)
    }

    impl ArtBackground {
        pub(super) fn set_art(&self, tex: Option<&gdk::Texture>, key: &str) {
            if *self.last_key.borrow() == key { return; }
            self.last_key.replace(key.to_owned());

            // Two statements, not `if let Some(a) = self.anim.borrow_mut().take() { a.skip(); }`:
            // the RefMut temporary from borrow_mut() stays alive for the whole
            // if-let block (Rust's temporary lifetime rule for if-let
            // scrutinees), so anim would still be borrowed while skip() runs
            // — and skip() synchronously fires connect_done, which borrows
            // anim again and panics. (Same bug as FlipCover's set_content.)
            let old_anim = self.anim.borrow_mut().take();
            if let Some(a) = old_anim { a.skip(); }

            let animate = crate::config::with(|cfg| cfg.animations)
                && gtk::Settings::default().is_some_and(|s| s.is_gtk_enable_animations());

            if !animate {
                self.front.replace(tex.cloned());
                self.back.replace(None);
                self.progress.set(0.0);
                self.obj().queue_draw();
                return;
            }

            self.back.replace(tex.cloned());

            let obj = self.obj();
            let target = adw::CallbackAnimationTarget::new(glib::clone!(
                #[weak] obj, move |v| {
                    obj.imp().progress.set(v as f32);
                    obj.queue_draw();
                }
            ));
            let anim = adw::TimedAnimation::new(&*obj, 0.0, 1.0, FADE_DURATION_MS, target);
            anim.set_easing(adw::Easing::EaseInOutCubic);
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
    }
}

// ── Public wrapper ─────────────────────────────────────────────────────────────

use gtk::glib;
use gtk::subclass::prelude::*;

glib::wrapper! {
    pub struct ArtBackground(ObjectSubclass<imp::ArtBackground>)
        @extends gtk::Widget;
}

impl ArtBackground {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// Cross-fade to `tex` (or the no-art gradient if `None`) from whatever
    /// was last shown. `key` identifies *which* state this is (e.g. the art
    /// URL, or a fixed marker for "no art") — repeated calls with the same
    /// key are a no-op, so callers that re-run on every poll tick don't
    /// re-trigger the fade.
    pub fn set_art(&self, tex: Option<&gtk::gdk::Texture>, key: &str) {
        self.imp().set_art(tex, key);
    }

    /// Hard reset to the no-art gradient, bypassing the de-dupe key and any
    /// in-flight fade — for device disconnect/reset, not a content change.
    pub fn clear(&self) {
        self.imp().clear();
    }
}

impl Default for ArtBackground {
    fn default() -> Self { Self::new() }
}
