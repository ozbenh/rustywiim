//! # PlaybackView
//!
//! The full playback display: FlipCover artwork with vignette frame,
//! title/artist/album, status + quality lines, seek bar with pos/dur,
//! prev/play/next + shuffle/repeat transport, "Restart Pairing", and an
//! embedded `VolumeControl`. Previously `widgets.rs`'s
//! `PlaybackWidgets`/`build_playback_widgets()`/`build_right_pane()`
//! plus the window-driven `update_playback_ui()`, the full-panel slice
//! of `reset_device_ui()`, and `update_artwork()`.
//!
//! Like `MiniPlaybackView`, the host's blurred `ArtBackground` is handed
//! in at construction (`None` for a host without one) and fed artwork
//! alongside the view's own FlipCover.
//!
//! Declares the `configure-eq` host-request signal (no emitter yet — the
//! EQ button lands with the equalizer feature): views ask their host to
//! open configuration surfaces via GObject signals rather than knowing
//! what the host is; device-directed actions go straight to the bound
//! `DeviceState`.

pub mod imp {
    use std::cell::{Cell, OnceCell, RefCell};
    use std::sync::OnceLock;

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use gtk::glib;
    use glib::subclass::Signal;

    use crate::device::state::DeviceState;
    use crate::ui::art_background::ArtBackground;
    use crate::ui::flip_cover::FlipCover;
    use crate::ui::icons::IconSet;
    use crate::ui::views::common::SwipeText;
    use crate::ui::views::volume::VolumeControl;

    #[derive(Default)]
    pub struct PlaybackView {
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
        pub(super) album:    OnceCell<SwipeText>,
        pub(super) status:   OnceCell<gtk::Label>,
        pub(super) quality:  OnceCell<gtk::Label>,
        pub(super) pos:      OnceCell<gtk::Label>,
        pub(super) dur:      OnceCell<gtk::Label>,
        pub(super) seek:     OnceCell<gtk::Scale>,
        pub(super) bt_pair:  OnceCell<gtk::Button>,
        pub(super) btn_prev: OnceCell<gtk::Button>,
        pub(super) btn_play: OnceCell<gtk::Button>,
        pub(super) btn_next: OnceCell<gtk::Button>,
        pub(super) shuffle:  OnceCell<gtk::Button>,
        pub(super) repeat:   OnceCell<gtk::Button>,
        pub(super) volume:   OnceCell<VolumeControl>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PlaybackView {
        const NAME: &'static str = "PlaybackView";
        type Type = super::PlaybackView;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for PlaybackView {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    // Host request: open equalizer configuration for this
                    // view's device. No emitter yet (the EQ button doesn't
                    // exist) — declared so hosts can wire it up as soon as
                    // one does.
                    Signal::builder("configure-eq").build(),
                ]
            })
        }

        fn dispose(&self) {
            if let Some(ds) = self.ds.get() {
                for id in self.handlers.take() {
                    ds.disconnect(id);
                }
            }
        }
    }
    impl WidgetImpl for PlaybackView {}
    impl BinImpl for PlaybackView {}
}

use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use gtk::{Align, Box as GtkBox, Button, Label, Orientation, Scale};

use crate::device::capabilities;
use crate::device::playback::PlaybackStatus;
use crate::device::state::{playback_changed, ConnectionState, DeviceState};
use crate::ui::art_background::ArtBackground;
use crate::ui::flip_cover::FlipCover;
use crate::ui::icons::IconSet;
use super::common::{
    apply_repeat_ui, apply_shuffle_ui, build_bt_pair_button, format_bt_status_line,
    format_quality_line, format_status_line, is_unknown, SwipeText,
};
use super::volume::VolumeControl;

glib::wrapper! {
    pub struct PlaybackView(ObjectSubclass<imp::PlaybackView>)
        @extends adw::Bin, gtk::Widget;
}

impl PlaybackView {
    /// Build the full playback display bound to `ds`. `art_bg` is the
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

        // hexpand+vexpand (default Fill alignment) so the widget always gets
        // the full art area to work with — it does its own aspect-preserving
        // "contain"/fixed-size centering internally (draw_content() in
        // flip_cover.rs), so unlike gtk::Picture it doesn't need a
        // content-derived natural size for halign(Center) to center against.
        // It also renders both real art AND the fallback icon itself
        // (crossfading between them), so no separate art_stack/input_icon.
        let artwork = FlipCover::new();
        artwork.set_hexpand(true);
        artwork.set_vexpand(true);

        // drop_shadow starts false regardless of theme — it's only wanted
        // for legibility against Modern's blurred background, and gets
        // toggled live by update_art_background_visibility() in ui/mod.rs
        // (called once more right after window construction, so this
        // initial value only matters for the instant before that runs).
        let title  = SwipeText::new("Not connected", "track-title",  true, false);
        let artist = SwipeText::new("",              "track-artist", true, false);
        let album  = SwipeText::new("",              "track-album",  true, false);
        let status = Label::builder().css_classes(["status-badge"]).halign(Align::Center).build();
        // Always visible (never `.set_visible(false)`) so its line-height is
        // permanently reserved in the layout — otherwise the artwork above it
        // resizes whenever quality info appears/disappears (e.g. no bitrate
        // data for the current source). Empty text still keeps its line
        // height in Pango's logical extents, same as the other labels here.
        let quality = Label::builder().css_classes(["quality-label"]).halign(Align::Center).build();
        let pos = Label::builder().label("0:00").css_classes(["dim-label"]).build();
        let dur = Label::builder().label("0:00").css_classes(["dim-label"]).build();
        let seek = Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 1.0);
        seek.set_hexpand(true);
        seek.set_draw_value(false);
        seek.add_css_class("seek-scale");
        seek.set_round_digits(0);

        let btn_prev = Button::builder()
            .icon_name("media-skip-backward-symbolic")
            .css_classes(["transport-btn", "circular", "flat"]).build();
        let btn_play = Button::builder()
            .icon_name("media-playback-start-symbolic")
            .css_classes(["play-btn", "circular", "suggested-action"]).build();
        let btn_next = Button::builder()
            .icon_name("media-skip-forward-symbolic")
            .css_classes(["transport-btn", "circular", "flat"]).build();
        let shuffle = Button::builder()
            .icon_name("media-playlist-shuffle-symbolic")
            .css_classes(["loop-btn", "circular", "flat"]).tooltip_text("Shuffle: Off").build();
        let repeat = Button::builder()
            .icon_name("media-playlist-repeat-symbolic")
            .css_classes(["loop-btn", "circular", "flat"]).tooltip_text("Repeat: Off").build();
        // Icon + text label, its own row below the status label (see the
        // assembly below), not inside the transport row — a text button
        // there previously widened the row enough to shift
        // btn_prev/btn_play/btn_next off-center whenever it appeared; its
        // own row can't affect that row's centering at all, regardless of
        // size.
        let bt_pair = build_bt_pair_button("bt-pair-btn", 14);

        let volume = VolumeControl::new(ds, false);

        // ── Assembly (previously build_right_pane()) ──────────────────────
        // Transport buttons are centred.
        let transport = GtkBox::builder()
            .orientation(Orientation::Horizontal).spacing(12).halign(Align::Center).build();
        transport.prepend(&shuffle);
        transport.append(&btn_prev);
        transport.append(&btn_play);
        transport.append(&btn_next);
        transport.append(&repeat);

        // Vol button sits at the right edge of the seek row, aligned with the bar's right end.
        let seek_row = GtkBox::builder().orientation(Orientation::Horizontal).spacing(8).build();
        seek_row.append(&pos);
        seek_row.append(&seek);
        seek_row.append(&dur);
        volume.set_margin_start(4);
        seek_row.append(&volume);

        // Overlay adds a radial vignette frame over the artwork that fades into the panel background.
        let art_overlay = gtk::Overlay::new();
        art_overlay.set_vexpand(true);
        art_overlay.set_child(Some(&artwork));
        let art_frame = GtkBox::builder()
            .hexpand(true).vexpand(true)
            .css_classes(["art-frame"])
            .can_target(false)
            .build();
        art_overlay.add_overlay(&art_frame);

        // Seek row + transport grouped into one card under RustyWiiM Modern
        // (see modern.css); inert everywhere else, same as "panel-card".
        // Artwork/title/artist/album/status/quality stay uncarded, floating
        // directly on the blurred background when that theme is active.
        let controls_card = GtkBox::builder()
            .orientation(Orientation::Vertical).spacing(8)
            .css_classes(["controls-card"])
            .build();
        controls_card.append(&seek_row);
        controls_card.append(&transport);

        let right_pane = GtkBox::builder()
            .orientation(Orientation::Vertical).spacing(8).hexpand(true)
            .margin_top(8).margin_bottom(8).margin_start(12).margin_end(16)
            .build();
        right_pane.append(&art_overlay);
        right_pane.append(&title.stack);
        right_pane.append(&artist.stack);
        right_pane.append(&album.stack);
        right_pane.append(&status);
        // Sits below the status label rather than in the transport row (see
        // `bt_pair`'s own comment) — invisible by default, `GtkBox` doesn't
        // reserve space for a hidden child either way.
        right_pane.append(&bt_pair);
        right_pane.append(&quality);
        right_pane.append(&controls_card);

        self.set_child(Some(&right_pane));

        // ── Actions (device-directed, wired internally) ───────────────────
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
        shuffle.connect_clicked({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                let Some(ds) = obj.imp().ds.get() else { return };
                let ps = ds.playback_state();
                ds.do_set_loop_mode(!ps.shuffle, ps.repeat);
            }
        });
        repeat.connect_clicked({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                let Some(ds) = obj.imp().ds.get() else { return };
                let ps = ds.playback_state();
                ds.do_set_loop_mode(ps.shuffle, ps.repeat.next());
            }
        });
        seek.connect_change_value({
            let weak = self.downgrade();
            move |_, _, value| {
                if let Some(obj) = weak.upgrade() {
                    if let Some(ds) = obj.imp().ds.get() {
                        ds.do_seek(value as u32);
                    }
                }
                glib::Propagation::Proceed
            }
        });

        imp.artwork.set(artwork).unwrap();
        let _ = imp.title.set(title);
        let _ = imp.artist.set(artist);
        let _ = imp.album.set(album);
        imp.status.set(status).unwrap();
        imp.quality.set(quality).unwrap();
        imp.pos.set(pos).unwrap();
        imp.dur.set(dur).unwrap();
        imp.seek.set(seek).unwrap();
        imp.bt_pair.set(bt_pair).unwrap();
        imp.btn_prev.set(btn_prev).unwrap();
        imp.btn_play.set(btn_play).unwrap();
        imp.btn_next.set(btn_next).unwrap();
        imp.shuffle.set(shuffle).unwrap();
        imp.repeat.set(repeat).unwrap();
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

        // An input switch changes the source-icon artwork fallback (the
        // status line follows via playback-changed's OTHER bit, as before).
        let id = ds.connect_input_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                if !obj.imp().active.get() { return; }
                obj.update_artwork();
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
    /// full-panel block. "Disconnected" only for the real steady states;
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
        imp.album.get().unwrap().set_text("");
        imp.status.get().unwrap().set_label("");
        imp.quality.get().unwrap().set_label("");
        imp.artwork.get().unwrap().clear();
        if let Some(Some(bg)) = imp.art_bg.get() { bg.clear(); }
        imp.bt_pair.get().unwrap().set_visible(false);
        for btn in [
            imp.btn_play.get(), imp.btn_prev.get(), imp.btn_next.get(),
            imp.shuffle.get(), imp.repeat.get(),
        ].into_iter().flatten() {
            btn.set_sensitive(false);
        }
        let seek = imp.seek.get().unwrap();
        seek.set_sensitive(false);
        seek.set_value(0.0);
        imp.pos.get().unwrap().set_visible(false);
        imp.dur.get().unwrap().set_visible(false);
        // Volume needs nothing here — VolumeControl renders its own
        // offline state (disabled, level 0) from its own subscription.
    }

    /// Apply the changed-field groups `mask` flags — the live-update path
    /// (previously `update_playback_ui()`). Volume is absent here: the
    /// embedded `VolumeControl` has its own subscription.
    fn apply_mask(&self, mask: u32) {
        use playback_changed as PC;
        let imp = self.imp();
        let Some(ds) = imp.ds.get() else { return };
        if ds.device_info().is_none() { return; }

        let ps = ds.playback_state();

        if mask & (PC::TIME | PC::OTHER) != 0 {
            // Position/duration are only valid while `Playing` or `Paused`
            // (`device/state.rs` deliberately clears them to zero and won't
            // trust a poll's reading of either otherwise, including an
            // unrecognized `Unknown(_)` status) — the seek bar needs to
            // agree, not just show "0:00" while still letting the (stale,
            // previous track's) range/interactivity linger.
            let seekable = ps.caps.can_seek && matches!(ps.status, PlaybackStatus::Playing | PlaybackStatus::Paused);
            if mask & PC::TIME != 0 {
                let cur_s = ps.position.as_secs();
                let tot_s = ps.duration.as_secs();
                let seek = imp.seek.get().unwrap();
                // Reset the range too, not just the value — leaving a stale
                // (previous track's) upper bound while duration reads 0
                // meant the thumb's on-screen position was still relative
                // to the old range on the next `set_value()`, not visually
                // "at zero" the way `pos`/`dur`'s labels already were.
                seek.set_range(0.0, if tot_s > 0 { tot_s as f64 } else { 1.0 });
                // Keep the fill empty while seeking isn't possible, rather
                // than showing a real (but non-interactive/misleading)
                // position — the position ticking away on a source with no
                // real seek concept (radio, a physical input) reads as
                // "there's a track here to scrub through," which isn't true.
                seek.set_value(if seekable { cur_s as f64 } else { 0.0 });
                imp.pos.get().unwrap().set_label(&format!("{}:{:02}", cur_s / 60, cur_s % 60));
                imp.dur.get().unwrap().set_label(&format!("{}:{:02}", tot_s / 60, tot_s % 60));
                // Duration is meaningless when unknown (0), regardless of
                // whether seeking itself is possible — hide it rather than
                // show a "0:00" that looks like a real (if zero-length) total.
                imp.dur.get().unwrap().set_visible(tot_s > 0);
                // Position stays visible whenever it's actually nonzero
                // (still useful to know "how far in" even on a source with
                // no seek concept, e.g. a live stream) — only hidden when
                // seek is unavailable *and* there's nothing to show anyway.
                imp.pos.get().unwrap().set_visible(seekable || cur_s > 0);
            }
            if mask & PC::OTHER != 0 {
                let playing = matches!(ps.status, PlaybackStatus::Playing);
                imp.btn_play.get().unwrap().set_icon_name(if playing {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                });
                let is_bluetooth = ps.source_name.as_deref() == Some("Bluetooth");
                imp.status.get().unwrap().set_label(&if is_bluetooth {
                    format_bt_status_line(ps.bt_connected, ps.bt_device_name.as_deref(), ps.bt_pairing)
                } else {
                    format_status_line(&ps.status, ps.source_name.as_deref())
                });
                // Hidden while already pairing (nothing to "restart") as
                // well as while connected.
                imp.bt_pair.get().unwrap().set_visible(is_bluetooth && !ps.bt_connected && !ps.bt_pairing);
                apply_shuffle_ui(imp.shuffle.get().unwrap(), ps.shuffle);
                apply_repeat_ui(imp.repeat.get().unwrap(), ps.repeat);
                imp.btn_play.get().unwrap().set_sensitive(ps.caps.can_playpause);
                imp.btn_prev.get().unwrap().set_sensitive(ps.caps.can_previous);
                imp.btn_next.get().unwrap().set_sensitive(ps.caps.can_next);
                imp.shuffle.get().unwrap().set_sensitive(ps.caps.can_shuffle);
                imp.repeat.get().unwrap().set_sensitive(ps.caps.can_repeat);
                let seek = imp.seek.get().unwrap();
                seek.set_sensitive(seekable);
                if !seekable {
                    seek.set_value(0.0);
                }
                imp.dur.get().unwrap().set_visible(ps.duration.as_secs() > 0);
                imp.pos.get().unwrap().set_visible(seekable || ps.position.as_secs() > 0);
            }
        }

        if mask & (PC::TITLE | PC::ARTIST | PC::ALBUM | PC::OTHER) != 0 {
            if mask & PC::TITLE != 0 {
                imp.title.get().unwrap().set_text(if is_unknown(&ps.title) { "" } else { &ps.title });
            }
            if mask & PC::ARTIST != 0 {
                imp.artist.get().unwrap().set_text(if is_unknown(&ps.artist) { "" } else { &ps.artist });
            }
            if mask & PC::ALBUM != 0 {
                imp.album.get().unwrap().set_text(if is_unknown(&ps.album) { "" } else { &ps.album });
            }
            if mask & PC::OTHER != 0 {
                // Never hidden — see the quality label's construction
                // comment. An empty label keeps the same reserved height.
                let q = ps.quality.map(|q| format_quality_line(&q, ps.codec_label.as_deref())).unwrap_or_default();
                imp.quality.get().unwrap().set_label(&q);
            }
        }

        if mask & PC::ARTWORK != 0 {
            self.update_artwork();
        }
    }

    /// Decode and display artwork, or fall back to the source icon —
    /// previously the full-panel branch of the window's `update_artwork()`.
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
                imp.icons.get().unwrap().source_paintable(icon_key), 128.0,
                &format!("icon:{icon_key}"));
        }
    }
}
