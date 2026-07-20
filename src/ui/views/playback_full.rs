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
    use crate::ui::views::common::{ServiceLabel, SwipeText};
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
        pub(super) service:  OnceCell<ServiceLabel>,
        /// The new `translate_quality_badge()`-driven badge, next to
        /// `service` — distinct from `quality` below, which keeps the
        /// existing bitrate/depth/sample-rate text.
        pub(super) quality_badge: OnceCell<gtk::Label>,
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

use crate::config;
use crate::device::capabilities;
use crate::device::playback::PlaybackStatus;
use crate::device::state::{playback_changed, ConnectionState, DeviceState};
use crate::ui::art_background::ArtBackground;
use crate::ui::flip_cover::FlipCover;
use crate::ui::icons::IconSet;
use super::common::{
    apply_repeat_ui, apply_shuffle_ui, build_bt_pair_button, format_bt_status_line,
    format_quality_line, format_status_only, is_unknown, ServiceLabel, SwipeText,
};
use super::volume::VolumeControl;

glib::wrapper! {
    pub struct PlaybackView(ObjectSubclass<imp::PlaybackView>)
        @extends adw::Bin, gtk::Widget;
}

/// Which container arrangement `PlaybackView` assembles its (otherwise
/// identical, already-built) widgets into — chosen once at construction,
/// not switchable at runtime yet.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaybackLayout {
    /// Today's existing arrangement: artwork full-width above title/
    /// artist/album/status/quality, then a seek row (with volume at its
    /// right end) and the transport row underneath.
    Classic,
    /// Artwork on the left (bigger), title/artist/album/status/quality on
    /// the right with the transport + volume row underneath them (no seek
    /// bar in that row) — the seek bar instead spans the full width below
    /// both. Used by Kiosk mode's fullscreen display, where the classic
    /// vertical arrangement wastes horizontal space.
    WideRight,
}

/// `WideRight`'s artwork is a square capped by two independent limits, so
/// it never grows so tall it eats the whole screen on a narrow-but-tall
/// layout, nor so wide it crowds out the text column on a wide-but-short
/// one. Plain percentages, not pre-divided fractions, so they're easy to
/// tune directly.
const WIDE_RIGHT_ART_MAX_HEIGHT_PCT: f64 = 60.0;
const WIDE_RIGHT_ART_MAX_WIDTH_PCT: f64 = 33.0;

/// The artwork's side length (it's always square) for a screen of
/// `screen_w` × `screen_h` pixels: the smaller of `WIDE_RIGHT_ART_MAX_HEIGHT_PCT`%
/// of the screen's height and `WIDE_RIGHT_ART_MAX_WIDTH_PCT`% of its width —
/// whichever axis is more constraining wins, so the artwork always fits
/// within *both* bounds at once. Computed from the real screen/window size
/// rather than negotiated from surrounding widgets (see this function's
/// call site for why: FlipCover has no intrinsic size, so nothing else in
/// this layout previously accounted for the artwork's *vertical* budget at
/// all, which on some screens left it — and the whole title/controls block
/// sharing its height — noticeably undersized).
pub(crate) fn compute_wide_right_art_side(screen_w: i32, screen_h: i32) -> i32 {
    let from_height = screen_h as f64 * WIDE_RIGHT_ART_MAX_HEIGHT_PCT / 100.0;
    let from_width  = screen_w as f64 * WIDE_RIGHT_ART_MAX_WIDTH_PCT / 100.0;
    let side = from_height.min(from_width).round() as i32;
    if crate::ui::DEBUG_UI.load(std::sync::atomic::Ordering::Relaxed) {
        let limiting = if from_height <= from_width { "height" } else { "width" };
        println!(
            "{} [ui] wide-right art size: screen={screen_w}x{screen_h} \
             from_height={from_height:.1} ({WIDE_RIGHT_ART_MAX_HEIGHT_PCT}% of height) \
             from_width={from_width:.1} ({WIDE_RIGHT_ART_MAX_WIDTH_PCT}% of width) \
             limiting={limiting} side={side}",
            crate::timestamp(),
        );
    }
    side
}

/// Left/right margin for the wide-right layout's content, as a fraction of
/// the artwork's own side length — exposed so `KioskWindow` can align
/// `StatusBarView`'s edges (a separate widget, not part of this layout's
/// own tree) to match, now that there's no separator line between them to
/// visually excuse a mismatch.
pub(crate) fn wide_right_margin_h(art_side: i32) -> i32 {
    (art_side as f64 * 0.12).round() as i32
}

/// Recompute and apply the `WideRight` layout's typography/control sizing
/// from `h` — the artwork's real pixel size (square, so its measured width
/// and height are equal; the caller passes width specifically, see the
/// call site's own comment for why) — writing scoped CSS (`class`, unique
/// per `PlaybackView` instance so different windows never clobber each
/// other's provider rules) into `provider` and setting the two group
/// spacings directly (plain widget properties, not something CSS
/// controls). All ratios below are fractions of `h`, chosen so
/// title+artist+album (with their gaps) come to roughly 2/3 of it —
/// matching text_col_grid's own 2:1 row split — and the controls band
/// (the other 1/3) gets a button size that scales the same way, rather
/// than the fixed pixel sizes `.transport-btn`/`.play-btn`/`.loop-btn`
/// normally use, which only look right on one specific screen size.
fn apply_wide_right_scale(
    class: &str, provider: &gtk::CssProvider,
    title_group: &GtkBox, status_group: &GtkBox, service_group: &GtkBox, volume: &VolumeControl,
    service: &ServiceLabel,
    h: i32,
) {
    let h = h as f64;
    // Two rounds of "20% smaller" on top of the original pass
    // (0.22/0.12/0.10/0.09/0.055/0.03), tuned against live testing on
    // both a 4K desktop and a Raspberry Pi touchscreen.
    // Title reduced a further ~15% (0.1408 -> 0.12) by request — still
    // clearly bigger than artist below.
    let title_px  = (h * 0.12).round() as i32;
    let artist_px = (h * 0.0768).round() as i32;
    let album_px  = (h * 0.064).round() as i32;
    // "Slightly smaller than the album name" per the design ask, then
    // reduced another ~20% (0.85 -> 0.68) — the whole badge read too big
    // in Kiosk mode. The icon's `pixel_size` (a widget property, not
    // CSS-reachable) is kept proportional to this text at the same 3:1
    // ratio Classic's fixed values use (36px icon : 12px `.service-name`
    // base font-size), then reduced a further 10% (3.0 -> 2.7) — the icon
    // specifically (not the text) still read too big.
    let service_px = (album_px as f64 * 0.68).round() as i32;
    service.set_icon_pixel_size((service_px as f64 * 2.7).round() as i32);
    let text_gap  = (h * 0.0576).round() as i32;
    title_group.set_spacing(text_gap);

    // Floor added — confirmed live: pure `h * 0.0352` (no floor, like
    // everything else in this function) reads right on a 4K screen but is
    // genuinely too small to read on a Raspberry Pi's smaller `h`. Reuses
    // 18px, the same "small but legible in Kiosk mode" baseline already
    // established by the old fixed `window.kiosk-window .ip-label`/
    // `.device-info` CSS values (dark.css/system.css) rather than
    // inventing a new number. Only kicks in below h≈511 — a real, if
    // deliberate, deviation from pure proportionality: legibility has a
    // hard floor human eyes need regardless of screen size, unlike pure
    // layout spacing.
    let status_px = (h * 0.0352).round().max(18.0) as i32;
    let status_gap = (h * 0.0192).round() as i32;
    status_group.set_spacing(status_gap.max(2));
    // Spacing between service/quality-badge/bitrate-string — was a fixed
    // 16px (plus a redundant matching margin on top of it, doubling to
    // 32px), not proportional like everything else here, so it looked
    // wrong at any screen size other than whatever it happened to be
    // eyeballed against. A fraction of `h` instead, same as `text_gap`/
    // `status_gap` above — not independently re-tuned against a real
    // screen, so the exact factor may still need adjusting once seen live.
    let service_gap = (h * 0.045).round() as i32;
    service_group.set_spacing(service_gap.max(4));

    // The controls band is the bottom 1/3 of the column; the visible card
    // itself only takes 2/3 of that (bottom-justified — see band_row's own
    // comment), minus ".controls-card"'s own padding on each side. That
    // padding used to be a *fixed* 12px (modern.css) subtracted as a flat
    // 24 here — the exact same class of bug `seek_h`/`service_gap` above
    // were fixed for, just not yet caught here: subtracting a constant
    // from a value that itself scales with screen size shrinks the small-
    // screen result disproportionately more than the large-screen one, so
    // the buttons (and the whole band around them) read comparatively
    // bigger on a small screen and smaller on a large one — confirmed
    // live as exactly the "text bigger there, bottom bar bigger there"
    // mismatch between a 4K monitor and a small Pi screen. Now a fraction
    // of `h` too, so it shrinks/grows together with everything else
    // instead of eating a growing/shrinking share of it.
    let card_padding = (h * 0.01).round().max(4.0);
    let card_h = ((h / 3.0) * (2.0 / 3.0) - 2.0 * card_padding).max(24.0);
    let transport_btn = (card_h * 0.55).round() as i32;
    let play_btn      = (card_h * 0.68).round() as i32;
    let loop_btn       = transport_btn;
    let transport_icon = (transport_btn as f64 * 0.45).round() as i32;
    let play_icon      = (play_btn as f64 * 0.45).round() as i32;
    // The volume button's own icon is a `pixel_size`-set child Image, not
    // an icon-name button `-gtk-icon-size` (below) could reach — confirmed
    // live, it otherwise stays fixed at its small default while the other
    // transport icons scale up around it, reading as noticeably tinier.
    volume.set_icon_pixel_size(transport_icon);

    // The seek bar's own trough thickness — `.seek-scale trough`'s base
    // rule (dark.css/system.css) hardcodes `min-height: 4px`, the one
    // piece of this layout that was never brought under this function's
    // scoped scaling at all (unlike everything else here) — same class of
    // bug `service_gap` above just got fixed for, just never touched
    // until now. A floor only, deliberately no ceiling — a hard cap would
    // break the linear relationship with `status_px` above (which has
    // none), so their *ratio* would stop being constant across screen
    // sizes, the exact "proportions don't match between screens" problem
    // this is meant to fix. Factor kept low (already-thin at typical
    // sizes) rather than matching `status_gap`-class values, since the
    // trough was reported as a bit too prominent even at the old flat
    // 4px. Not independently re-tuned against a real screen.
    let seek_h = (h * 0.006).round().max(2.0) as i32;

    // ".controls-card" only has any real padding/background under
    // RustyWiiM Modern (modern.css) — under System/Dark it's an inert
    // class name (dark.css/system.css never style it at all), so
    // injecting a padding override unconditionally would introduce
    // padding that never existed there before. Only override it where
    // there's an existing fixed value to correct in the first place.
    let card_padding_rule = if matches!(config::with(|cfg| cfg.theme), config::ThemeMode::RustyWiiMModern) {
        format!(".{class} .controls-card {{ padding: {}px; }}\n", card_padding.round() as i32)
    } else {
        String::new()
    };

    provider.load_from_string(&format!(
        ".{class} .track-title {{ font-size: {title_px}px; }}\n\
         .{class} .track-artist {{ font-size: {artist_px}px; }}\n\
         .{class} .track-album {{ font-size: {album_px}px; }}\n\
         .{class} .service-name {{ font-size: {service_px}px; }}\n\
         .{class} .quality-label {{ font-size: {status_px}px; }}\n\
         .{class} .dim-label {{ font-size: {status_px}px; }}\n\
         .{class} .vol-level {{ font-size: {status_px}px; }}\n\
         .{class} .seek-scale trough {{ min-height: {seek_h}px; border-radius: {half}px; }}\n\
         {card_padding_rule}\
         .{class} .transport-btn:not(.vol-mute-btn) {{ min-width: {transport_btn}px; min-height: {transport_btn}px; -gtk-icon-size: {transport_icon}px; }}\n\
         .{class} .loop-btn {{ min-width: {loop_btn}px; min-height: {loop_btn}px; -gtk-icon-size: {transport_icon}px; }}\n\
         .{class} .play-btn {{ min-width: {play_btn}px; min-height: {play_btn}px; -gtk-icon-size: {play_icon}px; }}\n\
         .{class} .vol-btn {{ min-height: {transport_btn}px; }}\n",
        half = seek_h / 2,
    ));
}

impl PlaybackView {
    /// Build the full playback display bound to `ds`. `art_bg` is the
    /// host's blurred background to feed artwork to (`None` for a host
    /// without one). `size_source` (`WideRight` only, ignored otherwise)
    /// returns the *available* (width, height) `WideRight` should size
    /// itself against, or `None` if not known yet — called once
    /// synchronously at build time (letting sizing apply correct values
    /// before this view is ever painted, avoiding a flash the first
    /// caller — see below — didn't need) and then every frame afterward
    /// via a tick callback, reapplying only when the result actually
    /// changes, so a resizable host stays correctly sized as it's resized
    /// rather than freezing at whatever it was when built.
    ///
    /// Deliberately a callback, not a plain `Option<(i32, i32)>` snapshot:
    /// the "available size" isn't always just the window's own — Kiosk
    /// mode's window IS the whole available area, but embedded in a
    /// `DeviceWindow`'s `Paned`, the space actually available to this view
    /// is the window's width *minus whatever the sidebar/divider
    /// currently take*, which changes independently of anything this view
    /// does. Reading the window's *whole* width there once caused a real
    /// bug: this view's own size_request (set from that too-large number)
    /// became large enough that the `Paned` itself refused to let the
    /// sidebar open past a small width, treating this view's artificially
    /// inflated minimum as a hard floor on how much the sidebar could take
    /// instead. Each host's own closure reports only what's genuinely
    /// available to *this view specifically*.
    pub(crate) fn new(
        ds: &DeviceState, icons: &Rc<IconSet>, art_bg: Option<&ArtBackground>, layout: PlaybackLayout,
        size_source: Rc<dyn Fn() -> Option<(i32, i32)>>,
    ) -> Self {
        let obj: Self = glib::Object::new();
        obj.build(ds, icons, art_bg, layout, size_source);
        obj
    }

    fn build(
        &self, ds: &DeviceState, icons: &Rc<IconSet>, art_bg: Option<&ArtBackground>, layout: PlaybackLayout,
        size_source: Rc<dyn Fn() -> Option<(i32, i32)>>,
    ) {
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
        // "dim-label", not "status-badge" — same grey as pos/dur (they
        // share this row now), not the accent/highlight color the old
        // class used, matching by request. `status-badge` is gone entirely
        // (was only ever used here).
        let status = Label::builder().css_classes(["dim-label"]).halign(Align::Center).build();
        let service = ServiceLabel::new("service-name");
        // Next to `service`, same rounded-rect badge as its own text
        // fallback (`ServiceLabel::new()`'s "service-name-pill") and same
        // font-size class, so the two read as one matched pair.
        let quality_badge = Label::builder()
            .css_classes(["service-name", "service-name-pill"]).visible(false)
            // Extra gap from the service label/icon on top of the
            // group box's own spacing — reads as a separate badge, not
            // glued to it.
            .margin_start(6)
            // Without this, the label's box defaults to `Fill` within its
            // parent row and stretches to match the row's own height (set
            // by its tallest sibling, e.g. the service icon) — the pill's
            // border/background then paints around that *stretched*
            // allocation, not the text's natural size, however tight the
            // CSS padding/min-height is. `Center` keeps its own box at
            // natural content size, with any extra row height as outside
            // margin instead of inside the border.
            .valign(Align::Center)
            .build();
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

        match layout {
            PlaybackLayout::Classic => {
                // Position/duration sit on their own row underneath the
                // scale (flush left/right) instead of beside it — a plain
                // Box, not a CenterBox, for the same reason WideRight's own
                // version isn't one (see that layout's identical comment):
                // CenterBox reserves symmetric space for both sides based
                // on the *larger* label, which both misaligns the shorter
                // one and can inflate the row's own natural width. Volume
                // moves to the transport row (matching WideRight) now that
                // it no longer shares the seek row with pos/dur — freed up,
                // the scale gets the row's full width instead.
                seek.set_hexpand(true);
                // `status` sits in the same row as pos/dur, centered
                // between them (same font size as both already — see
                // below) — a CenterBox rather than a plain Box, since this
                // row genuinely wants a true centered third element.
                let time_row = gtk::CenterBox::new();
                pos.set_halign(Align::Start);
                dur.set_halign(Align::End);
                time_row.set_start_widget(Some(&pos));
                time_row.set_center_widget(Some(&status));
                time_row.set_end_widget(Some(&dur));
                // spacing(2), same tight grouping as WideRight's seek+time
                // row — the scale and its labels read as one unit, not two
                // separately-spaced rows.
                let seek_block = GtkBox::builder().orientation(Orientation::Vertical).spacing(2).build();
                seek_block.append(&seek);
                seek_block.append(&time_row);

                // Experiment: volume pinned to the right edge of the card
                // instead of clustered with the rest of the transport
                // buttons — transport stays centred on its own (a
                // CenterBox's center widget), volume sits in the end slot.
                let controls_row = gtk::CenterBox::new();
                controls_row.set_center_widget(Some(&transport));
                controls_row.set_end_widget(Some(&volume));

                // Service name on the left edge of the controls row,
                // vertically aligned with the transport buttons — an
                // `Overlay`, not `controls_row`'s own `CenterBox` start
                // slot: a `GtkCenterBox` shrinks its start/end children to
                // keep the center widget truly centered, which starved
                // this of width once `transport`+`volume` already filled
                // most of the row (confirmed live — showed neither icon
                // nor text). An overlay child is sized off its own natural
                // size instead, floating in whatever room the row's left
                // edge actually has, so it can't be starved the same way.
                // Also tried: its own row in `right_pane` (piled up too
                // much text under the artwork, the opposite of the point);
                // the artwork's own top-left corner (sat over the image
                // itself, which read poorly against arbitrary art).
                service.widget.set_halign(Align::Start);
                service.widget.set_valign(Align::Center);
                let service_group = GtkBox::builder()
                    .orientation(Orientation::Horizontal).spacing(6)
                    .halign(Align::Start).valign(Align::Center)
                    .build();
                service_group.append(&service.widget);
                service_group.append(&quality_badge);
                let controls_overlay = gtk::Overlay::new();
                controls_overlay.set_child(Some(&controls_row));
                controls_overlay.add_overlay(&service_group);

                // Seek block + transport grouped into one card under
                // RustyWiiM Modern (see modern.css); inert everywhere else,
                // same as "panel-card". Artwork/title/artist/album/status/
                // quality stay uncarded, floating directly on the blurred
                // background when that theme is active. spacing(8),
                // unchanged from before this row was split in two — so
                // transport's own gap from the seek bar (now the bottom of
                // seek_block, not the scale row directly) doesn't grow
                // just because time_row was inserted above it.
                let controls_card = GtkBox::builder()
                    .orientation(Orientation::Vertical).spacing(8)
                    .css_classes(["controls-card"])
                    .build();
                controls_card.append(&seek_block);
                controls_card.append(&controls_overlay);

                let right_pane = GtkBox::builder()
                    .orientation(Orientation::Vertical).spacing(8).hexpand(true)
                    .margin_top(8).margin_bottom(8).margin_start(12).margin_end(16)
                    .build();
                right_pane.append(&art_overlay);
                right_pane.append(&title.stack);
                right_pane.append(&artist.stack);
                right_pane.append(&album.stack);
                // Sits below the album line rather than in the transport
                // row (see `bt_pair`'s own comment) — invisible by default,
                // `GtkBox` doesn't reserve space for a hidden child either way.
                right_pane.append(&bt_pair);
                right_pane.append(&quality);
                right_pane.append(&controls_card);

                self.set_child(Some(&right_pane));
            }
            PlaybackLayout::WideRight => {
                // Volume moved out of the controls row entirely, to the
                // outer right edge of the column (aligned with the seek
                // bar's own right edge — see the `top_row_overlay` comment
                // below for why that's structurally guaranteed, not a
                // coincidence), by request.
                // Same semi-transparent card styling the classic layout's
                // seek+transport group gets under RustyWiiM Modern (see
                // modern.css's ".controls-card" — inert under System/Dark).
                transport.add_css_class("controls-card");
                // Left-aligned like the rest of this column, not centered
                // (the shared construction above centers it for the
                // classic layout, where it sits under a centered artwork).
                transport.set_halign(Align::Start);

                // Left-aligned, not centered under the artwork — the
                // "wide-right-text" class (see dark.css/system.css) scales
                // up title/artist/album to read at a distance.
                title.set_center_when_fits(false);
                artist.set_center_when_fits(false);
                album.set_center_when_fits(false);
                // `status` stays centered (default from its construction
                // above) — it now lives under the seek row, same as
                // Classic, not in this left-justified column at all.
                bt_pair.set_halign(Align::Start);
                quality.set_halign(Align::Start);

                // Title/artist/album — pinned to the top of the column,
                // which (art_overlay and text_col are both explicitly
                // sized to the same height below) is also the artwork's
                // own top edge. Spacing is set dynamically below, once
                // the real screen size is known.
                let title_group = GtkBox::builder()
                    .orientation(Orientation::Vertical)
                    .valign(Align::Start)
                    .build();
                title_group.append(&title.stack);
                title_group.append(&artist.stack);
                title_group.append(&album.stack);

                // `bt_pair` beside the controls box rather than above it —
                // frees up the column for a bigger title block. `status`
                // no longer lives here (moved under the seek row, same as
                // Classic); the bitrate/depth/rate string (`quality`) moved
                // out too, to `service_group` below, right of the quality
                // badge.
                let status_group = GtkBox::builder()
                    .orientation(Orientation::Vertical)
                    .valign(Align::Center)
                    .build();
                status_group.append(&bt_pair);

                // Controls card first (left), status/bt_pair after it —
                // both pinned to the bottom of their band via the grid
                // below (valign(End) here just keeps them together as a
                // unit; the actual "sits at the very bottom" comes from
                // the band's own cell in text_col_grid).
                let band_row = GtkBox::builder()
                    .orientation(Orientation::Horizontal)
                    .valign(Align::End)
                    .build();
                band_row.append(&transport);
                band_row.append(&status_group);

                // Service name + quality badge + bitrate/depth/rate
                // string, directly above the controls band, left
                // justified like the rest of this column. Spacing is set
                // dynamically below (`apply_wide_right_scale()`'s own
                // `service_gap`), proportional to screen size like every
                // other gap in this layout — cancel out `quality_badge`/
                // `quality`'s own fixed construction-time margins (used by
                // Classic instead) so the box's spacing is the only gap.
                quality_badge.set_margin_start(0);
                quality.set_margin_start(0);
                let service_group = GtkBox::builder()
                    .orientation(Orientation::Horizontal)
                    .halign(Align::Start)
                    .build();
                service_group.append(&service.widget);
                service_group.append(&quality_badge);
                service_group.append(&quality);
                let controls_col = GtkBox::builder()
                    .orientation(Orientation::Vertical)
                    .valign(Align::End)
                    .build();
                controls_col.append(&service_group);
                controls_col.append(&band_row);

                // Splits the column into an exact 2:1 ratio — title block
                // vs. the controls band — regardless of how tall the text
                // actually renders, via a homogeneous 3-row grid (title
                // spans 2 rows, the band spans 1).
                let text_col_grid = gtk::Grid::builder()
                    .row_homogeneous(true).hexpand(true).vexpand(true)
                    .build();
                text_col_grid.attach(&title_group, 0, 0, 1, 2);
                text_col_grid.attach(&controls_col, 0, 2, 1, 1);

                let text_col = GtkBox::builder()
                    .orientation(Orientation::Vertical)
                    .hexpand(true).valign(Align::Start)
                    .css_classes(["wide-right-text"])
                    .build();
                text_col.append(&text_col_grid);

                // The artwork is a fixed square, sized directly from the
                // real screen dimensions (see compute_wide_right_art_side()
                // for the algorithm) rather than negotiated from
                // surrounding widgets. FlipCover has no intrinsic size of
                // its own (its measure() always returns "no preference"),
                // so without an explicit size it just stretches to fill
                // whatever a container gives it — and nothing in an
                // earlier version of this layout accounted for the
                // *vertical* budget at all (only the row's width), which
                // on a Raspberry Pi 5's screen produced a noticeably
                // undersized artwork with roughly half the screen left
                // empty below the seek bar. `set_valign(Start)` on both
                // (not the default Fill) so neither stretches past its own
                // size_request even if this row ends up taller than
                // expected for some other reason.
                art_overlay.set_hexpand(false);
                art_overlay.set_vexpand(false);
                art_overlay.set_valign(Align::Start);
                // Gap to text_col set below as a fraction of the artwork's
                // side, once known — a fixed px gap looked fine on one
                // screen and wrong on another (confirmed live: noticeably
                // too wide on a Raspberry Pi 5 next to a small screen's
                // smaller artwork), same lesson as the font/button sizing.
                let top_row = GtkBox::builder()
                    .orientation(Orientation::Horizontal)
                    .build();
                top_row.append(&art_overlay);
                top_row.append(&text_col);

                // Volume, overlaid on `top_row` rather than packed inside
                // it — `top_row` (art_overlay + text_col) and `seek_row`
                // below are both direct children of `content_block`, a
                // plain vertical `GtkBox`, so they share exactly the same
                // width/right edge as each other (and so does `seek`
                // itself, `hexpand`ed to fill `seek_row`) — overlaying on
                // `top_row` at `halign(End)` is what actually guarantees
                // alignment with the seek bar's own right edge, rather
                // than hoping some other nested box's width happens to
                // match. `valign(End)` puts it at the bottom of `top_row`,
                // roughly level with the transport controls in
                // `controls_col`/`band_row` below it — not level with the
                // seek bar itself, which is a separate row underneath
                // (avoids colliding with `dur`'s own right-aligned slot in
                // `time_row`).
                volume.set_halign(Align::End);
                volume.set_valign(Align::End);
                let top_row_overlay = gtk::Overlay::new();
                top_row_overlay.set_child(Some(&top_row));
                top_row_overlay.add_overlay(&volume);

                // Seek scale full width; position/status/duration sit on
                // their own row underneath it — a CenterBox, same as
                // Classic, so `status` lands genuinely centered between
                // pos/dur rather than as a separate row (which pushed the
                // controls further down for no reason).
                seek.set_hexpand(true);
                let time_row = gtk::CenterBox::new();
                pos.set_halign(Align::Start);
                dur.set_halign(Align::End);
                time_row.set_start_widget(Some(&pos));
                time_row.set_center_widget(Some(&status));
                time_row.set_end_widget(Some(&dur));
                let seek_row = GtkBox::builder().orientation(Orientation::Vertical).spacing(2).build();
                seek_row.append(&seek);
                seek_row.append(&time_row);

                let content_block = GtkBox::builder()
                    .orientation(Orientation::Vertical).hexpand(true)
                    .valign(Align::Start)
                    .build();
                content_block.append(&top_row_overlay);
                content_block.append(&seek_row);

                // Pushes content_block down by 1/10 of the available
                // height, proportionally rather than by a fixed pixel
                // guess: a 10-row homogeneous grid forces every row to the
                // same height (including the empty row 0, once given more
                // height than its own natural content needs — see
                // vexpand/hexpand below), so row 0 alone is always exactly
                // 1/10 of whatever height this view ends up with. A plain
                // margin_top couldn't do that without knowing the window's
                // actual size, which isn't available in static CSS either.
                let top_spacer = GtkBox::new(Orientation::Vertical, 0);
                let vgrid = gtk::Grid::builder()
                    .row_homogeneous(true).hexpand(true).vexpand(true)
                    .build();
                vgrid.attach(&top_spacer, 0, 0, 1, 1);
                vgrid.attach(&content_block, 0, 1, 1, 9);

                // Margins set below as fractions of the artwork's side too.
                let outer = GtkBox::builder()
                    .orientation(Orientation::Vertical).hexpand(true).vexpand(true)
                    .build();
                outer.append(&vgrid);

                self.set_child(Some(&outer));

                // ── Proportional typography/control sizing ─────────────
                // Scoped to `outer` (not just `text_col`) so the same
                // rules also reach `pos`/`dur` (`.dim-label`), which live
                // in `seek_row` — a sibling of `top_row`/`text_col`, not a
                // descendant of it.
                let class = format!("wr-scale-{:x}", outer.as_ptr() as usize);
                outer.add_css_class(&class);
                let provider = gtk::CssProvider::new();
                if let Some(display) = gtk::gdk::Display::default() {
                    gtk::style_context_add_provider_for_display(
                        &display, &provider, gtk::STYLE_PROVIDER_PRIORITY_USER,
                    );
                }

                // Applies compute_wide_right_art_side()'s result for a
                // given screen size: sizes the artwork/text column and
                // re-derives font/button sizing off that same side length.
                // A plain closure (not a one-shot function) since it's
                // called either synchronously below (host_size_hint
                // already known — true for every Kiosk device switch) or
                // from the tick-callback fallback further down (only the
                // very first bind at startup, before the window's real
                // size is known yet).
                let apply_for_screen = glib::clone!(
                    #[strong] art_overlay, #[strong] text_col,
                    #[strong] title_group, #[strong] status_group, #[strong] service_group, #[strong] volume,
                    #[strong] service,
                    #[strong] band_row, #[strong] controls_col, #[strong] top_row,
                    #[strong] content_block, #[strong] outer,
                    #[strong] class, #[strong] provider,
                    move |screen_w: i32, screen_h: i32| {
                        let side = compute_wide_right_art_side(screen_w, screen_h);
                        art_overlay.set_size_request(side, side);
                        text_col.set_size_request(-1, side);
                        apply_wide_right_scale(
                            &class, &provider, &title_group, &status_group, &service_group, &volume, &service, side,
                        );

                        // All structural gaps/margins below are fractions
                        // of `side` too, for the same reason the font/
                        // button sizing is: a fixed px gap that looks right
                        // on one screen (this was tuned against a 4K
                        // desktop) is visibly too wide on a much smaller
                        // one — confirmed live on a Raspberry Pi 5, whose
                        // much-smaller artwork left a fixed 40px gap to
                        // text_col looking disproportionately large.
                        let s = side as f64;
                        band_row.set_spacing((s * 0.09).round() as i32);
                        controls_col.set_spacing((s * 0.03).round() as i32);
                        top_row.set_spacing((s * 0.10).round() as i32);
                        content_block.set_spacing((s * 0.18).round() as i32);
                        let margin_h = wide_right_margin_h(side);
                        let margin_v = (s * 0.06).round() as i32;
                        outer.set_margin_start(margin_h);
                        outer.set_margin_end(margin_h);
                        outer.set_margin_top(margin_v);
                        outer.set_margin_bottom(margin_v);
                    }
                );
                let initial = size_source().filter(|(w, h)| *w > 0 && *h > 0);
                if let Some((w, h)) = initial {
                    apply_for_screen(w, h);
                }
                // Keeps tracking `size_source()`'s result for as long as
                // this view lives (GTK stops calling a tick callback on
                // its own once the widget it's attached to is destroyed —
                // nothing to disconnect by hand here) rather than applying
                // once and stopping: this is what makes a resizable host
                // (a plain DeviceWindow, unlike Kiosk mode's fixed
                // fullscreen one) keep the artwork/text/controls sized
                // correctly as the user drags it, not just at the moment
                // it was built. `size_source()` itself must never be
                // influenced by anything this code sets (see `new()`'s own
                // comment on why it's a callback, not a snapshot) — an
                // earlier version measured this view's own allocated
                // height directly, which fed back into this same
                // computation and compounded into runaway growth every
                // frame; both hosts' current closures read something
                // external instead (the window's size, and — for
                // DeviceWindow specifically — the Paned's own divider
                // position, set only by user drag/config restore).
                let last_size = std::cell::Cell::new(initial.unwrap_or((0, 0)));
                self.add_tick_callback(glib::clone!(
                    #[strong] apply_for_screen,
                    move |_widget, _clock| {
                        let Some((w, h)) = size_source() else { return glib::ControlFlow::Continue };
                        if w <= 0 || h <= 0 { return glib::ControlFlow::Continue; }
                        if (w, h) != last_size.get() {
                            last_size.set((w, h));
                            apply_for_screen(w, h);
                        }
                        glib::ControlFlow::Continue
                    }
                ));
            }
        }

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
        imp.service.set(service).unwrap();
        imp.quality_badge.set(quality_badge).unwrap();
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
        imp.service.get().unwrap().set(None, imp.icons.get().unwrap());
        imp.quality_badge.get().unwrap().set_visible(false);
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
                    format_status_only(&ps.status)
                });
                // No service badge for a physical input — there's no app/
                // stream behind it, just whatever's plugged in, and its
                // name already goes in the title (see the TITLE block
                // below) instead.
                let service_name = if ps.is_physical_input { None } else { ps.source_name.as_deref() };
                imp.service.get().unwrap().set(service_name, imp.icons.get().unwrap());
                let quality_badge = imp.quality_badge.get().unwrap();
                match ps.codec_label.as_deref() {
                    Some(q) => { quality_badge.set_label(q); quality_badge.set_visible(true); }
                    None => quality_badge.set_visible(false),
                }
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
            // Also re-evaluated on a bare `OTHER` (a mode/input switch with
            // no real title change) — a physical input's title comes from
            // `source_name` (its own display name, e.g. "Optical In"), not
            // `ps.title`, which is meaningless without a real app/stream
            // behind it, and `is_physical_input`/`source_name` both change
            // via `OTHER`, not `TITLE`.
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
            if mask & PC::ARTIST != 0 {
                imp.artist.get().unwrap().set_text(if is_unknown(&ps.artist) { "" } else { &ps.artist });
            }
            if mask & PC::ALBUM != 0 {
                imp.album.get().unwrap().set_text(if is_unknown(&ps.album) { "" } else { &ps.album });
            }
            if mask & PC::OTHER != 0 {
                // Never hidden — see the quality label's construction
                // comment. An empty label keeps the same reserved height.
                let q = ps.quality.map(|q| format_quality_line(&q)).unwrap_or_default();
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
