//! `EqPanel` — the device- and target-specific host that assembles the
//! reusable editor widgets (`GraphicEqView`/`ParametricEqView`) and the
//! small chrome widgets (`ui::eq::chrome`) into a working EQ editor for
//! one device connection.
//!
//! Presented as a plain `adw::Window` for this first pass — deliberately
//! not a GObject itself (nothing subscribes to `EqPanel`), same
//! `Rc<Inner>` shape `DeviceWindowInner` uses. The window is just
//! whatever currently hosts the content; nothing below assumes it's a
//! window specifically, so a future embedding (a panel/sidebar instead of
//! a standalone window) wouldn't need to touch this module's logic.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use crate::device::capabilities::{self, EqKind, EqLayerKind};
use crate::device::eq::{
    dbg as eq_dbg, default_peq_freq_hz, ChannelBands, EqPresetList, EqSession, EqState, EqTarget,
    GraphicBand, ParametricBand, PeqBandMode, TargetOverview,
};
use crate::device::state::DeviceState;
use crate::ui::icons::IconSet;

use super::chrome::{EqChannelPicker, EqChannelToggle, EqMechanismToggle, EqPresetPicker, EqSourcePicker};
use super::graphic::GraphicEqView;
use super::parametric::ParametricEqView;

const FLUSH_DELAY: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel { Stereo, Left, Right }

fn kind_token(kind: EqKind) -> &'static str {
    match kind {
        EqKind::Graphic => "graphic",
        EqKind::Parametric => "parametric",
        EqKind::ToneControl => "tonecontrol",
    }
}

struct Inner {
    ds:     DeviceState,
    icons:  Rc<IconSet>,
    stack:  gtk::Stack,
    graphic_view:     GraphicEqView,
    parametric_view:  ParametricEqView,
    mechanism_toggle: EqMechanismToggle,
    source_picker:    EqSourcePicker,
    channel_toggle:   EqChannelToggle,
    channel_picker:   EqChannelPicker,
    preset_picker:    EqPresetPicker,
    /// "Save…" — sits between `preset_picker` and `reset_btn`, sensitive
    /// only while the current state is "Custom" (see `update_save_btn()`).
    save_btn:         gtk::Button,
    reset_btn:        gtk::Button,
    /// Wrapper `gtk::Box`es pairing `source_picker`/`preset_picker` with
    /// their "Source:"/"Preset:" label — visibility is toggled on these,
    /// never on the pickers directly, so the label always tracks.
    source_cluster:   gtk::Box,
    preset_cluster:   gtk::Box,
    status_label:     gtk::Label,
    spinner:          gtk::Spinner,

    session:      RefCell<Option<EqSession>>,
    overview:     RefCell<Vec<TargetOverview>>,
    current_target: RefCell<Option<EqTarget>>,
    current_kind:   RefCell<Option<EqKind>>,
    current_lr:     Cell<bool>,
    current_channel: Cell<Channel>,
    /// The full (both-channel, un-flattened) `EqState` from the last
    /// fetch — needed so an L/R write can supply the untouched channel's
    /// real bands rather than a guess (see `set_*_bands_lr`'s doc
    /// comment: always both channels' full state, never a partial delta).
    last_full_state: RefCell<Option<EqState>>,

    /// Every mechanism's last-known `EqState` for the *current* target —
    /// populated up front (sequentially, one HTTP call per mechanism, per
    /// the codebase's no-parallel-HTTP rule) whenever the target changes,
    /// so switching between Off/GEQ/PEQ afterwards is instant instead of
    /// paying a fresh round trip each click (the "too much lag" testing
    /// feedback). Cleared on target switch; entries refreshed after any
    /// action that could have changed them (see `apply_overview_entry()`'s
    /// ground-truth re-fetch).
    mech_cache: RefCell<Vec<(EqKind, EqState)>>,
    /// Device-wide (not per-target) preset lists, cached per mechanism —
    /// see `EqSession::list_presets()`'s own doc comment on why this
    /// doesn't vary by target.
    preset_cache: RefCell<Vec<(EqKind, EqPresetList)>>,
    /// Which mechanisms (for the current target) have been locally edited
    /// since their own last fetch/preset-load — drives the preset
    /// picker's "Custom" label/checkmark. Tracked client-side rather than
    /// trusting the device to clear its own `Name` field on edit (both
    /// `pywiim`-adjacent prior art and this codebase's own `rustywiim-old`
    /// predecessor just echo whatever `Name` the device reports, with no
    /// "was it edited since" signal of their own to draw on) — we already
    /// know exactly when an edit happens (`on_band_changed()`), so there's
    /// no need to guess from the wire. Cleared per-kind on a genuine fetch
    /// success (fresh read, prefetch, resync, or preset load); set on any
    /// band edit. Cleared entirely on target switch.
    dirty_kinds: RefCell<Vec<EqKind>>,

    dirty_bands:     RefCell<BTreeSet<usize>>,
    flush_timer:     RefCell<Option<glib::SourceId>>,
    write_in_flight: Cell<bool>,
}

fn cache_get_state(cache: &[(EqKind, EqState)], kind: EqKind) -> Option<EqState> {
    cache.iter().find(|(k, _)| *k == kind).map(|(_, s)| s.clone())
}

fn cache_put_state(cache: &mut Vec<(EqKind, EqState)>, kind: EqKind, state: EqState) {
    match cache.iter_mut().find(|(k, _)| *k == kind) {
        Some(entry) => entry.1 = state,
        None => cache.push((kind, state)),
    }
}

fn cache_get_presets(cache: &[(EqKind, EqPresetList)], kind: EqKind) -> Option<EqPresetList> {
    cache.iter().find(|(k, _)| *k == kind).map(|(_, l)| l.clone())
}

fn cache_put_presets(cache: &mut Vec<(EqKind, EqPresetList)>, kind: EqKind, list: EqPresetList) {
    match cache.iter_mut().find(|(k, _)| *k == kind) {
        Some(entry) => entry.1 = list,
        None => cache.push((kind, list)),
    }
}

fn is_dirty(inner: &Rc<Inner>, kind: EqKind) -> bool {
    inner.dirty_kinds.borrow().contains(&kind)
}

fn mark_dirty(inner: &Rc<Inner>, kind: EqKind) {
    let mut d = inner.dirty_kinds.borrow_mut();
    if !d.contains(&kind) { d.push(kind); }
}

fn mark_clean(inner: &Rc<Inner>, kind: EqKind) {
    inner.dirty_kinds.borrow_mut().retain(|k| *k != kind);
}

/// Translated label + icon for a source (input) token — the same mapping
/// `InputOutputView::populate_input()` uses (`capabilities::
/// input_display_name()`/`icon_canon_for_input()` plus the device's own
/// rename map), reused here rather than showing the raw wire token.
fn source_display(inner: &Rc<Inner>, id: &str) -> (String, gtk::gdk::Paintable) {
    let device_id = inner.ds.capabilities().map(|c| c.device_id)
        .unwrap_or(capabilities::DeviceId::WiimGeneric);
    let std_name = capabilities::input_display_name(Some(device_id), id).to_string();
    let label = match inner.ds.mode_renames().get(id) {
        Some(user) if !user.is_empty() && user != &std_name => format!("{user} ({std_name})"),
        _ => std_name,
    };
    let icon_key = capabilities::icon_canon_for_input(id, device_id);
    (label, inner.icons.source_paintable(icon_key).clone())
}

fn eq_state_active_preset(state: &EqState) -> Option<String> {
    match state {
        EqState::Off => None,
        EqState::Graphic { active_preset, .. } | EqState::Parametric { active_preset, .. } => active_preset.clone(),
    }
}

/// Whether the current target's `kind` mechanism actually offers
/// Stereo-vs-L/R — confirmed live that the official app never offers
/// this for GEQ at all, only PEQ (see
/// `capabilities::EqMechanism::supports_lr_channels`'s own doc comment).
/// `false` if the profile/mechanism can't be found at all, same
/// fail-safe direction as that field's own resolution.
fn mechanism_supports_lr(inner: &Rc<Inner>, kind: EqKind) -> bool {
    inner.session.borrow().as_ref()
        .and_then(|s| s.profile().layers.iter().find(|l| l.kind == EqLayerKind::Source))
        .and_then(|l| l.mechanisms.iter().find(|m| m.kind == kind))
        .is_some_and(|m| m.supports_lr_channels)
}

/// Pushes the current label/checkmark to the preset picker without
/// touching its row list — cheap enough to call on every band edit
/// (flips the button to "Custom" immediately) as well as after a fresh
/// fetch (to show the device's own reported preset name again).
fn update_preset_button(inner: &Rc<Inner>, kind: EqKind) {
    let active = cache_get_state(&inner.mech_cache.borrow(), kind).and_then(|s| eq_state_active_preset(&s));
    let dirty = is_dirty(inner, kind);
    inner.preset_picker.set_active(active.as_deref(), dirty);
    update_save_btn(inner, active.as_deref(), dirty);
}

/// `save_btn` ("Save…", next to the preset picker itself, before Reset —
/// deliberately not inside the preset popover any more) is sensitive only
/// while the current state is "Custom": `dirty`, or no `active` match at
/// all (see `EqPresetPicker::set_presets()`'s own doc comment on why
/// those are the same state from the button's perspective) — there's
/// nothing new to offer saving otherwise.
fn update_save_btn(inner: &Rc<Inner>, active: Option<&str>, dirty: bool) {
    inner.save_btn.set_sensitive(dirty || active.is_none());
}

pub(crate) struct EqPanel {
    inner: Rc<Inner>,
}

impl EqPanel {
    /// Builds and presents a new panel for `ds` as a fully independent
    /// toplevel — deliberately no `transient_for` parent, matching every
    /// other secondary window in this codebase (Settings, DiscoveryWindow,
    /// device windows); see this window's own construction comment for why.
    /// Returns the window itself so a caller that needs to know when it
    /// closes (Kiosk mode, to stop inhibiting its own screensaver/auto-hide
    /// — see `KioskWindow::external_window_opened()`'s doc comment) can
    /// attach its own `connect_destroy()` without this module needing to
    /// know that's why.
    pub(crate) fn present(icons: &Rc<IconSet>, ds: &DeviceState) -> adw::Window {
        let graphic_view = GraphicEqView::new();
        let parametric_view = ParametricEqView::new();
        let off_page = adw::StatusPage::builder()
            .title("EQ Off")
            .description("Choose Graphic or Parametric EQ above to start editing.")
            .build();

        let stack = gtk::Stack::new();
        stack.add_named(&off_page, Some("off"));
        stack.add_named(&graphic_view, Some("graphic"));
        stack.add_named(&parametric_view, Some("parametric"));

        let mechanism_toggle = EqMechanismToggle::new();
        let source_picker = EqSourcePicker::new();
        let channel_toggle = EqChannelToggle::new();
        channel_toggle.set_visible(false);
        let channel_picker = EqChannelPicker::new();
        channel_picker.set_visible(false);
        let preset_picker = EqPresetPicker::new();
        // Real ellipsis (U+2026), not three dots — matches this codebase's
        // other "opens a further prompt" labels. Starts insensitive: no
        // target/kind is selected yet, so there's nothing to save.
        let save_btn = gtk::Button::builder().label("Save…").sensitive(false).build();
        let reset_btn = gtk::Button::builder().label("Reset").build();

        // Two rows. Each cluster's own visibility (toggled at every
        // existing `source_picker`/`preset_picker.set_visible()` call
        // site, redirected to the cluster instead) carries its "Source:"/
        // "Preset:" label along automatically — simpler than keeping a
        // separate label widget in sync with every one of those call
        // sites by hand. `reset_btn` only makes sense with a real
        // mechanism active (the same condition as the preset picker
        // itself), so it rides along inside `preset_cluster` too, rather
        // than needing its own separate visibility wiring.
        // "Source:"/"Preset:" share one fixed-width, right-justified label
        // column (both happen to be 7 characters) so the Source and Preset
        // buttons themselves line up at the same left edge across the two
        // rows, like a plain form layout, rather than each label just
        // hugging its own button at a slightly different offset.
        const SOURCE_PRESET_LABEL_CHARS: i32 = 7;
        let source_label = gtk::Label::builder().label("Source:").css_classes(["dim-label"]).build();
        source_label.set_width_chars(SOURCE_PRESET_LABEL_CHARS);
        source_label.set_xalign(1.0);
        let source_cluster = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        source_cluster.append(&source_label);
        source_cluster.append(&source_picker);

        // Top row, right side: the main Off/Graphic/Parametric selector.
        // Moved up here (off the second row, which also holds Preset)
        // since the two crowded/collided together — revisit this
        // placement if the layout changes again later (e.g. Source
        // becoming a button row). A small end margin keeps it from
        // sitting flush against the window's true right edge.
        mechanism_toggle.set_margin_end(8);

        let top_row = gtk::CenterBox::new();
        top_row.set_start_widget(Some(&source_cluster));
        top_row.set_end_widget(Some(&mechanism_toggle));

        // Second row, left side: Preset then Reset — `preset_cluster` is
        // what every existing `.set_visible()` call site (elsewhere in
        // this file) already targets, so `reset_btn` hides along with it
        // as a single unit without needing to touch those call sites again.
        let preset_label = gtk::Label::builder().label("Preset:").css_classes(["dim-label"]).build();
        preset_label.set_width_chars(SOURCE_PRESET_LABEL_CHARS);
        preset_label.set_xalign(1.0);
        let preset_inner = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        preset_inner.append(&preset_label);
        preset_inner.append(&preset_picker);
        preset_inner.append(&save_btn);
        preset_inner.append(&reset_btn);
        let preset_cluster = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        preset_cluster.append(&preset_inner);
        preset_cluster.set_visible(false);

        // Second row, right side: the Stereo/L-R mode menu (its own
        // "Mode:" label lives inside `EqChannelToggle` itself, so it only
        // ever shows alongside the menu — never for GEQ/Off) and — only
        // while L/R is selected — the Left/Right channel switch. Same end
        // margin as `mechanism_toggle` above it, so the "Right" switch
        // segment's right edge lines up with "Parametric"'s right edge
        // one row up, instead of sitting flush against the window's true
        // right edge while the row above doesn't.
        let channel_cluster = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        channel_cluster.set_margin_end(8);
        channel_cluster.append(&channel_toggle);
        channel_cluster.append(&channel_picker);

        let bottom_row = gtk::CenterBox::new();
        bottom_row.set_start_widget(Some(&preset_cluster));
        bottom_row.set_end_widget(Some(&channel_cluster));

        let top_bar = gtk::Box::new(gtk::Orientation::Vertical, 10);
        top_bar.set_margin_top(12);
        top_bar.set_margin_bottom(12);
        top_bar.set_margin_start(16);
        top_bar.set_margin_end(16);
        top_bar.append(&top_row);
        top_bar.append(&bottom_row);

        // A persistent bottom bar, not a row inside `content`'s normal
        // vertical flow — a plain child there took up space only while
        // visible, so every "Loading…"/error message appearing or
        // disappearing shifted the whole GEQ/PEQ view up and down.
        // `status_label` itself is never hidden (only its *text* changes,
        // to "" when idle) so this bar's own height stays fixed instead
        // of collapsing/expanding with it.
        let status_label = gtk::Label::builder().halign(gtk::Align::Start).build();
        let spinner = gtk::Spinner::new();
        let status_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        status_row.set_margin_top(6);
        status_row.set_margin_bottom(6);
        status_row.set_margin_start(12);
        status_row.set_margin_end(12);
        status_row.append(&spinner);
        status_row.append(&status_label);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
        content.append(&top_bar);
        content.append(&stack);

        let header = adw::HeaderBar::new();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));
        toolbar.add_bottom_bar(&status_row);

        // No `.transient_for()` — cage (the minimal wlroots kiosk
        // compositor used on the Raspberry Pi setup) has limited xdg_shell
        // support and never mapped this window at all while it declared a
        // parent — confirmed live, Settings (which has never set a parent)
        // showed fine on the same Pi/cage setup where this didn't.
        // 768 used to be roomier than the GEQ grid actually needed once
        // its column widths were tightened (real Pango-measured label
        // widths instead of a guessed per-character estimate, plus a 10%
        // `column_spacing` trim) — a plain `default_width` hint doesn't
        // cap growth if content still needs more, but it was opening
        // wider than necessary before this shrank the GEQ's own natural
        // width back down. "modern-bg-window" is a plain CSS
        // background-image gradient under RustyWiiM Modern (modern.css) —
        // a real `ArtBackground` + `Overlay` (the main window's own
        // approach) was tried first and reverted: making a bare
        // `gtk::Overlay` the window's *direct* content, instead of
        // `adw::ToolbarView`, lost the window's rounded corners and its
        // content-driven minimum size, and (once `ArtBackground` hides
        // itself outside Modern) left a genuinely transparent hole clean
        // through to the desktop under System/Dark themes — libadwaita
        // windows use an alpha-capable surface for their CSD shadows/
        // rounded corners, so an unpainted region really is a hole, not
        // just a visual gap (confirmed live; same root cause `344e9ca`
        // "Fix transparent background with modern theme" already hit for
        // `ArtBackground` itself). A plain CSS gradient keeps this
        // window's structure/content exactly as it was otherwise.
        let window = adw::Window::builder()
            .title("Equalizer")
            .content(&toolbar)
            .default_width(700)
            .default_height(560)
            .build();
        // `.add_css_class()` *after* construction, not `.css_classes([...])`
        // in the builder above — the builder property calls
        // `gtk_widget_set_css_classes()`, which *replaces* the widget's
        // whole class list rather than adding to it, wiping out the
        // `background`/`csd` classes GTK/libadwaita themselves set up
        // during construction. Without them, `window.csd`'s own
        // `border-radius: var(--window-radius)` rule (the only thing that
        // ever rounds a window) never matched at all — confirmed live via
        // GTK Inspector (the window's own class list really was just
        // `modern-bg-window`, nothing else; manually adding `background`/
        // `csd` back in Inspector fixed the rounding immediately). Same
        // class of bug the device window's own `.add_css_class("player-window")`
        // (also post-construction) already avoided by construction.
        window.add_css_class("modern-bg-window");

        let inner = Rc::new(Inner {
            ds: ds.clone(),
            icons: Rc::clone(icons),
            stack,
            graphic_view,
            parametric_view,
            mechanism_toggle,
            source_picker,
            channel_toggle,
            channel_picker,
            preset_picker,
            save_btn,
            reset_btn,
            source_cluster,
            preset_cluster,
            status_label,
            spinner,
            session: RefCell::new(None),
            overview: RefCell::new(Vec::new()),
            current_target: RefCell::new(None),
            current_kind: RefCell::new(None),
            current_lr: Cell::new(false),
            current_channel: Cell::new(Channel::Stereo),
            last_full_state: RefCell::new(None),
            mech_cache: RefCell::new(Vec::new()),
            preset_cache: RefCell::new(Vec::new()),
            dirty_kinds: RefCell::new(Vec::new()),
            dirty_bands: RefCell::new(BTreeSet::new()),
            flush_timer: RefCell::new(None),
            write_in_flight: Cell::new(false),
        });

        let panel = Self { inner: inner.clone() };
        panel.wire_signals();
        window.present();
        panel.open();

        // Keep the panel (and everything it owns) alive for as long as
        // the window exists — nothing else holds a strong ref to `inner`
        // once `present()` returns, since `EqPanel` itself isn't stored
        // anywhere. No device-side teardown needed on close: this panel
        // reads state once on open and edits from there, it never
        // subscribes to or polls the device, so there's nothing to
        // unsubscribe from.
        window.connect_close_request(move |_| {
            let _keep_alive = &panel;
            glib::Propagation::Proceed
        });
        window
    }

    fn wire_signals(&self) {
        let inner = &self.inner;

        inner.mechanism_toggle.connect_mechanism_selected({
            let inner = inner.clone();
            move |_, token| on_mechanism_selected(&inner, &token)
        });
        inner.source_picker.connect_source_selected({
            let inner = inner.clone();
            move |_, token| on_source_selected(&inner, token)
        });
        inner.channel_toggle.connect_channel_mode_toggled({
            let inner = inner.clone();
            move |_, lr| on_channel_mode_toggled(&inner, lr)
        });
        inner.channel_picker.connect_channel_selected({
            let inner = inner.clone();
            move |_, token| on_channel_selected(&inner, &token)
        });
        inner.preset_picker.connect_preset_selected({
            let inner = inner.clone();
            move |_, name| on_preset_selected(&inner, name)
        });
        inner.preset_picker.connect_preset_rename_requested({
            let inner = inner.clone();
            move |_, old, new| on_preset_rename_requested(&inner, old, new)
        });
        inner.preset_picker.connect_preset_delete_requested({
            let inner = inner.clone();
            move |_, name| on_preset_delete_requested(&inner, name)
        });
        inner.graphic_view.connect_band_changed({
            let inner = inner.clone();
            move |_, idx| on_band_changed(&inner, idx)
        });
        inner.parametric_view.connect_band_changed({
            let inner = inner.clone();
            move |_, idx| on_band_changed(&inner, idx)
        });
        inner.reset_btn.connect_clicked({
            let inner = inner.clone();
            move |_| on_reset_clicked(&inner)
        });
        inner.save_btn.connect_clicked({
            let inner = inner.clone();
            move |_| {
                let inner = inner.clone();
                crate::ui::eq::chrome::show_preset_name_prompt(
                    "Save Preset", "Preset name:", "", "Save",
                    move |name| on_preset_save_requested(&inner, name),
                );
            }
        });
    }

    /// Resolves `EqProfile` if needed (lazily, on first panel open —
    /// never at connect time, since most opens of a device window never
    /// touch the EQ editor at all), then loads the overview.
    /// `DeviceState::eq_profile()` caches the result for the connection's
    /// lifetime, so this only
    /// ever does real probing once per connection, on whichever panel
    /// open happens to be first.
    fn open(&self) {
        let inner = self.inner.clone();
        if inner.ds.eq_profile().is_some() {
            eq_dbg(&inner.ds.ip(), "panel: profile already cached on this connection");
            *inner.session.borrow_mut() = inner.ds.eq_session();
            load_overview(&inner);
            return;
        }
        if inner.ds.eq_unavailable() {
            inner.status_label.set_label("This device has no EQ.");
            return;
        }

        eq_dbg(&inner.ds.ip(), "panel: resolving EQ profile (first open on this connection)");
        inner.status_label.set_label("Checking EQ capability…");
        inner.spinner.set_spinning(true);

        let Some(client) = inner.ds.client() else { return };
        let Some(caps) = inner.ds.capabilities() else { return };
        let vendor = caps.vendor;
        run_async(&inner, move || {
            let client = client.clone();
            async move { crate::device::capabilities::resolve_eq_profile(&client, vendor).await }
        }, |inner, profile| {
            inner.spinner.set_spinning(false);
            match profile {
                Some(p) => {
                    inner.ds.store_eq_profile(Some(std::sync::Arc::new(p)));
                    *inner.session.borrow_mut() = inner.ds.eq_session();
                    inner.status_label.set_label("");
                    load_overview(inner);
                }
                None => {
                    inner.ds.store_eq_profile(None);
                    inner.status_label.set_label("This device has no EQ.");
                }
            }
        });
    }
}

fn load_overview(inner: &Rc<Inner>) {
    let Some(session) = inner.session.borrow().clone() else { return };
    run_async(inner, move || {
        let session = session.clone();
        async move { session.get_overview().await }
    }, |inner, result| {
        let Ok(overview) = result else {
            inner.status_label.set_label("Couldn't read EQ state.");
            return;
        };
        *inner.overview.borrow_mut() = overview.clone();

        let per_source = inner.session.borrow().as_ref()
            .is_some_and(|s| s.profile().layers.iter()
                .any(|l| l.kind == EqLayerKind::Source && matches!(l.scope, crate::device::capabilities::EqScope::PerSource)));
        inner.source_cluster.set_visible(per_source);
        if per_source {
            let sources: Vec<String> = overview.iter()
                .filter_map(|t| match &t.target { EqTarget::Source(s) => Some(s.clone()), _ => None })
                .collect();
            let inner2 = Rc::clone(inner);
            inner.source_picker.set_sources(&sources, move |id| source_display(&inner2, id));
        }

        let default = overview.first().cloned();
        if let Some(t) = default {
            select_target(inner, t.target.clone(), t);
        }
    });
}

fn select_target(inner: &Rc<Inner>, target: EqTarget, overview_entry: TargetOverview) {
    *inner.current_target.borrow_mut() = Some(target.clone());
    // `None` when the target is actually Off — this used to be set
    // unconditionally to `Some(overview_entry.kind)` even while off (the
    // device still remembers its last-active plugin, see
    // `TargetOverview`'s own doc comment), which was the actual root
    // cause of the reported bug: `on_channel_mode_toggled()`'s
    // `let Some(kind) = *inner.current_kind.borrow() else { return }`
    // guard always passed even with EQ genuinely Off, letting a channel-
    // mode command through and then optimistically showing a curve.
    *inner.current_kind.borrow_mut() = if overview_entry.enabled { Some(overview_entry.kind) } else { None };
    inner.current_lr.set(overview_entry.lr);
    inner.current_channel.set(Channel::Stereo);
    inner.channel_toggle.set_active(overview_entry.lr);
    // Stereo/L-R only means something once a mechanism is actually
    // active *and* that mechanism actually offers it (GEQ never does —
    // see `mechanism_supports_lr()`'s own doc comment) — hidden
    // otherwise, rather than present-but-a-silent-no-op.
    inner.channel_toggle.set_visible(overview_entry.enabled && mechanism_supports_lr(inner, overview_entry.kind));
    inner.channel_picker.set_visible(overview_entry.enabled && overview_entry.lr);
    inner.channel_picker.set_selected(false);

    if let EqTarget::Source(s) = &target {
        let (label, icon) = source_display(inner, s);
        inner.source_picker.set_current(&label, &icon);
    }

    inner.mech_cache.borrow_mut().clear();
    inner.preset_cache.borrow_mut().clear();
    inner.dirty_kinds.borrow_mut().clear();

    let mechanism_kinds: Vec<EqKind> = inner.session.borrow().as_ref()
        .and_then(|s| s.profile().layers.iter().find(|l| l.kind == EqLayerKind::Source))
        .map(|l| l.mechanisms.iter().map(|m| m.kind).collect())
        .unwrap_or_default();
    let mechanism_tokens: Vec<&str> = mechanism_kinds.iter().map(|k| kind_token(*k)).collect();
    inner.mechanism_toggle.set_mechanisms(&mechanism_tokens);
    inner.mechanism_toggle.set_selected(if overview_entry.enabled { kind_token(overview_entry.kind) } else { "off" });

    if mechanism_kinds.is_empty() {
        inner.stack.set_visible_child_name("off");
        inner.preset_cluster.set_visible(false);
        return;
    }
    prefetch_target(inner, target, mechanism_kinds, overview_entry);
}

/// Reads every mechanism's `EqState` for `target` up front, sequentially
/// (never parallel — this codebase's HTTP rule), so that once loaded,
/// clicking between Off/GEQ/PEQ is instant from `mech_cache` instead of
/// paying a fresh round trip per click (the "too much lag" testing
/// feedback). A brief spinner covers this one batch of reads; nothing
/// else in the panel blocks on network I/O this way.
fn prefetch_target(inner: &Rc<Inner>, target: EqTarget, mechanisms: Vec<EqKind>, entry: TargetOverview) {
    let Some(session) = inner.session.borrow().clone() else { return };
    eq_dbg(&inner.ds.ip(), &format!("prefetching {} mechanism(s) for target", mechanisms.len()));
    inner.status_label.set_label("Loading…");
    inner.spinner.set_spinning(true);
    run_async(inner, move || {
        let session = session.clone();
        let target = target.clone();
        async move {
            let mut results = Vec::new();
            for kind in mechanisms {
                let r = session.get_eq_state(&target, kind).await;
                results.push((kind, r));
            }
            results
        }
    }, move |inner, results| {
        inner.status_label.set_label("");
        inner.spinner.set_spinning(false);
        let mut cache = Vec::new();
        for (kind, r) in results {
            match r {
                Ok(state) => {
                    mark_clean(inner, kind);
                    cache.push((kind, state));
                }
                Err(e) => eq_dbg(&inner.ds.ip(), &format!("prefetch get_eq_state({kind:?}) failed: {e}")),
            }
        }
        *inner.mech_cache.borrow_mut() = cache;
        if entry.enabled {
            show_cached_or_fetch(inner, entry.kind);
            refresh_preset_picker(inner);
        } else {
            inner.stack.set_visible_child_name("off");
            inner.preset_cluster.set_visible(false);
        }
    });
}

/// Instant display path: show from `mech_cache` if we have it, else fall
/// back to a fresh fetch (e.g. that one mechanism failed during prefetch).
fn show_cached_or_fetch(inner: &Rc<Inner>, kind: EqKind) {
    // The lookup is pulled into its own `let` first, not inlined into the
    // `if let`'s scrutinee — see `on_channel_selected()`'s doc comment on
    // why an inlined `.borrow()` there stays alive for the whole `if let`
    // body and panicked once `show_state()` tried to borrow the same
    // `RefCell` again. `show_state()` doesn't touch `mech_cache`, so
    // inlining here wouldn't currently panic, but keeping the same
    // defensive shape avoids relying on that staying true.
    let cached = cache_get_state(&inner.mech_cache.borrow(), kind);
    if let Some(state) = cached {
        show_state(inner, kind, &state);
        return;
    }
    let Some(target) = inner.current_target.borrow().clone() else { return };
    fetch_and_show(inner, target, kind);
}

fn fetch_and_show(inner: &Rc<Inner>, target: EqTarget, kind: EqKind) {
    let Some(session) = inner.session.borrow().clone() else { return };
    run_async(inner, move || {
        let session = session.clone();
        let target = target.clone();
        async move { session.get_eq_state(&target, kind).await }
    }, move |inner, result| {
        let Ok(state) = result else {
            eq_dbg(&inner.ds.ip(), &format!("fetch_and_show: get_eq_state({kind:?}) failed"));
            inner.status_label.set_label("Couldn't read EQ state.");
            return;
        };
        cache_put_state(&mut inner.mech_cache.borrow_mut(), kind, state.clone());
        mark_clean(inner, kind);
        show_state(inner, kind, &state);
    });
}

/// Re-fetches ground truth (`get_overview()`) after any action that could
/// have changed the device's actual EQ state (mechanism switch, disable,
/// channel-mode toggle) and reconciles every selector widget to match —
/// fixes the bug where toggling L/R while EQ was Off silently displayed a
/// curve as if enabling it had worked, because the old code assumed the
/// command's effect instead of confirming it.
fn resync_after_action(inner: &Rc<Inner>) {
    let Some(session) = inner.session.borrow().clone() else { return };
    run_async(inner, move || {
        let session = session.clone();
        async move { session.get_overview().await }
    }, |inner, result| {
        let Ok(overview) = result else {
            eq_dbg(&inner.ds.ip(), "resync_after_action: get_overview failed");
            return;
        };
        *inner.overview.borrow_mut() = overview.clone();
        let Some(target) = inner.current_target.borrow().clone() else { return };
        let Some(entry) = overview.iter().find(|t| t.target == target).cloned() else { return };
        apply_overview_entry(inner, entry);
    });
}

fn apply_overview_entry(inner: &Rc<Inner>, entry: TargetOverview) {
    eq_dbg(&inner.ds.ip(), &format!("resync: enabled={} kind={:?} lr={}", entry.enabled, entry.kind, entry.lr));
    inner.current_lr.set(entry.lr);
    inner.channel_toggle.set_active(entry.lr);
    inner.channel_toggle.set_visible(entry.enabled && mechanism_supports_lr(inner, entry.kind));
    inner.channel_picker.set_visible(entry.enabled && entry.lr);
    inner.mechanism_toggle.set_selected(if entry.enabled { kind_token(entry.kind) } else { "off" });
    *inner.current_kind.borrow_mut() = if entry.enabled { Some(entry.kind) } else { None };

    if !entry.enabled {
        inner.stack.set_visible_child_name("off");
        inner.preset_cluster.set_visible(false);
        return;
    }

    // Ground-truth confirmation: one fresh read of this mechanism's real
    // state — cheap (a single call), and only ever done right after a
    // mutating action, never during plain cached tab-switching.
    let kind = entry.kind;
    let Some(session) = inner.session.borrow().clone() else { return };
    let Some(target) = inner.current_target.borrow().clone() else { return };
    run_async(inner, move || {
        let session = session.clone();
        let target = target.clone();
        async move { session.get_eq_state(&target, kind).await }
    }, move |inner, result| {
        match result {
            Ok(state) => {
                cache_put_state(&mut inner.mech_cache.borrow_mut(), kind, state.clone());
                mark_clean(inner, kind);
                show_state(inner, kind, &state);
            }
            Err(e) => eq_dbg(&inner.ds.ip(), &format!("resync get_eq_state({kind:?}) failed: {e}")),
        }
    });
    refresh_preset_picker(inner);
}

/// Shows the preset picker for the current mechanism, from cache when
/// available, else fetching (and caching) it. Presets are device-wide per
/// mechanism, not per target (see `EqSession::list_presets()`), so this
/// doesn't need to re-fetch on every target switch — only the first time
/// a given mechanism's presets are needed.
fn refresh_preset_picker(inner: &Rc<Inner>) {
    let Some(kind) = *inner.current_kind.borrow() else {
        inner.preset_cluster.set_visible(false);
        return;
    };
    if kind == EqKind::ToneControl {
        inner.preset_cluster.set_visible(false);
        return;
    }
    // See `show_cached_or_fetch()`'s comment: pulled into its own `let`
    // rather than inlined into the `if let`, on the same defensive
    // principle (this body doesn't currently re-borrow `preset_cache`
    // either, but there's no reason to rely on that).
    let active = cache_get_state(&inner.mech_cache.borrow(), kind).and_then(|s| eq_state_active_preset(&s));
    let dirty = is_dirty(inner, kind);
    let cached = cache_get_presets(&inner.preset_cache.borrow(), kind);
    if let Some(list) = cached {
        inner.preset_picker.set_presets(&list.hardwired, &list.custom, active.as_deref(), dirty);
        update_save_btn(inner, active.as_deref(), dirty);
        inner.preset_cluster.set_visible(true);
        return;
    }
    let Some(session) = inner.session.borrow().clone() else { return };
    run_async(inner, move || {
        let session = session.clone();
        async move { session.list_presets(kind).await }
    }, move |inner, result| {
        // Only apply if the mechanism is still current — the user may
        // have switched away while this was in flight.
        if *inner.current_kind.borrow() != Some(kind) { return; }
        match result {
            Ok(list) => {
                let active = cache_get_state(&inner.mech_cache.borrow(), kind).and_then(|s| eq_state_active_preset(&s));
                let dirty = is_dirty(inner, kind);
                inner.preset_picker.set_presets(&list.hardwired, &list.custom, active.as_deref(), dirty);
                update_save_btn(inner, active.as_deref(), dirty);
                cache_put_presets(&mut inner.preset_cache.borrow_mut(), kind, list);
                inner.preset_cluster.set_visible(true);
            }
            Err(e) => {
                eq_dbg(&inner.ds.ip(), &format!("list_presets({kind:?}) failed: {e}"));
                inner.preset_cluster.set_visible(false);
            }
        }
    });
}

fn show_state(inner: &Rc<Inner>, kind: EqKind, state: &EqState) {
    let filters: Vec<crate::device::eq::PeqBandMode> = inner.session.borrow().as_ref()
        .and_then(|s| s.profile().layers.iter().find(|l| l.kind == EqLayerKind::Source))
        .and_then(|l| l.mechanisms.iter().find(|m| m.kind == EqKind::Parametric))
        .map(|m| m.filters.clone())
        .unwrap_or_default();

    *inner.last_full_state.borrow_mut() = Some(state.clone());
    let selected = select_channel_state(state, inner.current_channel.get());
    match kind {
        EqKind::Graphic => {
            inner.graphic_view.set_state(&selected);
            inner.stack.set_visible_child_name("graphic");
        }
        EqKind::Parametric => {
            inner.parametric_view.set_filters(&filters);
            inner.parametric_view.set_state(&selected);
            inner.stack.set_visible_child_name("parametric");
        }
        EqKind::ToneControl => {}
    }
    // Idempotent either way (cache-hit tab switch or a genuine fresh
    // fetch) — always reflects whatever `mech_cache`/`dirty_kinds`
    // currently say for `kind`, never resets either on its own.
    update_preset_button(inner, kind);
}

/// Flattens whichever channel is currently selected (Stereo, or the
/// `EqChannelPicker`'s Left/Right choice) into a `Stereo`-shaped
/// `EqState` — the one thing `GraphicEqView`/`ParametricEqView` ever see
/// (see their own doc comments: they never handle `LeftRight` directly).
fn select_channel_state(state: &EqState, channel: Channel) -> EqState {
    // Real bug, confirmed live (2026-07-23): a stray extra match arm here
    // used to catch `Channel::Right` *before* the real Left-vs-Right
    // branch below ever ran, so selecting "Right" always displayed the
    // Left channel's bands (mislabeled as Right) — editing while "Right"
    // was selected then wrote those edits into the device's real Right
    // slot, but starting from Left's values rather than Right's own.
    fn pick<B: Clone>(bands: &ChannelBands<B>, channel: Channel) -> Vec<B> {
        match bands {
            ChannelBands::Stereo(v) => v.clone(),
            ChannelBands::LeftRight { left, right } =>
                if channel == Channel::Right { right.clone() } else { left.clone() },
        }
    }
    match state {
        EqState::Off => EqState::Off,
        EqState::Graphic { bands, active_preset } =>
            EqState::Graphic { bands: ChannelBands::Stereo(pick(bands, channel)), active_preset: active_preset.clone() },
        EqState::Parametric { bands, active_preset } =>
            EqState::Parametric { bands: ChannelBands::Stereo(pick(bands, channel)), active_preset: active_preset.clone() },
    }
}

fn on_source_selected(inner: &Rc<Inner>, token: String) {
    let Some(entry) = inner.overview.borrow().iter()
        .find(|t| t.target == EqTarget::Source(token.clone())).cloned() else { return };
    select_target(inner, EqTarget::Source(token), entry);
}

fn on_mechanism_selected(inner: &Rc<Inner>, token: &str) {
    let Some(target) = inner.current_target.borrow().clone() else { return };
    let Some(session) = inner.session.borrow().clone() else { return };
    let previous_kind = *inner.current_kind.borrow();
    eq_dbg(&inner.ds.ip(), &format!("panel: mechanism selected: {token}"));

    match token {
        "off" => {
            let Some(current) = previous_kind else { return };
            inner.stack.set_visible_child_name("off");
            inner.preset_cluster.set_visible(false);
            inner.channel_toggle.set_visible(false);
            inner.channel_picker.set_visible(false);
            let target2 = target.clone();
            run_async(inner, move || {
                let session = session.clone();
                let target = target2.clone();
                async move { session.disable(&target, current).await }
            }, |inner, result| {
                if let Err(e) = &result { eq_dbg(&inner.ds.ip(), &format!("disable() failed: {e}")); }
                resync_after_action(inner);
            });
        }
        "graphic" | "parametric" => {
            let kind = if token == "graphic" { EqKind::Graphic } else { EqKind::Parametric };
            *inner.current_kind.borrow_mut() = Some(kind);
            inner.channel_toggle.set_visible(mechanism_supports_lr(inner, kind));
            // Instant from the target-switch prefetch cache; the
            // ground-truth resync below corrects this afterward if the
            // device didn't actually end up in this state.
            show_cached_or_fetch(inner, kind);
            refresh_preset_picker(inner);
            let target2 = target.clone();
            run_async(inner, move || {
                let session = session.clone();
                let target = target2.clone();
                async move { session.enable_mechanism(&target, kind).await }
            }, move |inner, result| {
                if let Err(e) = &result { eq_dbg(&inner.ds.ip(), &format!("enable_mechanism({kind:?}) failed: {e}")); }
                resync_after_action(inner);
            });
        }
        _ => {}
    }
}

fn on_channel_mode_toggled(inner: &Rc<Inner>, lr: bool) {
    let Some(target) = inner.current_target.borrow().clone() else { return };
    let Some(kind) = *inner.current_kind.borrow() else { return };
    let Some(session) = inner.session.borrow().clone() else { return };
    eq_dbg(&inner.ds.ip(), &format!("panel: channel mode toggled lr={lr}"));
    inner.channel_picker.set_visible(lr);
    inner.channel_picker.set_selected(false);
    inner.current_channel.set(Channel::Stereo);

    run_async(inner, move || {
        let session = session.clone();
        let target = target.clone();
        async move { session.set_channel_mode(&target, kind, lr).await }
    }, move |inner, result| {
        if let Err(e) = &result { eq_dbg(&inner.ds.ip(), &format!("set_channel_mode failed: {e}")); }
        // Ground-truth resync, not an optimistic `fetch_and_show()` — see
        // `apply_overview_entry()`'s doc comment: this is exactly the fix
        // for the reported bug (toggling L/R while Off silently looked
        // like it had turned EQ on).
        resync_after_action(inner);
    });
}

fn on_channel_selected(inner: &Rc<Inner>, token: &str) {
    inner.current_channel.set(if token == "right" { Channel::Right } else { Channel::Left });
    let Some(kind) = *inner.current_kind.borrow() else { return };
    // Already-in-hand data (this session's own last full-state fetch) —
    // no network fetch, just re-render the other channel from it.
    //
    // The clone is pulled out into its own `let` first, not inlined into
    // the `if let`'s scrutinee: a temporary `Ref` guard created inside an
    // `if let` condition lives for the whole `if let` body, not just the
    // condition — inlined, `show_state()`'s own `last_full_state.borrow_mut()`
    // a few lines down would panic with "already borrowed" against the
    // still-live outer guard (confirmed live — this crashed on every
    // Left/Right click).
    let last_state = inner.last_full_state.borrow().clone();
    if let Some(state) = last_state {
        show_state(inner, kind, &state);
    }
}

fn on_preset_selected(inner: &Rc<Inner>, name: String) {
    let Some(target) = inner.current_target.borrow().clone() else { return };
    let Some(kind) = *inner.current_kind.borrow() else { return };
    let Some(session) = inner.session.borrow().clone() else { return };
    eq_dbg(&inner.ds.ip(), &format!("panel: loading preset {name:?} for {kind:?}"));
    inner.status_label.set_label("Loading preset…");
    run_async(inner, move || {
        let session = session.clone();
        let target = target.clone();
        let name = name.clone();
        async move { session.load_preset(&target, kind, &name).await }
    }, move |inner, result| {
        inner.status_label.set_label("");
        match result {
            Ok(state) => {
                cache_put_state(&mut inner.mech_cache.borrow_mut(), kind, state.clone());
                mark_clean(inner, kind);
                show_state(inner, kind, &state);
            }
            Err(e) => {
                eq_dbg(&inner.ds.ip(), &format!("load_preset failed: {e}"));
                inner.status_label.set_label("Couldn't load preset.");
            }
        }
    });
}

fn on_preset_save_requested(inner: &Rc<Inner>, name: String) {
    if name.trim().is_empty() { return; }
    let Some(target) = inner.current_target.borrow().clone() else { return };
    let Some(kind) = *inner.current_kind.borrow() else { return };
    let Some(session) = inner.session.borrow().clone() else { return };
    eq_dbg(&inner.ds.ip(), &format!("panel: saving preset {name:?} for {kind:?}"));
    let name2 = name.clone();
    run_async(inner, move || {
        let session = session.clone();
        let target = target.clone();
        let name = name2.clone();
        async move { session.save_preset(&target, kind, &name).await }
    }, move |inner, result| {
        if let Err(e) = &result {
            eq_dbg(&inner.ds.ip(), &format!("save_preset failed: {e}"));
            inner.status_label.set_label("Couldn't save preset.");
            return;
        }
        // Saving names the *current* (just-edited) state, so it's no
        // longer "Custom" — it now matches the newly-saved preset.
        mark_clean(inner, kind);
        // Split from the `if let` itself: `.borrow()`'s temporary `Ref`
        // guard is lifetime-extended across the whole `if let` block when
        // it's inline in the scrutinee, so a `.borrow_mut()` further down
        // in the same block panics ("RefCell already borrowed") even
        // though `cache_get_state` already returned an owned `EqState` by
        // that point — confirmed live via bug2.txt.
        let state = cache_get_state(&inner.mech_cache.borrow(), kind);
        if let Some(state) = state {
            let renamed = match state {
                EqState::Graphic { bands, .. } => EqState::Graphic { bands, active_preset: Some(name.clone()) },
                EqState::Parametric { bands, .. } => EqState::Parametric { bands, active_preset: Some(name.clone()) },
                EqState::Off => EqState::Off,
            };
            cache_put_state(&mut inner.mech_cache.borrow_mut(), kind, renamed);
        }
        // Insert the new name into the cached list directly, rather than
        // dropping the cache and re-fetching `list_presets()` from the
        // device: confirmed live, the device doesn't reflect a just-saved
        // preset in that list immediately (same settle-time class of
        // issue `load_preset()` already works around with its own
        // `PRESET_SETTLE` sleep before re-reading) — a re-fetch here
        // raced that and won every time, showing the stale list until the
        // window was closed and reopened. We already know exactly what
        // we asked the device to save, so there's nothing to actually
        // wait on.
        let mut list = cache_get_presets(&inner.preset_cache.borrow(), kind).unwrap_or_default();
        if !list.custom.contains(&name) { list.custom.push(name.clone()); }
        cache_put_presets(&mut inner.preset_cache.borrow_mut(), kind, list.clone());
        inner.preset_picker.set_presets(&list.hardwired, &list.custom, Some(&name), false);
        update_save_btn(inner, Some(&name), false);
        inner.preset_cluster.set_visible(true);
    });
}

fn on_preset_rename_requested(inner: &Rc<Inner>, old_name: String, new_name: String) {
    let Some(kind) = *inner.current_kind.borrow() else { return };
    let Some(session) = inner.session.borrow().clone() else { return };
    eq_dbg(&inner.ds.ip(), &format!("panel: renaming preset {old_name:?} -> {new_name:?} for {kind:?}"));
    run_async(inner, move || {
        let session = session.clone();
        let old_name = old_name.clone();
        let new_name = new_name.clone();
        async move {
            let result = session.rename_preset(kind, &old_name, &new_name).await;
            (new_name, result)
        }
    }, move |inner, (new_name, result)| {
        if let Err(e) = &result {
            eq_dbg(&inner.ds.ip(), &format!("rename_preset failed: {e}"));
            inner.status_label.set_label("Couldn't rename preset.");
            return;
        }
        // If the renamed preset was the active one, follow the rename so
        // the button/checkmark still point at the (now-renamed) entry.
        // Same `if let` temporary-extension gotcha as `on_preset_save_requested`
        // above — bind the owned `EqState` first, then drop the `.borrow()`
        // before any `.borrow_mut()` further down.
        let state = cache_get_state(&inner.mech_cache.borrow(), kind);
        if let Some(state) = state {
            if eq_state_active_preset(&state).is_some() && !is_dirty(inner, kind) {
                let renamed = match state {
                    EqState::Graphic { bands, .. } => EqState::Graphic { bands, active_preset: Some(new_name) },
                    EqState::Parametric { bands, .. } => EqState::Parametric { bands, active_preset: Some(new_name) },
                    EqState::Off => EqState::Off,
                };
                cache_put_state(&mut inner.mech_cache.borrow_mut(), kind, renamed);
            }
        }
        inner.preset_cache.borrow_mut().retain(|(k, _)| *k != kind);
        refresh_preset_picker(inner);
    });
}

fn on_preset_delete_requested(inner: &Rc<Inner>, name: String) {
    let Some(kind) = *inner.current_kind.borrow() else { return };
    let Some(session) = inner.session.borrow().clone() else { return };
    eq_dbg(&inner.ds.ip(), &format!("panel: deleting preset {name:?} for {kind:?}"));
    let name2 = name.clone();
    run_async(inner, move || {
        let session = session.clone();
        let name = name2.clone();
        async move { session.delete_preset(kind, &name).await }
    }, move |inner, result| {
        if let Err(e) = &result {
            eq_dbg(&inner.ds.ip(), &format!("delete_preset failed: {e}"));
            inner.status_label.set_label("Couldn't delete preset.");
            return;
        }
        inner.preset_cache.borrow_mut().retain(|(k, _)| *k != kind);
        // If the deleted preset was the currently-active one, the button
        // must fall back to "Custom" — it no longer names anything real.
        // Same `if let` temporary-extension gotcha as the save/rename
        // handlers above: bind the owned `EqState` first, then drop the
        // `.borrow()` before `.borrow_mut()` further down.
        let state = cache_get_state(&inner.mech_cache.borrow(), kind);
        if let Some(state) = state {
            if eq_state_active_preset(&state).as_deref() == Some(name.as_str()) {
                let cleared = match state {
                    EqState::Graphic { bands, .. } => EqState::Graphic { bands, active_preset: None },
                    EqState::Parametric { bands, .. } => EqState::Parametric { bands, active_preset: None },
                    EqState::Off => EqState::Off,
                };
                cache_put_state(&mut inner.mech_cache.borrow_mut(), kind, cleared);
            }
        }
        refresh_preset_picker(inner);
    });
}

/// Resets every band of the *currently shown* channel/mechanism to flat
/// — GEQ: every gain back to 0dB; PEQ: every mode back to Peak, gain
/// 0dB, Q 0.25, and frequency back to that band's default center
/// (`default_peq_freq_hz()` — see its own doc comment on where those
/// values come from). Applied straight to the currently-displayed view
/// (whichever channel that happens to be, in L/R mode) and then run
/// through the exact same `on_band_changed()` path a real slider drag
/// would take, one call per band — this is what marks the mechanism
/// dirty (a reset is itself an edit, per the same "Custom" convention as
/// any other change) and enqueues the debounced write, rather than
/// needing its own separate write path.
fn on_reset_clicked(inner: &Rc<Inner>) {
    let Some(kind) = *inner.current_kind.borrow() else { return };
    let count = match kind {
        EqKind::Graphic => {
            let EqState::Graphic { bands: ChannelBands::Stereo(mut bands), active_preset } = inner.graphic_view.state() else { return };
            for band in &mut bands { band.gain_db = 0.0; }
            let count = bands.len();
            inner.graphic_view.set_state(&EqState::Graphic { bands: ChannelBands::Stereo(bands), active_preset });
            count
        }
        EqKind::Parametric => {
            let EqState::Parametric { bands: ChannelBands::Stereo(mut bands), active_preset } = inner.parametric_view.state() else { return };
            for (i, band) in bands.iter_mut().enumerate() {
                band.mode = PeqBandMode::Peak;
                band.gain_db = 0.0;
                band.q = 0.25;
                band.freq_hz = default_peq_freq_hz(i);
            }
            let count = bands.len();
            inner.parametric_view.set_state(&EqState::Parametric { bands: ChannelBands::Stereo(bands), active_preset });
            count
        }
        EqKind::ToneControl => return,
    };
    eq_dbg(&inner.ds.ip(), &format!("panel: reset {kind:?} to flat ({count} bands)"));
    for idx in 0..count {
        on_band_changed(inner, idx as u32);
    }
}

/// Debounce: coalesce band indices touched within `FLUSH_DELAY` of each
/// other into one write; at most one write in flight at a time (per the
/// codebase's sequential-HTTP rule), with later edits queued behind it.
fn on_band_changed(inner: &Rc<Inner>, idx: u32) {
    inner.dirty_bands.borrow_mut().insert(idx as usize);
    if let Some(kind) = *inner.current_kind.borrow() {
        mark_dirty(inner, kind);
        update_preset_button(inner, kind);
    }
    if inner.flush_timer.borrow().is_some() { return; }
    let inner2 = inner.clone();
    let id = glib::timeout_add_local_once(FLUSH_DELAY, move || {
        *inner2.flush_timer.borrow_mut() = None;
        flush_writes(&inner2);
    });
    *inner.flush_timer.borrow_mut() = Some(id);
}

fn flush_writes(inner: &Rc<Inner>) {
    if inner.write_in_flight.get() {
        // Re-arm; the in-flight completion handler re-checks dirty_bands.
        return;
    }
    let dirty: Vec<usize> = std::mem::take(&mut *inner.dirty_bands.borrow_mut()).into_iter().collect();
    if dirty.is_empty() { return; }
    let Some(target) = inner.current_target.borrow().clone() else { return };
    let Some(kind) = *inner.current_kind.borrow() else { return };
    let Some(session) = inner.session.borrow().clone() else { return };
    let lr = inner.current_lr.get();

    inner.write_in_flight.set(true);

    match (kind, lr) {
        (EqKind::Graphic, false) => {
            let full = inner.graphic_view.state();
            let EqState::Graphic { bands: ChannelBands::Stereo(all), .. } = &full else { return };
            let bands: Vec<(usize, GraphicBand)> = dirty.iter().filter_map(|&i| all.get(i).cloned().map(|b| (i, b))).collect();
            run_async(inner, move || {
                let session = session.clone();
                let target = target.clone();
                let bands = bands.clone();
                async move { session.set_graphic_bands(&target, &bands).await }
            }, move |inner, result| on_write_complete(inner, kind, full, result));
        }
        (EqKind::Parametric, false) => {
            let full = inner.parametric_view.state();
            let EqState::Parametric { bands: ChannelBands::Stereo(all), .. } = &full else { return };
            let bands: Vec<(usize, ParametricBand)> = dirty.iter().filter_map(|&i| all.get(i).cloned().map(|b| (i, b))).collect();
            run_async(inner, move || {
                let session = session.clone();
                let target = target.clone();
                let bands = bands.clone();
                async move { session.set_parametric_bands(&target, &bands).await }
            }, move |inner, result| on_write_complete(inner, kind, full, result));
        }
        (EqKind::Graphic, true) => {
            let EqState::Graphic { bands: ChannelBands::Stereo(edited), active_preset } = inner.graphic_view.state() else {
                inner.write_in_flight.set(false);
                return;
            };
            let Some((other_left, other_right)) = last_lr_graphic_bands(inner) else {
                inner.write_in_flight.set(false);
                return;
            };
            let (left, right) = match inner.current_channel.get() {
                Channel::Right => (other_left, edited),
                _              => (edited, other_right),
            };
            let full = EqState::Graphic { bands: ChannelBands::LeftRight { left: left.clone(), right: right.clone() }, active_preset };
            run_async(inner, move || {
                let session = session.clone();
                let target = target.clone();
                async move { session.set_graphic_bands_lr(&target, &left, &right).await }
            }, move |inner, result| on_write_complete(inner, kind, full, result));
        }
        (EqKind::Parametric, true) => {
            let EqState::Parametric { bands: ChannelBands::Stereo(edited), active_preset } = inner.parametric_view.state() else {
                inner.write_in_flight.set(false);
                return;
            };
            let Some((other_left, other_right)) = last_lr_parametric_bands(inner) else {
                inner.write_in_flight.set(false);
                return;
            };
            let (left, right) = match inner.current_channel.get() {
                Channel::Right => (other_left, edited),
                _              => (edited, other_right),
            };
            let full = EqState::Parametric { bands: ChannelBands::LeftRight { left: left.clone(), right: right.clone() }, active_preset };
            run_async(inner, move || {
                let session = session.clone();
                let target = target.clone();
                async move { session.set_parametric_bands_lr(&target, &left, &right).await }
            }, move |inner, result| on_write_complete(inner, kind, full, result));
        }
        (EqKind::ToneControl, _) => {
            inner.write_in_flight.set(false);
        }
    }
}

/// The other channel's last-fetched bands, for combining with whichever
/// channel was just edited into one full L/R write (see `set_*_bands_lr`'s
/// doc comment on why a partial single-channel write isn't used).
fn last_lr_graphic_bands(inner: &Rc<Inner>) -> Option<(Vec<GraphicBand>, Vec<GraphicBand>)> {
    match inner.last_full_state.borrow().as_ref() {
        Some(EqState::Graphic { bands: ChannelBands::LeftRight { left, right }, .. }) =>
            Some((left.clone(), right.clone())),
        _ => None,
    }
}

fn last_lr_parametric_bands(inner: &Rc<Inner>) -> Option<(Vec<ParametricBand>, Vec<ParametricBand>)> {
    match inner.last_full_state.borrow().as_ref() {
        Some(EqState::Parametric { bands: ChannelBands::LeftRight { left, right }, .. }) =>
            Some((left.clone(), right.clone())),
        _ => None,
    }
}

/// `committed`: the full `EqState` (both channels, if L/R) this write was
/// meant to establish — on success, becomes the new cached/last-known
/// state for `kind`, so switching mechanisms away and back afterward
/// shows the just-written edit rather than a stale pre-edit snapshot from
/// the target-switch prefetch.
fn on_write_complete(inner: &Rc<Inner>, kind: EqKind, committed: EqState, result: anyhow::Result<()>) {
    inner.write_in_flight.set(false);
    if let Err(e) = &result {
        eq_dbg(&inner.ds.ip(), &format!("write failed for {kind:?}: {e}"));
        inner.status_label.set_label("A write failed — Reload to resync.");
    } else {
        *inner.last_full_state.borrow_mut() = Some(committed.clone());
        cache_put_state(&mut inner.mech_cache.borrow_mut(), kind, committed);
    }
    if !inner.dirty_bands.borrow().is_empty() {
        flush_writes(inner);
    }
}

/// Shared async-call bridge: spawn `make_future` on the tokio thread,
/// apply `on_done` on the GTK thread with the result. The one place this
/// module talks to `rt()`/`async_channel`, per the codebase's established
/// tokio↔GTK bridge pattern (network I/O off the GTK thread, results
/// applied back on it via a channel).
fn run_async<T, Fut, MakeFut, F>(inner: &Rc<Inner>, make_future: MakeFut, on_done: F)
where
    T: Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    MakeFut: FnOnce() -> Fut + Send + 'static,
    F: FnOnce(&Rc<Inner>, T) + 'static,
{
    let (tx, rx) = async_channel::bounded(1);
    inner.ds.rt().spawn(async move {
        let result = make_future().await;
        let _ = tx.send(result).await;
    });
    let inner = inner.clone();
    glib::spawn_future_local(async move {
        if let Ok(result) = rx.recv().await {
            on_done(&inner, result);
        }
    });
}
