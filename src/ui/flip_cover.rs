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

#![allow(deprecated)] // glib::clone! old-style @weak syntax, matches scroll_fade_label.rs

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
                }
            }
        }
    }

    /// Draw `paintable` into `box_w × box_h`: "contain"-fit (preserve aspect,
    /// centred) if `fixed_size` is `None`, or at a fixed centred square size
    /// otherwise.
    fn draw_content(
        snapshot: &gtk::Snapshot,
        paintable: &gdk::Paintable,
        fixed_size: Option<f32>,
        box_w: f32,
        box_h: f32,
    ) {
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
        let x = (box_w - w) / 2.0;
        let y = (box_h - h) / 2.0;
        snapshot.save();
        snapshot.translate(&graphene::Point::new(x, y));
        paintable.snapshot(snapshot, w as f64, h as f64);
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
                @weak obj => move |v| {
                    obj.imp().progress.set(v as f32);
                    obj.queue_draw();
                }
            ));
            let anim = adw::TimedAnimation::new(&*obj, 0.0, 1.0, duration, target);
            anim.set_easing(easing);
            anim.connect_done(glib::clone!(
                @weak obj => move |_| {
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
        self.imp().set_content(Some((icon.clone(), Some(size_px))), false, key);
    }

    /// Hard reset to empty, bypassing the de-dupe key and any in-flight
    /// transition — for device disconnect/reset, not a content change.
    pub fn clear(&self) {
        self.imp().clear();
    }
}

impl Default for FlipCover {
    fn default() -> Self { Self::new() }
}
