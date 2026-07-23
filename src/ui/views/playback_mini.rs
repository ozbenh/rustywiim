//! # MiniPlaybackView
//!
//! The mini playback display: FlipCover artwork, title, artist·album
//! line, status line, "Restart Pairing", prev/play/next transport, and
//! an embedded `VolumeControl`. Previously the content half of
//! `widgets.rs`'s `build_mini_window()` plus the window-driven
//! `update_mini_playback()` / the mini slice of `reset_device_ui()` /
//! the mini branch of `update_artwork()`.
//!
//! Deliberately **not** part of this view (host chrome instead): the
//! top bar (device-name label, restore/close/menu buttons — "restore to
//! full window" presupposes a two-panel window pair this view knows
//! nothing about), the resize handle, the `WindowHandle` root, and the
//! blurred `ArtBackground`. The last is visually the *chrome's*
//! background, but it's driven by the same artwork data this view
//! already receives — so the host hands in an optional reference at
//! construction and the view feeds it, rather than the host needing its
//! own subscription.
//! rather: the view updates it alongside its own FlipCover; a host with
//! no blur background passes `None`).

pub mod imp {
    use std::cell::{Cell, OnceCell, RefCell};

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use gtk::glib;

    use crate::device::state::DeviceState;
    use crate::ui::art_background::ArtBackground;
    use crate::ui::flip_cover::FlipCover;
    use crate::ui::icons::IconSet;
    use crate::ui::views::common::{QualityBadge, ServiceLabel, SwipeText};
    use crate::ui::views::volume::VolumeControl;

    #[derive(Default)]
    pub struct MiniPlaybackView {
        pub(super) ds:       OnceCell<DeviceState>,
        pub(super) icons:    OnceCell<std::rc::Rc<IconSet>>,
        pub(super) handlers: RefCell<Vec<glib::SignalHandlerId>>,
        pub(super) active:   Cell<bool>,
        /// Host-owned blurred background this view feeds artwork to, if
        /// the host has one at all.
        pub(super) art_bg:   OnceCell<Option<ArtBackground>>,
        pub(super) artwork:  OnceCell<FlipCover>,
        pub(super) title:    OnceCell<SwipeText>,
        pub(super) artist:   OnceCell<SwipeText>,
        pub(super) status:   OnceCell<gtk::Label>,
        pub(super) service:  OnceCell<ServiceLabel>,
        pub(super) quality:  OnceCell<QualityBadge>,
        pub(super) bt_pair:  OnceCell<gtk::Button>,
        pub(super) btn_prev: OnceCell<gtk::Button>,
        pub(super) btn_play: OnceCell<gtk::Button>,
        pub(super) btn_next: OnceCell<gtk::Button>,
        pub(super) volume:   OnceCell<VolumeControl>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MiniPlaybackView {
        const NAME: &'static str = "MiniPlaybackView";
        type Type = super::MiniPlaybackView;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for MiniPlaybackView {
        fn dispose(&self) {
            if let Some(ds) = self.ds.get() {
                for id in self.handlers.take() {
                    ds.disconnect(id);
                }
            }
        }
    }
    impl WidgetImpl for MiniPlaybackView {}
    impl BinImpl for MiniPlaybackView {}
}

use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use gtk::{Align, Box as GtkBox, Button, Label, Orientation};

use crate::device::capabilities;
use crate::device::playback::PlaybackStatus;
use crate::device::state::{playback_changed, ConnectionState, DeviceState};
use crate::ui::art_background::ArtBackground;
use crate::ui::flip_cover::FlipCover;
use crate::ui::icons::IconSet;
use super::common::{
    build_bt_pair_button, format_bt_status_line, is_unknown,
    QualityBadge, ServiceLabel, SwipeText,
};
use super::volume::VolumeControl;

glib::wrapper! {
    pub struct MiniPlaybackView(ObjectSubclass<imp::MiniPlaybackView>)
        @extends adw::Bin, gtk::Widget;
}

impl MiniPlaybackView {
    /// Build the mini playback display bound to `ds`. `art_bg` is the
    /// host's blurred background to feed artwork to (`None` for a host
    /// without one). Starts **inactive** — the owner's first
    /// `set_active(true)` performs the initial render.
    pub(crate) fn new(ds: &DeviceState, icons: &Rc<IconSet>, art_bg: Option<&ArtBackground>) -> Self {
        let obj: Self = glib::Object::new();
        obj.build(ds, icons, art_bg);
        obj
    }

    fn build(&self, ds: &DeviceState, icons: &Rc<IconSet>, art_bg: Option<&ArtBackground>) {
        let imp = self.imp();
        imp.ds.set(ds.clone()).unwrap();
        let _ = imp.icons.set(Rc::clone(icons));
        let _ = imp.art_bg.set(art_bg.cloned());

        let artwork = FlipCover::new();
        // Opt in to a theme-drawn raised-edge frame around the artwork
        // (inert unless the active theme defines it — see
        // FlipCover::set_frame_enabled()'s doc comment).
        artwork.set_frame_enabled(true);
        artwork.set_hexpand(false);
        artwork.set_vexpand(false);
        artwork.set_valign(Align::Center);
        artwork.add_css_class("mini-art");
        // Defensive clip to the widget's own box (e.g. in case the 3D flip's
        // perspective transform renders very slightly outside its bounds at
        // extreme angles) — no rounded corners here, so nothing to clip normally.
        artwork.set_overflow(gtk::Overflow::Hidden);

        // Mini mode never runs inside Kiosk (no "M" there) — plain 1.0 multiplier.
        let title  = SwipeText::new("—", "mini-title",  false, false, 1.0);
        let artist = SwipeText::new("",  "mini-artist", false, false, 1.0);
        let bt_pair = build_bt_pair_button("mini-bt-pair-btn", 11);
        // Left-aligned (not the shared helper's default Center) so it lines
        // up with the status label's own left edge (halign(Start) below).
        bt_pair.set_halign(Align::Start);
        bt_pair.set_valign(Align::Center);

        // Overlaid on the artist line rather than appended as its own row —
        // `blank_playback_baseline()`/`has_playable_content()` guarantee
        // title and artist are always blank exactly when this button is
        // visible (both conditions are the same "nothing playable" check),
        // so there's nothing real for it to cover. A real extra row would
        // grow/shrink the whole mini window's height every time the
        // button's visibility flips; `gtk::Overlay` doesn't affect the main
        // child's size request by default (`measure_overlay` defaults to
        // `false`), so stacking it here keeps the window a fixed height
        // regardless.
        let artist_overlay = gtk::Overlay::new();
        artist_overlay.set_child(Some(&artist.stack));
        artist_overlay.add_overlay(&bt_pair);

        // Icon-only normally (see `apply_mask()`'s `PC::OTHER` handling) —
        // service/quality get their own badges next to it instead of being
        // appended to this text, same split as the full/WideRight layouts.
        // Still shows the full Bluetooth connection text verbatim in that
        // one case (`format_bt_status_line()`), which can run long, hence
        // still keeping `ellipsize` here.
        let status = Label::builder()
            .label("").css_classes(["mini-status-label"])
            .halign(Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let service = ServiceLabel::new("mini-status-label");
        // Started at 14px, matching `build_bt_pair_button`'s own
        // icon-alongside-small-text precedent rather than the much bigger
        // default meant for the full/WideRight layouts' own larger text;
        // bumped 20%, then another 10%, then back down to 16 once the
        // brand marks switched to BrandIcon's true-aspect-ratio sizing
        // (wordmark icons rendered visibly bigger than before at the same
        // height), by request.
        service.set_icon_pixel_size(16);
        // Same icon-vs-text-pill badge as the full/WideRight layouts'
        // own `quality_badge` — shown as the Hi-Res Audio mark instead of
        // text when the current tier has one.
        let quality = QualityBadge::new("mini-status-label");
        // Kept at its own fixed size (not re-derived from `service`'s,
        // which shrank independently above) — see `QualityBadge::new()`'s
        // comment for why the quality badge reads smaller everywhere.
        quality.set_icon_pixel_size(15);
        // Extra gap from the service label/icon on top of `info_row`'s
        // own spacing — reads as a separate badge, not glued to it.
        quality.widget.set_margin_start(6);

        let btn_prev = Button::builder()
            .icon_name("media-skip-backward-symbolic")
            .css_classes(["mini-transport-btn", "flat"]).build();
        let btn_play = Button::builder()
            .icon_name("media-playback-start-symbolic")
            .css_classes(["mini-play-btn", "suggested-action"]).build();
        let btn_next = Button::builder()
            .icon_name("media-skip-forward-symbolic")
            .css_classes(["mini-transport-btn", "flat"]).build();

        let volume = VolumeControl::new(ds, true);

        let transport_center = GtkBox::builder()
            .orientation(Orientation::Horizontal).spacing(2).build();
        transport_center.append(&btn_prev);
        transport_center.append(&btn_play);
        transport_center.append(&btn_next);

        let vol_end = GtkBox::builder()
            .valign(Align::Center).build();
        vol_end.append(&volume);

        // Card wraps only the actual playback controls (prev/play/next +
        // volume) — the status/service/quality group sits outside it, in
        // `info_row` below, whose own `hexpand(true)` pushes this card to
        // the row's trailing edge (the only hexpand child in the transport
        // row, so it absorbs all the leftover width).
        let controls_card = GtkBox::builder()
            .orientation(Orientation::Horizontal).spacing(6)
            .css_classes(["mini-transport-card"])
            .build();
        controls_card.append(&transport_center);
        controls_card.append(&vol_end);

        // Status icon + service + quality badges, grouped tightly on the
        // left — see `status`'s own construction comment above for why
        // service/quality moved out to their own badges here too.
        let info_row = GtkBox::builder()
            .orientation(Orientation::Horizontal).spacing(6)
            .halign(Align::Start).hexpand(true)
            .build();
        info_row.append(&status);
        info_row.append(&service.widget);
        info_row.append(&quality.widget);

        let transport = GtkBox::builder()
            .orientation(Orientation::Horizontal).hexpand(true)
            .build();
        transport.append(&info_row);
        transport.append(&controls_card);

        let info_box = GtkBox::builder()
            .orientation(Orientation::Vertical).spacing(4)
            .valign(Align::Center).hexpand(true)
            .build();
        info_box.append(&title.stack);
        info_box.append(&artist_overlay);
        info_box.append(&transport);

        let main_row = GtkBox::builder()
            .orientation(Orientation::Horizontal).spacing(12)
            .margin_start(14).margin_end(14).margin_bottom(14)
            .build();
        // Explicit background fills the vertical centering gap that appears
        // above info_box (valign=Center, shorter than the art stack).
        // Without it the NGL renderer can leave stale GPU buffer pixels
        // there. Not reliably reproducible since ScrollFadeLabel's rewrite
        // to a single-pass GSK snapshot(), so it's off by default — hidden
        // behind config.mini_stale_pixel_workaround (no Settings UI) rather
        // than deleted outright, so it can be flipped back on by
        // hand-editing config.json if the glitch turns up again, without a
        // rebuild.
        if crate::config::with(|cfg| cfg.mini_stale_pixel_workaround) {
            main_row.add_css_class("mini-main-row");
        }
        main_row.append(&artwork);
        main_row.append(&info_box);

        self.set_child(Some(&main_row));

        // ── Transport actions (device-directed, wired internally) ────────
        btn_play.connect_clicked({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                if let Some(ds) = obj.imp().ds.get() { ds.do_play_pause(); }
            }
        });
        btn_prev.connect_clicked({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                if let Some(ds) = obj.imp().ds.get() { ds.do_prev(); }
            }
        });
        btn_next.connect_clicked({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                if let Some(ds) = obj.imp().ds.get() { ds.do_next(); }
            }
        });
        bt_pair.connect_clicked({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                if let Some(ds) = obj.imp().ds.get() { ds.bt_enter_pairing(); }
            }
        });

        imp.artwork.set(artwork).unwrap();
        let _ = imp.title.set(title);
        let _ = imp.artist.set(artist);
        imp.status.set(status).unwrap();
        imp.service.set(service).unwrap();
        imp.quality.set(quality).unwrap();
        imp.bt_pair.set(bt_pair).unwrap();
        imp.btn_prev.set(btn_prev).unwrap();
        imp.btn_play.set(btn_play).unwrap();
        imp.btn_next.set(btn_next).unwrap();
        imp.volume.set(volume).unwrap();

        // ── DeviceState subscriptions ─────────────────────────────────────
        let id = ds.connect_playback_changed({
            let weak = self.downgrade();
            move |_, mask| {
                let Some(obj) = weak.upgrade() else { return };
                if !obj.imp().active.get() { return; }
                obj.apply_mask(mask);
            }
        });
        imp.handlers.borrow_mut().push(id);

        // An input switch changes the status line and the source-icon
        // artwork fallback — same full content refresh the old
        // window-driven path did (`update_mini_playback(ALL)`).
        let id = ds.connect_input_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                if !obj.imp().active.get() { return; }
                obj.refresh();
            }
        });
        imp.handlers.borrow_mut().push(id);

        // Connect (render cached state) and disconnect (render the
        // offline/"Disconnected" state) both arrive as device-changed.
        let id = ds.connect_device_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                if !obj.imp().active.get() { return; }
                obj.refresh();
            }
        });
        imp.handlers.borrow_mut().push(id);
    }

    /// See the view lifecycle contract (`views/mod.rs`). Forwarded to the
    /// embedded `VolumeControl` too — its own activation re-sync is what
    /// catches up a volume that changed while this view was inactive.
    pub(crate) fn set_active(&self, active: bool) {
        self.imp().volume.get().unwrap().set_active(active);
        let was = self.imp().active.replace(active);
        if active && !was { self.refresh(); }
    }

    /// The transport buttons, for the host's keyboard-shortcut flash.
    pub(crate) fn transport_buttons(&self) -> (Button, Button, Button) {
        let imp = self.imp();
        (imp.btn_prev.get().unwrap().clone(),
         imp.btn_play.get().unwrap().clone(),
         imp.btn_next.get().unwrap().clone())
    }

    /// The embedded volume cluster, for the host's Up/Down keyboard shortcuts.
    pub(crate) fn volume(&self) -> VolumeControl {
        self.imp().volume.get().unwrap().clone()
    }

    /// Full render from the `DeviceState` cache — live or offline.
    fn refresh(&self) {
        let Some(ds) = self.imp().ds.get() else { return };
        if ds.device_info().is_some() {
            self.apply_mask(playback_changed::ALL);
        } else {
            self.render_offline();
        }
    }

    /// The offline/disconnected rendering — previously `reset_device_ui()`'s
    /// mini block. "Disconnected" only for the real steady states;
    /// `Connecting` stays blank (the host shows its own spinner for that).
    fn render_offline(&self) {
        let imp = self.imp();
        let state = imp.ds.get().map(|ds| ds.connection_state());
        let title = if matches!(state, Some(ConnectionState::Failed | ConnectionState::Disconnected)) {
            "Disconnected"
        } else {
            ""
        };
        imp.title.get().unwrap().set_text(title);
        imp.artist.get().unwrap().set_text("");
        imp.status.get().unwrap().set_visible(false);
        imp.service.get().unwrap().set(None, imp.icons.get().unwrap());
        imp.quality.get().unwrap().widget.set_visible(false);
        imp.artwork.get().unwrap().clear();
        if let Some(Some(bg)) = imp.art_bg.get() { bg.clear(); }
        imp.bt_pair.get().unwrap().set_visible(false);
        for btn in [imp.btn_prev.get(), imp.btn_play.get(), imp.btn_next.get()].into_iter().flatten() {
            btn.set_sensitive(false);
        }
    }

    /// Apply the changed-field groups `mask` flags — the live-update path
    /// (previously `update_mini_playback()`). Volume is absent here: the
    /// embedded `VolumeControl` has its own subscription.
    fn apply_mask(&self, mask: u32) {
        use playback_changed as PC;
        let imp = self.imp();
        let Some(ds) = imp.ds.get() else { return };
        if ds.device_info().is_none() { return; }

        let ps = ds.playback_state();

        if mask & PC::OTHER != 0 {
            imp.btn_play.get().unwrap().set_icon_name(
                if matches!(ps.status, PlaybackStatus::Playing) {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                });
            // The plain play/pause/stop glyph this used to show alongside
            // the service badge wasn't particularly useful (reported live,
            // 2026-07-21) — hidden entirely now, letting the service/
            // quality badges sit where it used to. Still shown (as real
            // text, not an icon) for Bluetooth's own connection status,
            // which has no other place to go in this layout.
            let is_bluetooth = ps.source_name.as_deref() == Some("Bluetooth");
            let status_label = imp.status.get().unwrap();
            status_label.set_visible(is_bluetooth);
            if is_bluetooth {
                status_label.set_label(&format_bt_status_line(ps.bt_connected, ps.bt_device_name.as_deref(), ps.bt_pairing));
            }
            // No service badge for a physical input — same reasoning as
            // the full/WideRight layouts (see their identical comment):
            // there's no app/stream behind it, and its name goes in the
            // title instead (below).
            let service_name = if ps.is_physical_input { None } else { ps.source_name.as_deref() };
            imp.service.get().unwrap().set(service_name, imp.icons.get().unwrap());
            imp.quality.get().unwrap().set(ps.codec_label.as_deref(), imp.icons.get().unwrap());
            // Hidden while already pairing (nothing to "restart") as well
            // as while connected.
            imp.bt_pair.get().unwrap().set_visible(is_bluetooth && !ps.bt_connected && !ps.bt_pairing);
            imp.btn_play.get().unwrap().set_sensitive(ps.caps.can_playpause);
            imp.btn_prev.get().unwrap().set_sensitive(ps.caps.can_previous);
            imp.btn_next.get().unwrap().set_sensitive(ps.caps.can_next);
        }

        if mask & (PC::TITLE | PC::ARTIST | PC::ALBUM | PC::OTHER) != 0 {
            // The artist line combines artist + album; recompute if either changed.
            if mask & (PC::ARTIST | PC::ALBUM) != 0 {
                let artist = if is_unknown(&ps.artist) { "" } else { ps.artist.as_ref() };
                let album  = if is_unknown(&ps.album)  { "" } else { ps.album.as_ref() };
                let artist_line = match (artist.is_empty(), album.is_empty()) {
                    (true,  true)  => String::new(),
                    (true,  false) => album.to_owned(),
                    (false, true)  => artist.to_owned(),
                    (false, false) => format!("{artist} \u{00b7} {album}"),
                };
                imp.artist.get().unwrap().set_text(&artist_line);
            }
            // Also re-evaluated on a bare `OTHER` (a mode/input switch with
            // no real title change) — see the full/WideRight layouts'
            // identical comment on why a physical input's title comes from
            // `source_name` instead of `ps.title`.
            if mask & (PC::TITLE | PC::OTHER) != 0 {
                let title_text = if ps.is_physical_input {
                    ps.source_name.as_deref().unwrap_or("")
                } else if is_unknown(&ps.title) {
                    ""
                } else {
                    &ps.title
                };
                imp.title.get().unwrap().set_text(title_text);
            }
        }

        if mask & PC::ARTWORK != 0 {
            self.update_artwork();
        }
    }

    /// Decode and display artwork, or fall back to the source icon —
    /// previously the mini branch of the window's `update_artwork()`.
    /// The host's blurred background (if any) is fed the same content,
    /// unconditionally: cheap (a texture clone + queue_draw(), the latter
    /// a no-op while invisible), and keeps it in sync so enabling the
    /// Modern theme shows current art immediately instead of waiting for
    /// the next poll.
    fn update_artwork(&self) {
        let imp = self.imp();
        let Some(ds) = imp.ds.get() else { return };
        if ds.device_info().is_none() { return; }
        let flip = imp.artwork.get().unwrap();
        let art_bg = imp.art_bg.get().unwrap();

        let ps = ds.playback_state();
        let tex = ps.artwork.as_ref().and_then(|bytes| {
            let gbytes = glib::Bytes::from(bytes.as_ref());
            gtk::gdk::Texture::from_bytes(&gbytes).ok()
        });

        if let Some(tex) = &tex {
            let art_key = ps.art_url.as_deref().unwrap_or("");
            if let Some(bg) = art_bg { bg.set_art(Some(tex), art_key); }
            flip.set_art(Some(tex), art_key);
        } else {
            let mode = ds.current_mode();
            let source_id = capabilities::mode_to_input_source(mode);
            let icon_key = match ds.capabilities() {
                Some(caps) => capabilities::icon_canon_for_input(source_id, caps.device_id),
                None       => source_id,
            };
            // Fixed key (not per-source) so switching between different
            // no-art sources doesn't re-trigger the background fade for a
            // gradient that looks the same either way.
            if let Some(bg) = art_bg { bg.set_art(None, "__no_art__"); }
            flip.set_icon(
                imp.icons.get().unwrap().source_paintable(icon_key), 36.0,
                &format!("icon:{icon_key}"));
        }
    }
}
