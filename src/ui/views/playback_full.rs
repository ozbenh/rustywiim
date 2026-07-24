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
    use crate::ui::views::common::{QualityBadge, ServiceLabel, SwipeText};
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
        pub(super) quality_badge: OnceCell<QualityBadge>,
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
        /// Next to `volume`, on its left. Visibility follows
        /// `DeviceState::eq_hint()`, updated in `refresh()`/
        /// `render_offline()` alongside everything else that's
        /// connection/capability-dependent.
        pub(super) eq_btn:   OnceCell<gtk::Button>,
        /// `eq_btn`'s icon as an explicit child `gtk::Image`, not a plain
        /// `icon_name` button (same reason `VolumeControl`'s own icon is
        /// an explicit `pixel_size`-set child, per that struct's own
        /// comment) — so `WideRight`'s per-screen-size scaling can resize
        /// it to match the transport/volume icons instead of staying
        /// fixed at its small default while they scale up around it.
        pub(super) eq_icon:  OnceCell<gtk::Image>,
        /// The widget `fade_group()` fades for Kiosk's "All Controls"
        /// auto-hide — *not* necessarily the visible card itself. In
        /// WideRight it's `transport`, which genuinely is the whole
        /// translucent card (background included, safe to fade wholesale
        /// — nothing else shares it). In Classic it's `controls_row`
        /// (just the transport+volume `CenterBox`), deliberately *not*
        /// the outer card or its `Overlay` — those also contain
        /// `service_group` (a sibling overlay child, unaffected by
        /// `controls_row`'s own opacity but *not* by the `Overlay`
        /// widget's own) and the seek bar, none of which should fade.
        /// See `fade_group()`'s own doc comment.
        pub(super) controls_card: OnceCell<gtk::Widget>,
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
                    // view's device — emitted by the EQ button next to
                    // `volume`; the host (device_window) is what actually
                    // owns presenting `ui::eq::panel::EqPanel`, per this
                    // codebase's "views ask, never know what the host is"
                    // convention (see `views/mod.rs`'s doc comment).
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
    format_quality_line, format_status_only, is_unknown, QualityBadge, ServiceLabel, SwipeText,
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
/// Rounds to the nearest *even* integer — confirmed live (see
/// `dark.css`/`system.css`'s own "font-size 18px, not 17" fix) that
/// certain odd font-size values hit a rasterization/hinting rounding
/// artifact at some display scale factors (Kiosk mode's album name
/// missing its top line of pixels on a Raspberry Pi 5). That fix picked
/// one hardcoded value by hand; every font size below is computed
/// proportionally instead, so a plain `.round()` can land on an odd value
/// depending on the actual screen height — this rounds every one of them
/// the same way instead.
fn round_to_even(v: f64) -> i32 {
    let r = v.round() as i32;
    if r % 2 != 0 { r + 1 } else { r }
}

fn apply_wide_right_scale(
    class: &str, provider: &gtk::CssProvider,
    title_group: &GtkBox, status_group: &GtkBox, service_group: &GtkBox, volume: &VolumeControl,
    eq_icon: &gtk::Image, volume_cluster: &GtkBox,
    service: &ServiceLabel, quality_badge: &QualityBadge,
    h: i32,
) {
    let h = h as f64;
    // Two independent nudges by request, on top of everything else this
    // function already scales proportionally: the service/quality/bitrate
    // "ensemble" (service name + its icon, the quality badge, and the
    // bitrate/samplerate/bit-depth line) 5% smaller, and the transport/
    // volume controls 5% bigger — applied as flat multipliers on top of
    // each element's own existing formula rather than baked into the base
    // ratios, so they stay easy to re-tune independently later.
    const SERVICE_ENSEMBLE_SCALE: f64 = 0.80;
    const CONTROLS_SCALE: f64 = 1.20;
    // Two rounds of "20% smaller" on top of the original pass
    // (0.22/0.12/0.10/0.09/0.055/0.03), tuned against live testing on
    // both a 4K desktop and a Raspberry Pi touchscreen.
    // Title reduced a further ~15% (0.1408 -> 0.12) by request — still
    // clearly bigger than artist below.
    let title_px  = round_to_even(h * 0.12);
    let artist_px = round_to_even(h * 0.0768);
    let album_px  = round_to_even(h * 0.064);
    // Wood's VFD-panel captions ("Title"/"Artist"/"Album", EngravedLabel) —
    // one shared size for all three (they're a row label, not a hierarchy
    // like title/artist/album itself), a bit smaller than the album line
    // per Ben's ask. Harmless to always compute/emit — the `.vfd-caption`
    // selector below simply matches nothing when `vfd_panel` is off.
    let caption_px = round_to_even(album_px as f64 * 0.85);
    // "Slightly smaller than the album name" per the design ask, then
    // reduced another ~20% (0.85 -> 0.68) — the whole badge read too big
    // in Kiosk mode. The icon's height (`BrandIcon::set_height()`, a
    // widget property, not CSS-reachable) is kept proportional to this
    // text at the same 3:1 ratio Classic's fixed values use (36px icon :
    // 12px `.service-name` base font-size), then reduced a further 10%
    // (3.0 -> 2.7) — the icon specifically (not the text) still read too
    // big, then another ~10% (2.7 -> 2.43) once the brand marks switched
    // to `BrandIcon`'s true-aspect-ratio sizing (wordmark icons rendered
    // visibly bigger than before at the same height), then another 15%
    // (2.43 -> 2.07), main/Kiosk window only (Mini has its own fixed
    // value, sized separately).
    let service_px = round_to_even(album_px as f64 * 0.68 * SERVICE_ENSEMBLE_SCALE);
    let service_icon_px = (service_px as f64 * 2.07).round() as i32;
    service.set_icon_pixel_size(service_icon_px);
    // 20% smaller than the service icon — see `QualityBadge::new()`'s
    // comment for why the quality badge reads smaller everywhere.
    quality_badge.set_icon_pixel_size((service_icon_px as f64 * 0.8).round() as i32);
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
    let status_px = round_to_even((h * 0.0352).max(18.0));
    // The bitrate/samplerate/bit-depth string specifically, 20% smaller
    // than `status_px` (which `.dim-label`/`.vol-level` still use as-is) —
    // by request, reads as secondary detail rather than matching the
    // pos/dur/status row it sits under.
    let quality_line_px = round_to_even((status_px as f64 * 0.8 * SERVICE_ENSEMBLE_SCALE).max(14.0));
    crate::ui::dbg_ui(&format!(
        "wide-right font sizes: title={title_px} artist={artist_px} album={album_px} \
         service={service_px} status={status_px} quality_line={quality_line_px} (h={h})",
    ));
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
    let card_h = (((h / 3.0) * (2.0 / 3.0) - 2.0 * card_padding) * CONTROLS_SCALE).max(24.0);
    let transport_btn = (card_h * 0.55).round() as i32;
    let play_btn      = (card_h * 0.68).round() as i32;
    let loop_btn       = transport_btn;
    let transport_icon = (transport_btn as f64 * 0.45).round() as i32;
    let play_icon      = (play_btn as f64 * 0.45).round() as i32;
    // Volume/EQ a little shorter than the round transport buttons next to
    // them (Ben's ask) — `.vol-btn` used to match `transport_btn` exactly
    // (100%); `.eq-btn` had no explicit height rule here at all, so it
    // was left to whatever Adwaita's own content-based button sizing gave
    // it, inconsistent with volume right next to it. Both now share one
    // explicit, slightly-reduced height instead.
    let eq_vol_btn_h = (transport_btn as f64 * 0.8).round() as i32;
    // The volume button's own icon is a `pixel_size`-set child Image, not
    // an icon-name button `-gtk-icon-size` (below) could reach — confirmed
    // live, it otherwise stays fixed at its small default while the other
    // transport icons scale up around it, reading as noticeably tinier.
    volume.set_icon_pixel_size(transport_icon);
    // Same fix, same reason — the EQ button sat right next to `volume` at
    // its small fixed default size while everything around it scaled up,
    // confirmed live as visibly mismatched in Kiosk mode.
    eq_icon.set_pixel_size(transport_icon);
    // The gap between the EQ button and volume was a fixed 4px (set at
    // construction) — same class of bug as everything else in this
    // function: it read fine at the icon's small default size but stayed
    // stuck at 4px while the icons around it scaled up to ~100px in Kiosk
    // mode, so the two buttons read as glued together. A fraction of `h`
    // instead, same as `service_gap` above.
    volume_cluster.set_spacing((h * 0.045).round().max(8.0) as i32);

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

    // `.play-btn-old-gtk4` alongside `.play-btn` below: on GTK 4.14-4.18
    // (Ubuntu 24.04 — see `PlaybackView::pick_play_btn_css()`) the play
    // button carries "play-btn-old-gtk4" instead of "play-btn" (a separate
    // workaround for a box-shadow rendering bug on that GTK range — see
    // wood.css's own `.play-btn-old-gtk4` comment), so a rule naming only
    // "play-btn" never matches it there at all. Confirmed live: with only
    // the single selector, the button got no explicit min-width/min-
    // height/icon-size in Kiosk's WideRight layout on Ubuntu 24.04 and
    // collapsed to a sliver a few px wide (full height, since nothing else
    // constrains its width) — fine in Classic, which never runs this
    // per-instance CSS at all, and fine on a GTK new enough to still be
    // "play-btn".
    provider.load_from_string(&format!(
        ".{class} .track-title {{ font-size: {title_px}px; }}\n\
         .{class} .track-artist {{ font-size: {artist_px}px; }}\n\
         .{class} .track-album {{ font-size: {album_px}px; }}\n\
         .{class} .vfd-caption {{ font-size: {caption_px}px; }}\n\
         .{class} .service-name {{ font-size: {service_px}px; }}\n\
         .{class} .quality-label {{ font-size: {quality_line_px}px; }}\n\
         .{class} .dim-label {{ font-size: {status_px}px; }}\n\
         .{class} .vol-level {{ font-size: {status_px}px; }}\n\
         .{class} .seek-scale trough {{ min-height: {seek_h}px; border-radius: {half}px; }}\n\
         {card_padding_rule}\
         .{class} .transport-btn:not(.vol-mute-btn) {{ min-width: {transport_btn}px; min-height: {transport_btn}px; -gtk-icon-size: {transport_icon}px; }}\n\
         .{class} .loop-btn {{ min-width: {loop_btn}px; min-height: {loop_btn}px; -gtk-icon-size: {transport_icon}px; }}\n\
         .{class} .play-btn, .{class} .play-btn-old-gtk4 {{ min-width: {play_btn}px; min-height: {play_btn}px; -gtk-icon-size: {play_icon}px; }}\n\
         .{class} .vol-btn {{ min-height: {eq_vol_btn_h}px; }}\n\
         .{class} .eq-btn {{ min-height: {eq_vol_btn_h}px; }}\n",
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
    /// `is_kiosk` gates theme choices that are meant for Kiosk mode
    /// specifically rather than any `WideRight` view (`KioskWindow` passes
    /// `true`; `DeviceWindow`'s own "L" toggle passes `false`) — currently
    /// just Wood's `vfd_panel` tunable (see `ThemeTunables`' own doc
    /// comment): without this, that theme's normal-mode `WideRight` toggle
    /// would pick up the VFD glow/panel too, which isn't wanted.
    pub(crate) fn new(
        ds: &DeviceState, icons: &Rc<IconSet>, art_bg: Option<&ArtBackground>, layout: PlaybackLayout,
        size_source: Rc<dyn Fn() -> Option<(i32, i32)>>, text_speed_multiplier: f64, is_kiosk: bool,
    ) -> Self {
        let obj: Self = glib::Object::new();
        obj.build(ds, icons, art_bg, layout, size_source, text_speed_multiplier, is_kiosk);
        obj
    }

    fn pick_play_btn_css() -> &'static str {
        let major = gtk::major_version();
        let minor = gtk::minor_version();

        if major == 4 && (14..=18).contains(&minor) {
            "play-btn-old-gtk4"
        } else {
            "play-btn"
        }
    }

    fn build(
        &self, ds: &DeviceState, icons: &Rc<IconSet>, art_bg: Option<&ArtBackground>, layout: PlaybackLayout,
        size_source: Rc<dyn Fn() -> Option<(i32, i32)>>, text_speed_multiplier: f64, is_kiosk: bool,
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
        // Opt in to a theme-drawn raised-edge frame around the artwork
        // (inert unless the active theme defines it — see
        // FlipCover::set_frame_enabled()'s doc comment).
        artwork.set_frame_enabled(true);
        artwork.set_hexpand(true);
        artwork.set_vexpand(true);

        // drop_shadow starts false regardless of theme — it's only wanted
        // for legibility against Modern's blurred background, and gets
        // toggled live by update_art_background_visibility() in ui/mod.rs
        // (called once more right after window construction, so this
        // initial value only matters for the instant before that runs).
        let title  = SwipeText::new("Not connected", "track-title",  true, false, text_speed_multiplier);
        let artist = SwipeText::new("",              "track-artist", true, false, text_speed_multiplier);
        let album  = SwipeText::new("",              "track-album",  true, false, text_speed_multiplier);
        // "dim-label", not "status-badge" — same grey as pos/dur (they
        // share this row now), not the accent/highlight color the old
        // class used, matching by request. `status-badge` is gone entirely
        // (was only ever used here).
        let status = Label::builder().css_classes(["dim-label"]).halign(Align::Center).build();
        let service = ServiceLabel::new("service-name");
        // Next to `service`, same rounded-rect badge as its own text
        // fallback (`ServiceLabel::new()`'s "service-name-pill") and same
        // font-size class, so the two read as one matched pair.
        // Same rounded-rect badge as `service`'s own text fallback and
        // same font-size class, so the two read as one matched pair —
        // shown as the Hi-Res Audio certification mark instead of text
        // when the current tier has one (`QualityBadge::set()`).
        let quality_badge = QualityBadge::new("service-name");
        // Extra gap from the service label/icon on top of the group box's
        // own spacing — reads as a separate badge, not glued to it.
        quality_badge.widget.set_margin_start(6);
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
            .css_classes([PlaybackView::pick_play_btn_css(), "circular", "suggested-action"]).build();
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

        // EQ editor entry point, next to volume, on its left — hidden
        // until `refresh()` learns this device's `EqHint`. Emits
        // `configure-eq` rather than opening the panel directly: this
        // view doesn't know what its host is (see `views/mod.rs`'s doc
        // comment) — `device_window` is what actually presents
        // `ui::eq::panel::EqPanel`.
        let eq_icon = gtk::Image::builder()
            .icon_name("rustywiim-equalizer-symbolic")
            .build();
        // "eq-btn" alongside the generic Adwaita "circular"/"flat" pair:
        // inert under System/Dark/Modern (none of their stylesheets
        // reference it, same as panel-card/preset-tile staying inert
        // outside the themes that style them), but lets wood.css give this
        // button the same raised bevel as the volume/transport buttons
        // next to it without also reshaping it under every other theme
        // the way adding it to the existing ".vol-btn" class list would.
        let eq_btn = gtk::Button::builder()
            .child(&eq_icon)
            .tooltip_text("Equalizer")
            .css_classes(["circular", "flat", "eq-btn"])
            .visible(false)
            .build();
        eq_btn.connect_clicked({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                obj.emit_by_name::<()>("configure-eq", &[]);
            }
        });
        let volume_cluster = GtkBox::builder()
            .orientation(Orientation::Horizontal).spacing(4).build();
        volume_cluster.append(&eq_btn);
        volume_cluster.append(&volume);

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
                controls_row.set_end_widget(Some(&volume_cluster));

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
                service_group.append(&quality_badge.widget);
                let controls_overlay = gtk::Overlay::new();
                controls_overlay.set_child(Some(&controls_row));
                controls_overlay.add_overlay(&service_group);

                // Seek block + transport grouped into one card under
                // RustyWiiM Modern (see modern.css); inert everywhere else,
                // same as "panel-card". Kept exactly as originally built —
                // this card's own background/layout never changes, static
                // or under Kiosk's "All Controls" auto-hide.
                let controls_card = GtkBox::builder()
                    .orientation(Orientation::Vertical).spacing(8)
                    .css_classes(["controls-card"])
                    .build();
                controls_card.append(&seek_block);
                controls_card.append(&controls_overlay);
                // The fade target is `controls_row` specifically (just
                // transport+volume), not the whole card: fading the card
                // itself, or `controls_overlay`, would also fade
                // `service_group` (it's a sibling overlay child of the
                // *same* Overlay, and an Overlay's own opacity applies to
                // its whole composited result including overlay children)
                // — service/quality aren't meant to hide (still true even
                // after the "All Controls" ask), and the seek bar and the
                // card's own translucent background should both stay put
                // regardless, matching how this looked before "All
                // Controls" existed at all (confirmed live: an earlier
                // attempt that also moved the seek bar out of this card
                // entirely visibly shrank/shifted it — not wanted).
                self.imp().controls_card.set(controls_row.clone().upcast()).unwrap();

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
                // A theme can ask for a different internal arrangement of
                // this same layout — currently just Wood's
                // "kiosk_boxed_controls" (see ThemeTunables' own doc
                // comment): seek bar + service/quality + transport grouped
                // into one shared ".controls-card" (styled like the normal
                // device window's own card), instead of the default
                // "transport alone in a small card, seek/service loose
                // elsewhere in the column" arrangement below. Read once
                // here, not re-checked on a later theme switch — see that
                // same doc comment for why this is a construction-time,
                // not live-reactive, choice.
                let tunables = crate::ui::current_tunables();
                let boxed_controls = tunables.kiosk_boxed_controls;
                // Wood's Kiosk-only VFD panel/glow (see ThemeTunables'
                // `vfd_panel` doc comment) — gated on `is_kiosk` too, not
                // just the tunable, so this theme's normal-mode WideRight
                // toggle stays visually unaffected.
                let vfd_panel = is_kiosk && tunables.vfd_panel;
                let vfd_glow = vfd_panel
                    .then(|| tunables.vfd_glow_color.as_deref().and_then(|s| gtk::gdk::RGBA::parse(s).ok()))
                    .flatten();

                // Volume moved out of the controls row entirely, to the
                // outer right edge of the column (aligned with the seek
                // bar's own right edge — see the `top_row_overlay` comment
                // below for why that's structurally guaranteed, not a
                // coincidence), by request.
                // Same semi-transparent card styling the classic layout's
                // seek+transport group gets under RustyWiiM Modern (see
                // modern.css's ".controls-card" — inert under System/Dark).
                // Volume lives elsewhere in this layout (see above), so
                // unlike Classic this card is just the transport row —
                // *unless* `boxed_controls` is on, in which case a single
                // shared card further down (see below) takes over that
                // role instead, and `transport` itself stays classless.
                if !boxed_controls {
                    transport.add_css_class("controls-card");
                    self.imp().controls_card.set(transport.clone().upcast()).unwrap();
                }
                // Left-aligned like the rest of this column, not centered
                // (the shared construction above centers it for the
                // classic layout, where it sits under a centered artwork)
                // — *unless* boxed, which puts transport in a CenterBox's
                // center slot instead (below), matching Classic's own
                // controls_row exactly; Center here is what makes it
                // actually center within that cell rather than hugging
                // its own left edge inside it.
                transport.set_halign(if boxed_controls { Align::Center } else { Align::Start });

                // Left-aligned, not centered under the artwork — the
                // "wide-right-text" class (see dark.css/system.css) scales
                // up title/artist/album to read at a distance.
                title.set_center_when_fits(false);
                artist.set_center_when_fits(false);
                album.set_center_when_fits(false);
                // Same glow color on all three lines — they already read as
                // a clear hierarchy from `apply_wide_right_scale()`'s font
                // sizing alone (title biggest, album smallest), so a second,
                // brightness-based distinction on top of that was redundant
                // (and inconsistent with wood.css's own `.vfd-panel .track-*`
                // rule, which also gives all three the same fill color now).
                if let Some(glow) = vfd_glow {
                    title.set_glow(Some(glow.clone()));
                    artist.set_glow(Some(glow.clone()));
                    album.set_glow(Some(glow));
                }
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
                // Populated below only when `vfd_panel` is on — captured by
                // `apply_for_screen` further down so the captions' font
                // size can scale with everything else and restyle when it
                // changes, same as `title`/`artist`/`album` themselves.
                let mut vfd_captions: Vec<crate::ui::engraved_label::EngravedLabel> = Vec::new();
                if vfd_panel {
                    // Three separate backlit panels stacked vertically, one
                    // per line, rather than one big panel wrapping all
                    // three — experimental (Ben's ask): each line gets its
                    // own outset "vfd-panel" box (wood.css), so the group
                    // reads as a stack of individual readouts and better
                    // fills the column's vertical space than one large box
                    // with the same three lines just floating inside it.
                    // `title_group`'s own spacing (set dynamically below by
                    // `apply_wide_right_scale()`) is the gap between them.
                    //
                    // Each panel gets an "engraved into the wood" caption
                    // above it (`EngravedLabel` — a plain static widget,
                    // no scrolling/fading, unlike `ScrollFadeLabel`, which
                    // stays scoped to the VFD glow/drop-shadow looks it
                    // already has) naming the line below it, sitting
                    // directly on the wood-grain window background (Wood's
                    // own `ArtBackground` stays hidden under this theme —
                    // see `update_art_background_visibility()` — so that
                    // background is genuinely visible in the gap above
                    // each panel, not painted over).
                    //
                    // `title_group`/each `group` are `vexpand`+`Fill`,
                    // rather than the default `Start`, so the three
                    // caption+panel groups stretch to cover the artwork's
                    // full height between them instead of hugging the top
                    // with empty space left below — the panel itself
                    // stays `vexpand`+`Fill` too, with its stack
                    // vertically centered inside whatever extra height
                    // that gives it.
                    title_group.set_valign(Align::Fill);
                    title_group.set_vexpand(true);
                    for (caption, stack) in [
                        ("Title", &title.stack), ("Artist", &artist.stack), ("Album", &album.stack),
                    ] {
                        let cap = crate::ui::engraved_label::EngravedLabel::new(caption);
                        cap.add_label_css_class("vfd-caption");
                        cap.set_halign(Align::Start);
                        vfd_captions.push(cap.clone());

                        // `vexpand` too, not just `valign(Center)` — a
                        // `GtkBox` only gives a child extra room to center
                        // within if that child can actually expand into it;
                        // without this the stack stays sized to its own
                        // natural (small) height and the panel's real extra
                        // space (from its own `vexpand` below) goes
                        // unclaimed rather than centering the text inside
                        // it, which read as the text sitting at the panel's
                        // top edge instead of its middle.
                        stack.set_valign(Align::Center);
                        stack.set_vexpand(true);
                        let panel = GtkBox::builder()
                            .orientation(Orientation::Vertical)
                            .valign(Align::Fill)
                            .vexpand(true)
                            .css_classes(["vfd-panel"])
                            .build();
                        panel.append(stack);

                        let group = GtkBox::builder()
                            .orientation(Orientation::Vertical)
                            .vexpand(true)
                            .build();
                        group.append(&cap);
                        group.append(&panel);
                        title_group.append(&group);
                    }
                } else {
                    title_group.append(&title.stack);
                    title_group.append(&artist.stack);
                    title_group.append(&album.stack);
                }

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
                // the band's own cell in text_col_grid). `boxed_controls`
                // doesn't use this at all — transport goes into a
                // CenterBox instead (below), matching Classic's own
                // controls_row exactly (transport centered, volume/EQ at
                // the end) — `status_group` isn't part of that either in
                // Classic, so it gets its own small extra row in the card
                // instead, below. Still built either way (see
                // `controls_col`'s own comment on why an orphaned-but-
                // real widget is fine) since `apply_for_screen` captures
                // it unconditionally for its own spacing call.
                let band_row = GtkBox::builder()
                    .orientation(Orientation::Horizontal)
                    .valign(Align::End)
                    .build();
                if !boxed_controls {
                    band_row.append(&transport);
                    band_row.append(&status_group);
                }

                // Service name + quality badge + bitrate/depth/rate
                // string — moved between the artwork/text row and the seek
                // bar instead of above the controls band (by request,
                // 2026-07-21: it read as "floating" once Kiosk's "All
                // Controls" auto-hide could fade the band away from under
                // it). Left justified, same as everything else in this
                // layout — added as its own child of `content_block` below,
                // not nested inside `art_overlay`'s own column (an earlier
                // attempt at that distorted `top_row_overlay`'s height).
                // Its own gap above/below is `content_block`'s spacing,
                // shared with the rest of that Box's children — cancel out
                // `quality_badge`/`quality`'s own fixed construction-time
                // margins (used by Classic instead) so there's no separate
                // extra gap fighting it.
                quality_badge.widget.set_margin_start(0);
                quality.set_margin_start(0);
                let service_group = GtkBox::builder()
                    .orientation(Orientation::Horizontal)
                    .halign(Align::Start)
                    .build();
                service_group.append(&service.widget);
                service_group.append(&quality_badge.widget);
                service_group.append(&quality);
                // `controls_col`/`text_col_grid` still get built even when
                // `boxed_controls` is on and neither ends up attached
                // anywhere — both are captured by `apply_for_screen` below
                // regardless of layout choice, so keeping them real (if
                // unparented) objects avoids branching that closure too;
                // setting properties on an unparented widget is harmless,
                // it simply never renders.
                let controls_col = GtkBox::builder()
                    .orientation(Orientation::Vertical)
                    .valign(Align::End)
                    .build();
                let text_col_grid = gtk::Grid::builder()
                    .row_homogeneous(true).hexpand(true).vexpand(true)
                    .build();
                let text_col = GtkBox::builder()
                    .orientation(Orientation::Vertical)
                    .hexpand(true).valign(Align::Start)
                    .css_classes(["wide-right-text"])
                    .build();
                // Wood's Kiosk-only VFD panel (see `vfd_panel` above) — each
                // line gets its own outset backlit box now (`title_group`,
                // above), not `text_col` as a whole, so nothing needs
                // adding here.
                // `boxed_controls`: transport moves into a shared card
                // further down (built once seek_row exists, below) instead
                // of nesting under title_group in this column at all — so
                // `text_col` holds just the title block, no grid/split
                // needed since there's no controls band height to reserve
                // room for within it anymore. `controls_card` (the Kiosk
                // auto-hide fade target) is set later, once the card's own
                // CenterBox exists — see that construction's own comment.
                if boxed_controls {
                    text_col.append(&title_group);
                } else {
                    // Splits the column into an exact 2:1 ratio — title
                    // block vs. the controls band — regardless of how tall
                    // the text actually renders, via a homogeneous 3-row
                    // grid (title spans 2 rows, the band spans 1).
                    controls_col.append(&band_row);
                    text_col_grid.attach(&title_group, 0, 0, 1, 2);
                    text_col_grid.attach(&controls_col, 0, 2, 1, 1);
                    text_col.append(&text_col_grid);
                }

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
                // `boxed_controls`: volume/EQ move down into the shared
                // card instead (Classic's own controls_row end slot,
                // replicated below — "just like they are in the main
                // view," per Ben), so skip overlaying them here at all.
                if !boxed_controls {
                    volume_cluster.set_halign(Align::End);
                    volume_cluster.set_valign(Align::End);
                }
                let top_row_overlay = gtk::Overlay::new();
                top_row_overlay.set_child(Some(&top_row));
                if !boxed_controls {
                    top_row_overlay.add_overlay(&volume_cluster);
                }

                // Seek scale full width; position/status/duration sit on
                // their own row underneath it — a CenterBox, same as
                // Classic, so `status` lands genuinely centered between
                // pos/dur rather than as a separate row (which pushed the
                // controls further down for no reason).
                seek.set_hexpand(true);
                let time_row = gtk::CenterBox::new();
                pos.set_halign(Align::Start);
                dur.set_halign(Align::End);
                // Wood's Kiosk-only VFD scanline (see `vfd_panel` above) —
                // `VfdScanlineOverlay::wrap()` on each of pos/status/dur
                // individually (not `time_row` as a whole) so each keeps
                // its own halign inside the CenterBox rather than the
                // overlay's own natural-size wrapping interfering with
                // that. Inert outside Wood/Kiosk: `time_row.set_*_widget()`
                // just takes the plain label directly otherwise.
                if vfd_panel {
                    time_row.set_start_widget(Some(&crate::ui::vfd_scanline_overlay::VfdScanlineOverlay::wrap(&pos)));
                    time_row.set_center_widget(Some(&crate::ui::vfd_scanline_overlay::VfdScanlineOverlay::wrap(&status)));
                    time_row.set_end_widget(Some(&crate::ui::vfd_scanline_overlay::VfdScanlineOverlay::wrap(&dur)));
                } else {
                    time_row.set_start_widget(Some(&pos));
                    time_row.set_center_widget(Some(&status));
                    time_row.set_end_widget(Some(&dur));
                }
                let seek_row = GtkBox::builder().orientation(Orientation::Vertical).spacing(2).build();
                seek_row.append(&seek);
                seek_row.append(&time_row);

                let content_block = GtkBox::builder()
                    .orientation(Orientation::Vertical).hexpand(true)
                    .valign(Align::Start)
                    .build();
                content_block.append(&top_row_overlay);
                // `boxed_controls`: seek bar, service/quality, and the
                // transport band go into one shared card (styled like the
                // normal device window's own ".controls-card", per Ben's
                // ask — "it has some kind of glassy look") instead of
                // service/seek sitting loose as direct children of
                // `content_block` — same order requested: seek bar first,
                // service/quality under it, transport at the bottom.
                // `boxed_card` picks up its own margin-top in
                // `apply_for_screen()` below (a bit further down from the
                // artwork than the default arrangement's own gap, also by
                // request) rather than reusing `service_group`/`seek_row`'s
                // existing margins here, which were tuned for their old
                // role as `content_block`'s own direct children, not as a
                // card's internal padding — that Classic already handles
                // via plain `spacing()` on its own equivalent card, not
                // per-child margins, so this mirrors that instead.
                let boxed_card = boxed_controls.then(|| {
                    // Plain 8px, matching Classic's own equivalent card
                    // (`controls_card.spacing(8)`) — not proportional to
                    // screen size the way most of this layout's other
                    // gaps are (`apply_for_screen()` below, which sets this
                    // card's margin-top instead, but doesn't revisit this
                    // internal spacing).
                    // "controls-card-boxed", NOT "controls-card" — Kiosk's
                    // own wood.css rule strips ".controls-card"'s box
                    // entirely (the *other* half of this same request: no
                    // card behind the transport-only row), and this needs
                    // the opposite treatment, so it can't share that exact
                    // class name even though it wants the same base bevel
                    // styling otherwise (wood.css lists both selectors
                    // together for that shared recipe).
                    let card = GtkBox::builder()
                        .orientation(Orientation::Vertical)
                        .spacing(8)
                        .css_classes(["controls-card-boxed"])
                        .build();
                    // Same VFD treatment (font + amber glow) as the three
                    // panels above, on this card's own readout text — a
                    // marker class only, gated the same way (`vfd_panel`,
                    // itself already `is_kiosk`-gated), so Wood's normal-
                    // mode WideRight toggle stays unaffected and every
                    // other theme never adds it at all. Doesn't touch the
                    // transport/volume/EQ *buttons* sharing this card —
                    // wood.css's own `.vfd-readout` rule only targets the
                    // text classes (service-name/quality-label/dim-label/
                    // vol-level), which none of those buttons carry.
                    if vfd_panel {
                        card.add_css_class("vfd-readout");
                    }
                    card.append(&seek_row);
                    // Transport centered, volume/EQ at the end — Classic's
                    // own `controls_row` exactly (Ben: "EQ and volume also
                    // move down into the box, just like they are in the
                    // main view"). `service_group` as a floating overlay
                    // at the start rather than the CenterBox's own start
                    // slot, same reasoning as Classic's `controls_overlay`
                    // (see that construction's own comment): a CenterBox
                    // shrinks its start/end children to keep the center
                    // one truly centered, which starves a start child of
                    // width once transport+volume already fill most of
                    // the row — an overlay child sizes off its own natural
                    // size instead, floating in whatever room is actually
                    // free at the row's left edge.
                    let controls_row_boxed = gtk::CenterBox::new();
                    controls_row_boxed.set_center_widget(Some(&transport));
                    controls_row_boxed.set_end_widget(Some(&volume_cluster));
                    let controls_overlay_boxed = gtk::Overlay::new();
                    controls_overlay_boxed.set_child(Some(&controls_row_boxed));
                    // Wood's Kiosk-only VFD scanline (see `vfd_panel`
                    // above) — `service_group` (service name/icon, quality
                    // badge/icon, bitrate string) wrapped in its own nested
                    // `VfdScanlineOverlay::wrap()` first, so the dimming
                    // layer sits directly on top of it specifically, not
                    // the whole card; that wrapper then becomes the actual
                    // overlay child here, at the exact same natural size/
                    // position `service_group` alone would have had.
                    if vfd_panel {
                        controls_overlay_boxed.add_overlay(
                            &crate::ui::vfd_scanline_overlay::VfdScanlineOverlay::wrap(&service_group),
                        );
                    } else {
                        controls_overlay_boxed.add_overlay(&service_group);
                    }
                    // A few extra px on top of `card`'s own uniform
                    // spacing(8) between children, specifically widening
                    // the seek-bar-to-this-row gap and no other (Ben's
                    // ask) — `card.spacing()` alone can't do that (it's
                    // one value shared by every gap in the box), so this
                    // is additive margin on just this child instead.
                    // Doesn't touch the bottom status bar (remote/network
                    // icons, kiosk.rs's own `StatusBarView`): that's a
                    // fixed-size sibling of the whole playback view in
                    // `content_holder`, not part of `content_block`/
                    // `vgrid` at all, so nothing inside this card can
                    // shift its position.
                    controls_overlay_boxed.set_margin_top(8);
                    card.append(&controls_overlay_boxed);
                    // `status_group` (bt_pair) isn't part of Classic's own
                    // controls_row either — usually invisible (no active
                    // Bluetooth pairing), so its exact placement is low-
                    // stakes; a plain extra row at the bottom of the card
                    // rather than trying to squeeze it into the CenterBox
                    // above, which is tuned for exactly two slots.
                    card.append(&status_group);
                    // Kiosk auto-hide's own fade target for the boxed
                    // case: `controls_row_boxed` (transport+volume), not
                    // the whole card (which also holds seek/service that
                    // shouldn't fade) — the exact same widget Classic's
                    // own `controls_row` already is for this same purpose,
                    // see `controls_card`'s field doc comment.
                    self.imp().controls_card.set(controls_row_boxed.clone().upcast()).unwrap();
                    content_block.append(&card);
                    card
                });
                if !boxed_controls {
                    // Sits between the artwork/text row and the seek bar
                    // (by request, 2026-07-21 — moved here from above the
                    // controls band, where it read as "floating" once "All
                    // Controls" could fade the band away from under it) —
                    // a real sibling of `top_row_overlay`/`seek_row`, so
                    // its own height (plus its and `seek_row`'s small
                    // margins below) is the only thing added to
                    // `content_block`'s total — both margins are kept
                    // deliberately tight (not the original generous gap)
                    // so that total stays close to what `content_block`
                    // used to need, since any growth here gets amplified
                    // ~10/9x by `vgrid`'s homogeneous-row math below and
                    // pushed the bottom status bar down when it wasn't
                    // (confirmed live via screenshot).
                    content_block.append(&service_group);
                    content_block.append(&seek_row);
                }

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
                    #[strong] eq_icon, #[strong] volume_cluster,
                    #[strong] service, #[strong] quality_badge,
                    #[strong] title, #[strong] artist, #[strong] album,
                    #[strong] band_row, #[strong] controls_col, #[strong] top_row,
                    #[strong] content_block, #[strong] seek_row, #[strong] outer,
                    #[strong] class, #[strong] provider, #[strong] boxed_card,
                    #[strong] vfd_captions,
                    move |screen_w: i32, screen_h: i32| {
                        let side = compute_wide_right_art_side(screen_w, screen_h);
                        crate::ui::dbg_ui(&format!(
                            "wide-right rescale: screen={screen_w}x{screen_h} -> side={side}",
                        ));
                        art_overlay.set_size_request(side, side);
                        text_col.set_size_request(-1, side);
                        apply_wide_right_scale(
                            &class, &provider, &title_group, &status_group, &service_group, &volume,
                            &eq_icon, &volume_cluster,
                            &service, &quality_badge, side,
                        );
                        // Forces both faces of each SwipeText to recompute
                        // their style now that the CSS provider above just
                        // changed — see `SwipeText::force_restyle()`'s own
                        // doc comment for why this is needed (confirmed
                        // live: a still-hidden `gtk::Stack` face doesn't
                        // reliably notice the new rule on its own).
                        title.force_restyle();
                        artist.force_restyle();
                        album.force_restyle();
                        for cap in &vfd_captions {
                            cap.force_restyle();
                        }

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
                        // `content_block` has three children now
                        // (top_row_overlay, service_group, seek_row) —
                        // its own uniform `spacing` would put the *same*
                        // gap both above and below service_group, so each
                        // gets its own explicit margin instead. Both kept
                        // deliberately tight — service_group's own real
                        // height is already unavoidable added weight on
                        // `content_block`'s total, which `vgrid`'s
                        // homogeneous-row math below amplifies ~10/9x, so
                        // generous gaps on top of that pushed the bottom
                        // status bar down further than intended (confirmed
                        // live via screenshot). Together these two add
                        // noticeably less than the original single 0.18
                        // gap (directly below top_row_overlay) used to be
                        // on its own, before service_group existed here.
                        content_block.set_spacing(0);
                        if let Some(card) = &boxed_card {
                            // `boxed_controls`: seek_row/service_group are
                            // no longer direct children of `content_block`
                            // (see its own construction comment) — their
                            // margins above would just add unwanted extra
                            // gaps *inside* the card, on top of its own
                            // `spacing(8)`, so skip those and give the
                            // *card itself* the "further down from the
                            // artwork" gap instead (Ben's ask), still
                            // proportional to screen size like everything
                            // else here.
                            card.set_margin_top((s * 0.08).round() as i32);
                        } else {
                            service_group.set_margin_top((s * 0.04).round() as i32);
                            seek_row.set_margin_top((s * 0.1).round() as i32);
                        }
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
        imp.eq_btn.set(eq_btn).unwrap();
        imp.eq_icon.set(eq_icon).unwrap();

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

    /// Host hookup for the EQ button's request — see this file's own
    /// signal doc comment and `views/mod.rs`'s "views ask, never know
    /// what the host is" convention.
    pub(crate) fn connect_configure_eq<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("configure-eq", false, move |args| {
            let this = args[0].get::<Self>().unwrap();
            f(&this);
            None
        })
    }

    /// Widgets that fade together under Kiosk mode's "All Controls"
    /// auto-hide: the transport buttons (shuffle/prev/play/next/repeat,
    /// grouped as one widget — see `controls_card`'s own doc comment for
    /// what that actually is per layout), volume and the EQ button (listed
    /// explicitly since WideRight positions both outside that widget), and
    /// the plain status text ("Playing"/"Paused"/...). Deliberately *not*
    /// the seek bar, the card's own translucent background, or the
    /// service/quality badges — none of those should fade, in either
    /// layout (confirmed live: an earlier version that restructured
    /// Classic's card to pull the seek bar out of it visibly shrank/shifted
    /// the card even when nothing was fading, which is what this now
    /// avoids).
    pub(crate) fn fade_group(&self) -> Vec<gtk::Widget> {
        let imp = self.imp();
        vec![
            imp.controls_card.get().unwrap().clone(),
            imp.volume.get().unwrap().clone().upcast(),
            imp.eq_btn.get().unwrap().clone().upcast(),
            imp.status.get().unwrap().clone().upcast(),
        ]
    }

    /// Full render from the `DeviceState` cache — live or offline.
    fn refresh(&self) {
        let Some(ds) = self.imp().ds.get() else { return };
        if ds.device_info().is_some() {
            self.apply_mask(playback_changed::ALL);
            self.imp().eq_btn.get().unwrap()
                .set_visible(ds.eq_hint() == Some(crate::device::capabilities::EqHint::Likely));
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
        imp.quality_badge.get().unwrap().widget.set_visible(false);
        imp.quality.get().unwrap().set_label("");
        imp.artwork.get().unwrap().clear();
        if let Some(Some(bg)) = imp.art_bg.get() { bg.clear(); }
        imp.bt_pair.get().unwrap().set_visible(false);
        imp.eq_btn.get().unwrap().set_visible(false);
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
                quality_badge.set(ps.codec_label.as_deref(), imp.icons.get().unwrap());
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
