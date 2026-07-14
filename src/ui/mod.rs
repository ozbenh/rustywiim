#![allow(deprecated)] // glib clone! old-style @strong syntax

mod art_background;
pub mod devlist;
mod flip_cover;
mod icons;
pub(crate) mod menu;
mod scroll_fade_label;
mod playback;
pub(crate) mod settings;
mod views;
mod widgets;

use widgets::*;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use adw::prelude::*;
use glib::clone;
use gtk::gio;
use gtk::{Align, Box as GtkBox, CssProvider, Label, Orientation};

use crate::device::api::TlsMode;
use crate::config;
use crate::config::ThemeMode;
use crate::device::discovery::DiscoveryService;
use crate::device::discovery_manager::{DevicePresence, DiscoveryManager, ManagedEntry, SeedEntry};
use crate::device::manager::DeviceManager;
use crate::device::playback::RepeatMode;
use crate::device::state::{ConnectionState, DeviceState, FullModeGuard, DEBUG_STATE};

/// GApplication ID / icon name / GResource base path / `.desktop` basename —
/// all the same string by freedesktop convention, kept in one place so
/// there's no risk of them drifting apart.
pub const APP_ID: &str = "io.github.ozbenh.rustywiim";

pub static DEBUG_UI: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn dbg_ui(msg: &str) {
    if DEBUG_UI.load(Ordering::Relaxed) {
        println!("[ui] {msg}");
    }
}

/// Set just before the quit action starts closing windows, so the
/// close-request/destroy handlers it triggers (DeviceWindowInner::cleanup())
/// know this isn't a user-initiated close. A window closed because the app
/// is quitting should still be reopened on next launch; a window the user
/// explicitly closed should not.
static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// ── Shared window actions ─────────────────────────────────────────────────────

/// Register `win.about` and `win.settings` on any ApplicationWindow.
/// Both the device window, discovery window, and mini window share these actions.
/// `ds` is `None` for the discovery window (settings window title has no device name).
pub(crate) fn wire_window_actions(
    window:        &impl glib::object::IsA<gtk::ApplicationWindow>,
    ds:            Option<DeviceState>,
    open_settings: Rc<dyn Fn(Option<DeviceState>)>,
) {
    let window = window.upcast_ref::<gtk::ApplicationWindow>().clone();
    let about_action = gio::SimpleAction::new("about", None);
    let win = window.clone();
    about_action.connect_activate(move |_, _| {
        adw::AboutDialog::builder()
            .application_name("RustyWiiM")
            .application_icon(APP_ID)
            .version(concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")"))
            .developer_name("Benjamin Herrenschmidt")
            .copyright("© 2026 Benjamin Herrenschmidt")
            .license_type(gtk::License::MitX11)
            .website("https://github.com/ozbenh/rustywiim")
            .build()
            .present(Some(&win));
    });
    window.add_action(&about_action);

    // Use a WeakRef so the closure does not keep the DeviceState alive after the
    // device window closes.  Upgrading on activation gives the same device (or
    // None if it has already been freed, which opens global settings — harmless).
    let ds_weak: Option<glib::WeakRef<DeviceState>> = ds.as_ref().map(|d| d.downgrade());
    let settings_action = gio::SimpleAction::new("settings", None);
    settings_action.connect_activate(move |_, _| {
        open_settings(ds_weak.as_ref().and_then(|w| w.upgrade()));
    });
    window.add_action(&settings_action);
}

// ── DeviceSpec ────────────────────────────────────────────────────────────────

/// Describes a specific device to connect to when creating a new device window.
pub struct DeviceSpec {
    pub ip:       String,
    pub uuid:     String,
    pub tls_mode: TlsMode,
    /// Whether to actually attempt a connection immediately
    /// (`DeviceManager::get()`'s `try_connect`) — `false` when devlist
    /// already believes this device offline, so opening its window
    /// doesn't repeat an already-known-to-fail attempt; see that
    /// function's doc comment.
    pub try_connect: bool,
}

/// `--connect <scheme://ip[:port]>` override: when set, `AppState::activate()`
/// skips discovery entirely and opens exactly one device window straight at
/// this address (uuid unknown until `getStatusEx` resolves it, same as any
/// freshly-added manual device) — for pointing the app directly at
/// `wiim-simulator` without it needing to be discoverable via SSDP. Must be
/// set (via `set_direct_connect`) before `activate()` runs — in practice,
/// during `main.rs`'s `connect_handle_local_options`.
static DIRECT_CONNECT: std::sync::OnceLock<(String, TlsMode)> = std::sync::OnceLock::new();

pub fn set_direct_connect(ip: String, tls_mode: TlsMode) {
    let _ = DIRECT_CONNECT.set((ip, tls_mode));
}

// ── CSS ───────────────────────────────────────────────────────────────────────

const SYSTEM_CSS: &str = include_str!("../css/system.css");
const DARK_CSS: &str   = include_str!("../css/dark.css");
// RustyWiiM Modern layers its own overrides (card panels, divider styling,
// etc.) on top of the classic dark palette rather than duplicating it.
const MODERN_CSS: &str = concat!(
    include_str!("../css/dark.css"),
    include_str!("../css/modern.css"),
);

thread_local! {
    static THEME_PROVIDER: RefCell<Option<CssProvider>> = const { RefCell::new(None) };
}

fn theme_css(theme: ThemeMode) -> &'static str {
    match theme {
        ThemeMode::RustyWiiM       => DARK_CSS,
        ThemeMode::RustyWiiMModern => MODERN_CSS,
        _                          => SYSTEM_CSS,
    }
}

/// Build the full stylesheet for `theme`: a `@define-color` for the
/// user-configurable accent (named `rustywiim_accent`, not `accent_color` —
/// that name is libadwaita's own accent variable, which system.css deliberately
/// uses as-is to follow the OS accent for the System themes) followed by the
/// theme's own CSS. Defining it unconditionally is harmless for themes that
/// don't reference it (system.css doesn't).
fn build_css(theme: ThemeMode, accent: &str) -> String {
    format!("@define-color rustywiim_accent {accent};\n{}", theme_css(theme))
}

/// Swap the live CSS provider for one loaded from `css`. Replaces the
/// provider object rather than mutating it with `load_from_string` — GTK can
/// miss detecting a rule *removal* from the same provider object (e.g.
/// `window { background-color }` present in dark.css but absent in
/// system.css), leaving computed style caches stale.
fn reload_css_provider(css: &str) {
    let display = gtk::gdk::Display::default().unwrap();
    THEME_PROVIDER.with(|p| {
        let mut borrow = p.borrow_mut();
        if let Some(old) = borrow.take() {
            gtk::style_context_remove_provider_for_display(&display, &old);
        }
        let provider = CssProvider::new();
        provider.load_from_string(css);
        gtk::style_context_add_provider_for_display(
            &display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        *borrow = Some(provider);
    });
}

fn apply_color_scheme(theme: ThemeMode) {
    let scheme = match theme {
        ThemeMode::System          => adw::ColorScheme::Default,
        ThemeMode::SystemLight     => adw::ColorScheme::ForceLight,
        ThemeMode::SystemDark      => adw::ColorScheme::ForceDark,
        ThemeMode::RustyWiiM       => adw::ColorScheme::ForceDark,
        ThemeMode::RustyWiiMModern => adw::ColorScheme::ForceDark,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

/// Initialise the CSS provider for the current process.  Must be called once.
fn init_css(theme: ThemeMode) {
    apply_color_scheme(theme);
    let accent = config::with(|cfg| cfg.accent_color.clone());
    reload_css_provider(&build_css(theme, &accent));
}

/// App-icon GResource bundle, compiled at build time by `build.rs` from
/// `rustywiim.gresource.xml` (`glib-compile-resources`) — embedded directly
/// rather than shipped as a separate file, so the icon is available even
/// for a bare `cargo run`/unpackaged binary with no system icon-theme
/// install. A real packaged installadditionally installs
/// `icons/rustywiim-icon.svg` into the standard hicolor theme
/// path — that copy is what desktop launchers/window switchers resolve via
/// the `.desktop` file's `Icon=` key; this GResource copy is only for
/// in-process lookups (the About dialog, the default window icon) that
/// must work regardless of installation state.
static ICON_RESOURCE_BYTES: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/rustywiim.gresource"));

/// Register the embedded icon resource and point GTK's default icon theme
/// at it, so `application_icon`/`set_default_icon_name` can find `APP_ID`
/// by name. Must be called once, after the GDK display is available.
fn init_icon_resource() {
    let resource = gio::Resource::from_data(&glib::Bytes::from_static(ICON_RESOURCE_BYTES))
        .expect("bad embedded GResource — rustywiim.gresource.xml/build.rs mismatch");
    gio::resources_register(&resource);

    let display = gtk::gdk::Display::default().expect("GDK display not available");
    gtk::IconTheme::for_display(&display)
        .add_resource_path(&format!("/{}/icons", APP_ID.replace('.', "/")));

    gtk::Window::set_default_icon_name(APP_ID);
}

/// Re-apply just the accent colour (no theme switch, no colour-scheme change,
/// no ArtBackground visibility recompute) — for the Settings colour picker,
/// which only ever changes `config.accent_color` while the theme stays put.
pub(crate) fn apply_accent_color() {
    let theme  = config::with(|cfg| cfg.theme);
    let accent = config::with(|cfg| cfg.accent_color.clone());
    reload_css_provider(&build_css(theme, &accent));
    for win in gtk::Window::list_toplevels() {
        queue_draw_recursive(&win);
    }
}

/// Walk the widget tree rooted at `widget` and call `queue_draw()` on every
/// node.  `queue_draw()` on a container does NOT cascade to children in GTK4 —
/// each widget owns its snapshot cache independently, and only the widgets
/// that are individually marked dirty will be re-snapshot'd on the next frame.
fn queue_draw_recursive(widget: &gtk::Widget) {
    widget.queue_draw();
    let mut child = widget.first_child();
    while let Some(c) = child {
        queue_draw_recursive(&c);
        child = c.next_sibling();
    }
}

/// Find every `ArtBackground` in `widget`'s subtree and set its visibility.
/// An invisible widget is skipped entirely by GTK's measure/snapshot passes
/// — not just covered by opaque foreground content — so this is what
/// actually stops the blur rendering from running under any theme but
/// RustyWiiM Modern, rather than merely hiding its (still computed) output.
fn set_art_background_visible(widget: &gtk::Widget, visible: bool) {
    if let Some(bg) = widget.downcast_ref::<art_background::ArtBackground>() {
        bg.set_visible(visible);
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        set_art_background_visible(&c, visible);
        child = c.next_sibling();
    }
}

/// Find every `ScrollFadeLabel` in `widget`'s subtree and set its drop-shadow
/// flag (see `update_art_background_visibility()`, which calls this once per
/// window with a window-appropriate `enabled` value).
fn set_scroll_fade_drop_shadow(widget: &gtk::Widget, enabled: bool) {
    if let Some(label) = widget.downcast_ref::<scroll_fade_label::ScrollFadeLabel>() {
        label.set_drop_shadow(enabled);
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        set_scroll_fade_drop_shadow(&c, enabled);
        child = c.next_sibling();
    }
}

/// Sync every open window's `ArtBackground` visibility (and, for the mini
/// window, a CSS marker class + text drop-shadow) to the current theme +
/// mini_modern setting. Called on theme switch and whenever mini_modern is
/// toggled on its own — the latter doesn't need a full CSS provider reload,
/// so it's split out from `apply_theme()` rather than folded into it.
pub(crate) fn update_art_background_visibility() {
    let theme       = config::with(|cfg| cfg.theme);
    let mini_modern = config::with(|cfg| cfg.mini_modern);
    let modern = theme == ThemeMode::RustyWiiMModern;

    for win in gtk::Window::list_toplevels() {
        let is_mini = win.has_css_class("mini-window");
        let apply = modern && (!is_mini || mini_modern);
        set_art_background_visible(&win, apply);
        if is_mini {
            // modern.css keys mini-window-specific styling (frosted
            // mini-outer, etc.) off this — plain window.mini-window alone
            // can't tell "Modern is active" from "Modern + mini_modern".
            if apply { win.add_css_class("mini-window-modern"); }
            else     { win.remove_css_class("mini-window-modern"); }
        }
        // ScrollFadeLabel (title/artist/album on the main window, title/
        // artist on the mini window) renders manually via GSK and doesn't
        // pick up CSS text-shadow for free, so it needs this instead — only
        // wanted for Modern's blurred background, which is exactly what
        // `apply` already means for a non-mini window (`modern && !is_mini`
        // reduces to `modern`) as well as for the mini window's own
        // Modern-gated case.
        set_scroll_fade_drop_shadow(&win, apply);
    }
}

/// Switch the active CSS theme at runtime.
pub(crate) fn apply_theme(theme: ThemeMode) {
    apply_color_scheme(theme);

    let accent = config::with(|cfg| cfg.accent_color.clone());
    reload_css_provider(&build_css(theme, &accent));

    update_art_background_visibility();

    // Mark every widget in every window dirty so the next frame re-snapshot's
    // everything from the updated CSS.  Two passes: immediate + LOW-priority
    // idle (after any async Adwaita colour-scheme work at DEFAULT_IDLE priority).
    for win in gtk::Window::list_toplevels() {
        queue_draw_recursive(&win);
    }
    glib::idle_add_local_full(glib::Priority::LOW, || {
        for win in gtk::Window::list_toplevels() {
            queue_draw_recursive(&win);
        }
        glib::ControlFlow::Break
    });
}

// ── DeviceWindowInner ─────────────────────────────────────────────────────────
// All "content" widget state for one device window, kept together so that every
// GTK signal closure only needs one `Rc::clone(&inner)` capture instead of
// capturing half a dozen independent `Rc<RefCell<...>>` values.

struct DeviceWindowInner {
    ds:               DeviceState,
    /// Acquired once, right after `ds` is obtained, for the lifetime of
    /// this window (main + mini share the same `ds`, so one guard covers
    /// both surfaces) — releases automatically when this struct drops
    /// (window close/last-ref-drop), reverting `ds` to `Simple` mode.
    /// `device::discovery_manager`'s own tracked devices never acquire
    /// `Full` themselves (they only need `Simple`'s liveness+identity
    /// polling), so a window closing is the only thing that ever drops
    /// this back down. See `DeviceState::acquire_full()`.
    _full_mode:       FullModeGuard,
    show_devices_fn:  Rc<dyn Fn()>,
    sw:             SourceWidgets,
    ow:             OutputWidgets,
    pw:             PlaybackWidgets,
    presets:        views::presets::PresetsView,
    dev_info_label: Label,
    ip_label:       Label,
    net_icon:       gtk::Image,
    remote_icon:    gtk::Image,
    remote_label:   Label,
    icons:          Rc<icons::IconSet>,
    /// Shown (spinning) only while `ConnectionState::Connecting` — see
    /// `reset_device_ui()`. Overlaid on the header bar, not packed into
    /// it — see `build_header()`'s doc comment for why.
    connecting_spinner: gtk::Spinner,
    /// When `connecting_spinner` was last shown — `None` while hidden.
    /// Lets `hide_connecting_spinner()` enforce `MIN_SPINNER_DISPLAY`
    /// (see that constant) instead of hiding it again so fast (a
    /// same-LAN reconnect can resolve in well under 100ms) that it never
    /// renders a single visible frame — exactly as unreadable/glitchy as
    /// the text flash it replaced.
    spinner_shown_at: std::cell::Cell<Option<std::time::Instant>>,
    /// Pending deferred hide from `hide_connecting_spinner()`, if any —
    /// cancelled in `cleanup()` like `settle_timer`/`config_save_timer`.
    spinner_hide_timer: RefCell<Option<glib::SourceId>>,
    // Window / panel state — kept here so device-change and close handlers
    // only need one Rc<Inner> capture.
    window:              adw::ApplicationWindow,
    /// The full window's content widget (art background + toolbar/header +
    /// paned + bottom bar) — kept as its own handle so
    /// `DeviceWindowInner::apply_window_chrome()` can swap `window`'s
    /// content back to this when leaving mini mode. `window`'s content is
    /// exactly one of this or `mini.root` at any given time; there is only
    /// ever one top-level GTK window per device now, not two.
    full_content:        gtk::Overlay,
    /// Blurred-artwork background layer, behind everything else in the main
    /// window. Only actually visible under the RustyWiiM Modern theme — for
    /// other themes the foreground content above it is opaque, so this is
    /// always fed the current art but effectively inert otherwise.
    art_bg:              art_background::ArtBackground,
    paned:               gtk::Paned,
    left_pane:           gtk::Box,
    sidebar_btn:         gtk::ToggleButton,
    saved_panel_width:   Rc<RefCell<i32>>,
    panel_collapsing:    Rc<RefCell<bool>>,
    /// In-flight sidebar-toggle slide animation, if any — cancelled/skipped
    /// on the next toggle so rapid clicks don't pile up overlapping animations.
    panel_anim:          RefCell<Option<adw::TimedAnimation>>,
    settle_timer:        Rc<RefCell<Option<glib::SourceId>>>,
    /// Deferred config-save timer: cancelled and rescheduled on every
    /// state change so only one disk write happens after a burst of events.
    config_save_timer:   Rc<RefCell<Option<glib::SourceId>>>,
    /// UUID this window is/was for — pre-seeded at construction with the
    /// *expected* UUID (from `config`, before any live connection) so
    /// `DeviceWindow::uuid()` can dedup windows even before the device has
    /// answered its first API call; updated to the live UUID once known.
    /// **Not** by itself a record of "has window state actually been
    /// applied yet" — see `window_state_loaded`, which exists precisely
    /// because for any already-known device this starts out equal to the
    /// live UUID it will later be compared against, so it can't answer
    /// that question on its own.
    applied_window_key: RefCell<String>,
    /// Whether `apply_device_window_state()`'s body has actually run yet
    /// for the current connection. Starts `false` regardless of what
    /// `applied_window_key` was pre-seeded with, specifically so the very
    /// first real call for an already-known device (where
    /// `applied_window_key` already equals the incoming UUID from
    /// construction) doesn't get mistaken for "already applied" and
    /// silently skip loading the saved window size/panel state/
    /// `playback_access_override` — a real bug this field fixes.
    window_state_loaded: Cell<bool>,
    /// Guards `cleanup()` so its body only actually runs once, even though
    /// it's invoked from both `close-request` and `connect_destroy` for a
    /// single user-initiated close. Nothing in `cleanup()` benefits from
    /// running twice, and computing "is this the last visible window"
    /// specifically needs to *not* re-run on the second call, since by then
    /// the window may be torn down enough for that to (wrongly) come out
    /// differently — a single guard on the whole function is simpler than
    /// caching that one value.
    cleaned_up: Cell<bool>,
    /// Last known friendly name — the window title's fallback while
    /// `device_info()` is `None` (still `Connecting`, or `Failed`/
    /// "Disconnected"): otherwise there'd be nothing to show but the
    /// generic "RustyWiiM". Seeded at construction from
    /// `config::DeviceConfig::name` (that field's own doc comment promises
    /// exactly this, "displayed while connecting / offline" — it just was
    /// never actually wired up to the window title before), then kept
    /// fresh by `apply_device_info()` every time the device actually
    /// answers — so a *later* disconnect falls back to the most recently
    /// confirmed live name, not a stale config-time one (e.g. the device
    /// having since been renamed in the WiiM app). Empty for a brand-new
    /// device config has never seen before and hasn't connected yet.
    cached_name: RefCell<String>,
    // ── Mini player ───────────────────────────────────────────────────────────
    mini:              MiniWidgets,
    /// Which of the two panels ("full" or "mini") the one shared `window` is
    /// currently showing. Not to be confused with GNOME/the desktop's own
    /// notion of a "maximized" window — that's an orthogonal, OS-level state
    /// a window can be in *while* showing our full panel; see the
    /// `full_mode_size`/`full_mode_maximized`/`mini_mode_width` group below
    /// for how the two interact.
    mini_mode:         RefCell<bool>,

    // Remembered geometry for whichever panel *isn't* currently showing, so
    // switching back to it restores the right size instead of leaving the
    // window at whatever size the other panel happened to need. Both
    // directions are needed because, unlike the old design (a genuinely
    // separate GTK window per panel, where the hidden one just kept
    // whatever size/maximized-state it already had — free, no bookkeeping
    // needed), the two panels now share one real window: switching panels
    // actually resizes it, so the size the panel you're leaving had is
    // about to be overwritten and has to be saved *first*.
    //
    // "Full mode" here means our own full-panel/mini-panel distinction
    // (`mini_mode` above) — a completely separate thing from the desktop's
    // own "maximized" window state, which is *also* only meaningful while
    // showing the full panel (mini mode is never maximized — see
    // `apply_window_chrome()`'s `resizable(false)` comment for why a
    // maximized mini panel isn't something we want the desktop offering in
    // the first place). A window can be "in full mode, maximized",
    // "in full mode, not maximized", or "in mini mode" — maximized mini
    // mode is simply not a state that exists.
    /// The full panel's windowed (non-maximized) size to restore on
    /// `exit_mini_mode()` — captured by `enter_mini_mode()` right before it
    /// shrinks the window down for mini content, but only while the window
    /// isn't currently maximized (while maximized, `width()`/`height()`
    /// report the full-screen size, not a real windowed size worth
    /// remembering — see `enter_mini_mode()`'s own comment). `exit_mini_mode()`
    /// applies this via `set_default_size()` unconditionally, even when also
    /// about to re-maximize — see that function's comment for why: it's not
    /// just the *visible* restored size, it's also what GTK/the compositor
    /// falls back to if the user later un-maximizes by hand, which needs
    /// resetting away from whatever mini panel width `enter_mini_mode()`
    /// last requested.
    full_mode_size:      RefCell<(i32, i32)>,
    /// Whether the window was OS-maximized right before `enter_mini_mode()`
    /// last shrank it for mini content — restored by `exit_mini_mode()`
    /// (which calls `maximize()` instead of relying on `full_mode_size`
    /// alone). Needed because entering mini mode has to un-maximize the
    /// window first (a maximized window can't also be the small floating
    /// mini panel), which would otherwise lose that fact for good.
    full_mode_maximized: Cell<bool>,
    /// Set immediately before `exit_mini_mode()` calls `window.maximize()`
    /// to restore a remembered maximized state, and consumed (read-and-clear)
    /// by the window's own `notify::maximized` handler — see that handler's
    /// comment (`new_inner()`) for why: it also opportunistically captures
    /// `full_mode_size` on every *genuine* transition into maximized (not
    /// just at `enter_mini_mode()` time), and this flag is what tells it
    /// "this particular transition is our own restore, not a fresh
    /// external one — don't recapture, the window is only this size because
    /// it was mini a moment ago."
    maximize_call_pending: Cell<bool>,
    /// The mini panel's width to restore on `enter_mini_mode()`, kept
    /// up to date while the full panel is showing (there's no live mini
    /// content to read a current width off in that state). Updated from
    /// config on device-info-apply and captured fresh by `exit_mini_mode()`
    /// (reading `window.width()` while it's still showing mini content,
    /// which reflects any live drag-resize too). Falls back to
    /// `widgets::MINI_WIDTH_DEFAULT` wherever it's read and still `0`
    /// (never set this session, nothing saved in config either).
    mini_mode_width:     Cell<i32>,

    mini_btn:          gtk::Button,
}

impl Drop for DeviceWindowInner {
    fn drop(&mut self) {
        dbg_ui(&format!(
            "DeviceWindowInner dropped (uuid={})",
            self.applied_window_key.borrow()
        ));
    }
}

impl DeviceWindowInner {
    fn cleanup(&self) {
        // Fires from both close-request and connect_destroy for a single
        // user-initiated close; nothing below benefits from running twice,
        // and computing "is this the last visible window" specifically
        // needs to run exactly once, while self.window is still guaranteed
        // fully alive/registered (true on the first call, not guaranteed by
        // the second) — so just don't let there be a second call.
        if self.cleaned_up.replace(true) { return; }

        let uuid = self.ds.device_info()
            .map(|di| di.uuid)
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| self.applied_window_key.borrow().clone());
        let settle_pending  = self.settle_timer.borrow().is_some();
        let cfgsave_pending = self.config_save_timer.borrow().is_some();
        dbg_ui(&format!(
            "DeviceWindow cleanup uuid={uuid} settle_pending={settle_pending} cfgsave_pending={cfgsave_pending}"
        ));
        if let Some(id) = self.settle_timer.borrow_mut().take() { id.remove(); }
        if let Some(id) = self.config_save_timer.borrow_mut().take() { id.remove(); }
        if let Some(id) = self.spinner_hide_timer.borrow_mut().take() { id.remove(); }
        self.save_config_now();

        // Skip saving the "window_open" flag to false if that was the last
        // window, since that will be "quit" operation, it's more intuitive
        // to come back to the same state on re-launch.
        //
        // Window geometry is still saved above either way — only the
        // window_open flag itself is skipped, in two cases: an explicit
        // app quit (QUITTING), and closing what turns out to be the last
        // visible window.
        //
        // Settings windows never register with the GtkApplication, so they
        // don't count as "another visible window" here — closing your last
        // device window while a Settings window happens to be open still
        // preserves it. There's only ever one top-level GTK window per
        // device now (full and mini are two contents of the same window,
        // not two windows), so unlike an older version of this check, no
        // second window needs excluding here.
        let last_window = self.window.application().is_some_and(|app| {
            !app.windows().iter().any(|w| {
                w.upcast_ref::<gtk::Widget>() != self.window.upcast_ref::<gtk::Widget>()
                    && w.is_visible()
            })
        });
        dbg_ui(&format!(
            "DeviceWindow cleanup uuid={uuid} last_window={last_window} quitting={}",
            QUITTING.load(Ordering::Relaxed)
        ));
        if !uuid.is_empty() && !QUITTING.load(Ordering::Relaxed) && !last_window {
            dbg_ui(&format!("DeviceWindow cleanup uuid={uuid} persisting window_open=false"));
            config::update(|cfg| cfg.device_mut(&uuid).window_open = false);
        } else if !uuid.is_empty() {
            dbg_ui(&format!("DeviceWindow cleanup uuid={uuid} preserving window_open (last_window or quitting)"));
        }
    }
}

// ── DeviceWindow ──────────────────────────────────────────────────────────────

/// One device window.  Owns the GTK window and all content widgets.
#[derive(Clone)]
pub struct DeviceWindow {
    pub window: adw::ApplicationWindow,
    inner:      Rc<DeviceWindowInner>,
}

impl DeviceWindow {
    #[allow(dead_code)]
    pub fn ds(&self) -> &DeviceState { &self.inner.ds }

    /// UUID of the device this window was created for.  Returns the live UUID
    /// from the connected device if available, otherwise falls back to the UUID
    /// recorded at creation time (applied_window_key) so dedup works even before
    /// the device has responded to its first API call.
    pub fn uuid(&self) -> Option<String> {
        self.inner.ds.device_info()
            .map(|i| i.uuid)
            .filter(|u| !u.is_empty())
            .or_else(|| {
                let k = self.inner.applied_window_key.borrow().clone();
                if k.is_empty() { None } else { Some(k) }
            })
    }

    /// Build a device window connected to a specific device.
    pub fn new_for_device(
        app:            &adw::Application,
        device_manager: DeviceManager,
        show_devices_fn: Rc<dyn Fn()>,
        open_settings:  Rc<dyn Fn(Option<DeviceState>)>,
        spec:           DeviceSpec,
    ) -> Self {
        Self::new_inner(app, device_manager, show_devices_fn, open_settings, Some(spec))
    }

    fn new_inner(
        app:             &adw::Application,
        device_manager:  DeviceManager,
        show_devices_fn: Rc<dyn Fn()>,
        open_settings:   Rc<dyn Fn(Option<DeviceState>)>,
        device_spec:     Option<DeviceSpec>,
    ) -> Self {
        let icons = Rc::new(icons::IconSet::load());

        // Pick the device UUID to use for loading per-device window config.
        // The no-spec path no longer falls back to last_uuid (phased out).
        let cfg_uuid: String = device_spec.as_ref()
            .map(|s| s.uuid.clone())
            .filter(|u| !u.is_empty())
            .unwrap_or_default();

        let init_dev_cfg = config::with(|cfg| cfg.device(&cfg_uuid));

        let ds = match device_spec.as_ref() {
            Some(spec) => device_manager.get(
                &spec.uuid, &spec.ip, spec.tls_mode,
                init_dev_cfg.playback_access_override,
                init_dev_cfg.mute_access_override,
                spec.try_connect,
            ),
            None => {
                // No device spec: create a standalone state that isn't wired to
                // any device yet; polling still starts so the UI can be shown.
                let ds = DeviceState::new(device_manager.rt(), String::new());
                ds.start_polling();
                ds
            }
        };
        // A device window always wants Full mode — covers both branches
        // above (main + mini share this one `ds`, so one guard is enough
        // for both surfaces); released automatically when this window's
        // `DeviceWindowInner` drops. See `_full_mode`'s doc comment.
        let full_mode = ds.acquire_full();

        dbg_ui(&format!("DeviceWindow creating (uuid={})", cfg_uuid));

        let (header, sidebar_btn, mini_btn, connecting_spinner) = build_header(init_dev_cfg.panel_visible);
        let presets = views::presets::PresetsView::new(&ds, &icons);
        let sw = build_source_widgets(&icons);
        let ow = build_output_widgets(&icons);
        let left_pane = build_left_pane(&sw, &ow, &presets);
        let pw = build_playback_widgets(&ds);
        let right_pane = build_right_pane(&pw);
        let (mini, _mini_root) = build_mini_window(&ds);

        // ── Paned split + sidebar logic ───────────────────────────────────────────
        let paned = gtk::Paned::new(Orientation::Horizontal);
        paned.set_start_child(Some(&left_pane));
        paned.set_end_child(Some(&right_pane));
        paned.set_shrink_start_child(true);
        paned.set_shrink_end_child(false);
        paned.set_resize_start_child(false);
        paned.set_resize_end_child(true);
        paned.set_margin_top(4);
        paned.set_margin_bottom(8);

        let panel_width = if init_dev_cfg.paned_position > 0 { init_dev_cfg.paned_position } else { 200 };
        paned.set_position(panel_width);
        left_pane.set_visible(init_dev_cfg.panel_visible);

        let saved_panel_width  = Rc::new(RefCell::new(panel_width));
        let panel_collapsing   = Rc::new(RefCell::new(false));
        let settle_timer:      Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        let config_save_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

        let dev_info_label = Label::builder()
            .css_classes(["device-info"]).halign(Align::Center)
            .hexpand(true)
            .margin_top(4).margin_bottom(4).build();

        // "ip-label" alongside "dim-label" gives modern.css a hook to match
        // this label's exact size/treatment to "device-info" (which doesn't
        // share dim-label's font-size with the pos/dur time labels that
        // also use it) — see the comment on apply_device_info()'s
        // ip_label.set_visible(true) call for why this one needed it and
        // device-info didn't.
        let ip_label = Label::builder()
            .css_classes(["dim-label", "ip-label"])
            .margin_end(6).margin_top(4).margin_bottom(4)
            .visible(false)
            .build();

        let net_icon = gtk::Image::builder()
            .icon_size(gtk::IconSize::Normal)
            .css_classes(["net-icon"])
            .margin_end(8).margin_top(4).margin_bottom(4)
            .visible(false)
            .build();

        let bottom_end = GtkBox::new(Orientation::Horizontal, 0);
        bottom_end.append(&ip_label);
        bottom_end.append(&net_icon);

        // BLE remote presence/battery — left-hand side of the bottom bar,
        // hidden until the first `getStatusEx` result confirms a remote is
        // actually connected (see `update_remote_display()`).
        let remote_icon = gtk::Image::from_paintable(Some(icons.remote_paintable()));
        // 21px: net_icon's IconSize::Normal (16px) plus 2px, then a further
        // +3px per request.
        remote_icon.set_pixel_size(28);
        remote_icon.add_css_class("remote-icon");
        remote_icon.set_margin_start(8);
        remote_icon.set_margin_top(4);
        remote_icon.set_margin_bottom(4);
        remote_icon.set_visible(false);

        // Same classes as ip_label above (not just "dim-label") so it's
        // displayed identically — "ip-label" is specifically what fixes
        // modern.css's top-row clipping/fade that plain "dim-label" alone
        // doesn't (see ip_label's own comment above).
        let remote_label = Label::builder()
            .css_classes(["dim-label", "ip-label"])
            .margin_start(4).margin_top(4).margin_bottom(4)
            .visible(false)
            .build();

        let bottom_start = GtkBox::new(Orientation::Horizontal, 0);
        bottom_start.append(&remote_icon);
        bottom_start.append(&remote_label);

        let bottom_bar = gtk::CenterBox::new();
        bottom_bar.set_start_widget(Some(&bottom_start));
        bottom_bar.set_center_widget(Some(&dev_info_label));
        bottom_bar.set_end_widget(Some(&bottom_end));

        let outer = GtkBox::new(Orientation::Vertical, 0);
        outer.append(&paned);
        outer.append(&gtk::Separator::new(Orientation::Horizontal));
        outer.append(&bottom_bar);

        let full_toolbar = adw::ToolbarView::new();
        full_toolbar.add_top_bar(&header);
        full_toolbar.set_content(Some(&outer));

        // Blurred-artwork background layer, behind the toolbar. Always
        // present; only visible when the active theme makes the toolbar/
        // window backgrounds transparent (RustyWiiM Modern — see modern.css).
        // Initial visibility is set once the window exists, below —
        // `update_art_background_visibility()`'s walk only reaches whichever
        // of this tree / `mini.root` is actually attached as the window's
        // content at the time it runs, so the mini tree's own art_bg gets
        // its first real pass from `apply_window_chrome()` instead (called
        // either by the mini-mode startup restore below, or the first time
        // `enter_mini_mode()` ever runs).
        let art_bg = art_background::ArtBackground::new();
        art_bg.set_hexpand(true);
        art_bg.set_vexpand(true);
        let window_overlay = gtk::Overlay::new();
        window_overlay.set_child(Some(&art_bg));
        window_overlay.add_overlay(&full_toolbar);
        // Below the header row, not inside it — see `build_header()`'s doc
        // comment for why the header bar itself is the wrong place to
        // overlay this (collides with the native CSD window buttons).
        window_overlay.add_overlay(&connecting_spinner);

        let win_w = if init_dev_cfg.window_width  > 0 { init_dev_cfg.window_width  } else { 680 };
        let win_h = if init_dev_cfg.window_height > 0 { init_dev_cfg.window_height } else { 640 };
        let window = adw::ApplicationWindow::builder()
            .application(app).title("RustyWiiM").content(&window_overlay)
            .default_width(win_w).default_height(win_h)
            .build();
        window.add_css_class("player-window");
        if init_dev_cfg.window_maximized { window.maximize(); }

        // Diagnostic only (--debug=ui): logs every time GTK itself observes
        // a maximized/default-size change take effect, which can lag behind
        // (or simply not match) the synchronous call that requested it —
        // e.g. `set_default_size()` is a request, not a synchronous resize,
        // so there can be a gap between "we called it" and "GTK/the
        // compositor actually applied it." Added chasing a real bug: a
        // maximize → mini → back-to-full → un-maximize round trip restoring
        // to the mini panel's width instead of the full panel's saved size.
        window.connect_notify_local(Some("maximized"), |win, _| {
            dbg_ui(&format!(
                "window notify::maximized -> is_maximized={} width={} height={} default_size={:?}",
                win.is_maximized(), win.width(), win.height(), win.default_size(),
            ));
        });
        window.connect_notify_local(Some("default-width"), |win, _| {
            dbg_ui(&format!(
                "window notify::default-width -> default_size={:?} is_maximized={} width={} height={}",
                win.default_size(), win.is_maximized(), win.width(), win.height(),
            ));
        });
        window.connect_notify_local(Some("default-height"), |win, _| {
            dbg_ui(&format!(
                "window notify::default-height -> default_size={:?} is_maximized={} width={} height={}",
                win.default_size(), win.is_maximized(), win.width(), win.height(),
            ));
        });

        // apply_theme() only fires on explicit runtime switches, so a window
        // (main or mini, both now built) opened after the app already
        // started on some theme needs its initial art_bg visibility set
        // directly from the live config.
        update_art_background_visibility();

        let inner = Rc::new(DeviceWindowInner {
            ds: ds.clone(),
            _full_mode: full_mode,
            show_devices_fn,
            sw,
            ow,
            pw,
            presets,
            dev_info_label,
            ip_label,
            net_icon,
            remote_icon,
            remote_label,
            icons,
            connecting_spinner,
            spinner_shown_at: std::cell::Cell::new(None),
            spinner_hide_timer: RefCell::new(None),
            window: window.clone(),
            full_content: window_overlay.clone(),
            art_bg: art_bg.clone(),
            paned:  paned.clone(),
            left_pane: left_pane.clone(),
            sidebar_btn: sidebar_btn.clone(),
            saved_panel_width,
            panel_collapsing,
            panel_anim: RefCell::new(None),
            settle_timer,
            config_save_timer,
            applied_window_key: RefCell::new(cfg_uuid.clone()),
            window_state_loaded: Cell::new(false),
            cleaned_up: Cell::new(false),
            cached_name: RefCell::new(init_dev_cfg.name.clone().unwrap_or_default()),
            mini,
            // Seeded correctly from the start (not left `false` and fixed
            // up later) so the upcoming `populate_all()` call — which only
            // refreshes whichever of main/mini is actually active — targets
            // the right one immediately for a device restoring straight
            // into mini mode. See `init_dev_cfg.mini_mode` block below for
            // the rest of that restore (things that need `inner`/already-
            // built widgets to exist, so can't be folded into this literal).
            mini_mode:         RefCell::new(init_dev_cfg.mini_mode),
            // Seeded from the same config-restored (or default) width/height
            // the window itself was just constructed with (win_w/win_h,
            // above) — not (0, 0). Left at (0, 0), a device that gets
            // maximized before ever entering mini mode even once this
            // session would have no real size to fall back on:
            // enter_mini_mode() only captures a fresh value while *not*
            // currently maximized (maximized width()/height() would be the
            // screen resolution, not a real windowed size — see that
            // function's comment), so if it's *always* been maximized so
            // far, this field is the only source of truth there is.
            // Confirmed live via --debug=ui: exactly this scenario left
            // exit_mini_mode() holding (0, 0), which its own w>0&&h>0 guard
            // correctly refused to apply — but with nothing to fall back to
            // either, `default_size` was simply never corrected back from
            // whatever enter_mini_mode() had last set it to (the mini
            // panel's width), so a later manual un-maximize snapped to that
            // instead of the real windowed size.
            full_mode_size:      RefCell::new((win_w, win_h)),
            full_mode_maximized: Cell::new(init_dev_cfg.window_maximized),
            maximize_call_pending: Cell::new(false),
            mini_mode_width:     Cell::new(init_dev_cfg.mini_window_width),
            mini_btn:          mini_btn.clone(),
        });

        // ── DeviceState signal connections ────────────────────────────────────────
        // Use Rc::downgrade so the closures don't keep DeviceWindowInner alive
        // after the window is closed — broken upgrade() calls become no-ops.
        ds.connect_device_changed({
            let i = Rc::downgrade(&inner);
            move |ds| {
                let Some(i) = i.upgrade() else { return };
                dbg_ui(&format!(
                    "device-changed signal: device_info_present={}",
                    ds.device_info().is_some(),
                ));
                i.populate_all();
            }
        });

        ds.connect_network_changed({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.update_network_icon(); } }
        });

        ds.connect_remote_changed({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.update_remote_display(); } }
        });

        ds.connect_playback_changed({
            let i = Rc::downgrade(&inner);
            move |_, mask| {
                let Some(i) = i.upgrade() else { return };
                dbg_ui(&format!("playback-changed signal: mask={mask:#x}"));
                // `update_playback_ui()`/`update_mini_playback()` no-op
                // themselves while offline (`DeviceWindowInner::live()`) —
                // needed since a signal can race with (or briefly precede) a
                // disconnect, and acting on it anyway would repaint from
                // `playback_state()`'s still-cached fields, undoing whatever
                // `reset_device_ui()` just cleared.
                if *i.mini_mode.borrow() { i.update_mini_playback(mask); } else { i.update_playback_ui(mask); }
            }
        });

        ds.connect_input_changed({
            let i = Rc::downgrade(&inner);
            move |_| {
                let Some(i) = i.upgrade() else { return };
                // See the `playback-changed` handler above — same reasoning.
                if *i.mini_mode.borrow() { i.update_mini_playback(crate::device::state::playback_changed::ALL); } else { i.update_input_display(); }
            }
        });

        ds.connect_inputs_changed({
            let i = Rc::downgrade(&inner);
            move |_| {
                let Some(i) = i.upgrade() else { return };
                i.populate_source();
                i.update_input_display();
            }
        });

        ds.connect_output_changed({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.update_output_display(); } }
        });

        ds.connect_outputs_changed({
            let i = Rc::downgrade(&inner);
            move |_| {
                let Some(i) = i.upgrade() else { return };
                i.populate_output();
                i.update_output_display();
            }
        });

        // Populate immediately from whatever the DeviceState already has cached.
        inner.populate_all();

        // The views stay permanently active for now — each self-subscribes
        // to the DeviceState signals it needs, and today's window-driven
        // update paths refreshed them regardless of which panel was showing
        // anyway, so this matches existing behavior. Mode-following
        // activation (mini mode deactivating the full-panel views) comes
        // with the playback-view split.
        inner.pw.volume.set_active(true);
        inner.mini.volume.set_active(true);
        inner.presets.set_active(true);

        // Opportunistically keeps `full_mode_size` fresh from *any* genuine
        // maximize, not just the one `enter_mini_mode()` captures on its own
        // way in — covers a device that gets manually resized and then
        // maximized directly, with no intervening un-maximize, which
        // `enter_mini_mode()` alone can never see (by the time it runs,
        // width()/height() would already report the maximized size, not the
        // windowed one — see its own comment).
        //
        // The moment `is_maximized` flips to `true` is exactly the window
        // to catch this in: confirmed live (--debug=ui) that width()/
        // height() still report the *old*, pre-maximize windowed size at
        // that exact instant, for a brief window before GTK/the compositor
        // catches the surface up to the new maximized geometry — after
        // that, they're useless (screen resolution, not a real size to
        // remember). `maximize_call_pending` (see its own doc comment)
        // is what keeps this from misfiring on `exit_mini_mode()`'s own
        // restore-a-remembered-maximize call, which would otherwise
        // recapture the mini panel's own small size as if it were a real
        // windowed size the instant before maximizing.
        window.connect_notify_local(Some("maximized"), {
            let i = Rc::downgrade(&inner);
            move |win, _| {
                let Some(i) = i.upgrade() else { return };
                if !win.is_maximized() {
                    // Not a "genuine maximize" transition — nothing to
                    // capture. Still worth clearing defensively: a pending
                    // flag that somehow never got consumed by a "true"
                    // notify (e.g. the WM coalesced/dropped one) shouldn't
                    // silently swallow a later, unrelated one.
                    i.maximize_call_pending.set(false);

                    // The actual guarantee behind the maximize/mini/
                    // un-maximize fix (see exit_mini_mode()'s comment for
                    // the full reasoning): whenever the window becomes
                    // un-maximized while the full panel is the one showing
                    // (never while entering mini mode itself — that's about
                    // to shrink to mini dimensions right after this, forcing
                    // full_mode_size here would just fight that), force it
                    // back to full_mode_size if it isn't already there.
                    //
                    // Deferred via idle_add_local_once rather than applied
                    // synchronously right here: an earlier version called
                    // set_default_size() directly inside this same handler
                    // and it didn't stick — confirmed live, a compositor
                    // configure event for this *same* un-maximize transition
                    // arrived a moment later and silently overwrote it back
                    // to the wrong size. That call was racing (and losing
                    // to) the compositor's own in-flight negotiation for
                    // this transition, unlike enter_mini_mode()'s own
                    // set_default_size() call after unmaximize() — that one
                    // works because it runs on an already-*settled* window
                    // (this same transition has fully finished by the time
                    // any code the user's own actions trigger next runs),
                    // making it a plain resize, not a race. `Priority::DEFAULT_IDLE`
                    // (what idle_add_local_once uses) runs after any
                    // already-queued/in-flight Wayland protocol messages —
                    // i.e. after this transition's own remaining configure
                    // events, if any — hopefully letting our correction go
                    // last instead of first.
                    if !*i.mini_mode.borrow() {
                        let (fw, fh) = *i.full_mode_size.borrow();
                        if fw > 0 && fh > 0 {
                            let win = win.clone();
                            glib::idle_add_local_once(move || {
                                if win.width() != fw || win.height() != fh {
                                    dbg_ui(&format!(
                                        "un-maximize fixup (deferred): size is {}x{}, correcting to full_mode_size full:{fw},{fh}",
                                        win.width(), win.height(),
                                    ));
                                    win.set_default_size(fw, fh);
                                } else {
                                    dbg_ui(&format!(
                                        "un-maximize fixup (deferred): size already correct (full:{fw},{fh}), nothing to do",
                                    ));
                                }
                            });
                        }
                    }
                    return;
                }
                if i.maximize_call_pending.replace(false) {
                    dbg_ui(&format!(
                        "maximize notify: our own exit_mini_mode() restore skipping state saving"
                    ));
                    return; // our own restore, not a fresh external maximize
                }
                let (w, h) = (win.width(), win.height());
                if w > 0 && h > 0 {
                    let (old_fw, old_fh) = *i.full_mode_size.borrow();
                    *i.full_mode_size.borrow_mut() = (w, h);
                    dbg_ui(&format!(
                        "maximize notify: external maximize, storing size into full_mode_size: full:{w},{h} (was full:{old_fw},{old_fh})",
                    ));
                }
            }
        });

        // ── Sidebar toggle ────────────────────────────────────────────────────────
        let paned_btn_held = Rc::new(RefCell::new(false));
        const SNAP_PX: i32 = 30;

        inner.paned.connect_position_notify({
            let i    = Rc::downgrade(&inner);
            let held = Rc::clone(&paned_btn_held);
            move |p| {
                let Some(i) = i.upgrade() else { return };
                if *i.panel_collapsing.borrow() { return; }
                let pos = p.position();
                if pos >= SNAP_PX {
                    if !i.left_pane.is_visible() {
                        *i.panel_collapsing.borrow_mut() = true;
                        i.left_pane.set_visible(true);
                        *i.panel_collapsing.borrow_mut() = false;
                    }
                } else if i.left_pane.is_visible() {
                    *i.panel_collapsing.borrow_mut() = true;
                    i.left_pane.set_visible(false);
                    *i.panel_collapsing.borrow_mut() = false;
                }
                if let Some(id) = i.settle_timer.borrow_mut().take() { id.remove(); }
                let i2    = Rc::clone(&i);
                let held2 = Rc::clone(&held);
                let id = glib::timeout_add_local_once(
                    std::time::Duration::from_millis(50),
                    move || {
                        *i2.settle_timer.borrow_mut() = None;
                        let btn_held = *held2.borrow();
                        *held2.borrow_mut() = false;
                        let shown = i2.left_pane.is_visible();
                        if i2.sidebar_btn.is_active() != shown {
                            *i2.panel_collapsing.borrow_mut() = true;
                            i2.sidebar_btn.set_active(shown);
                            *i2.panel_collapsing.borrow_mut() = false;
                        }
                        if shown && !btn_held {
                            let pos = i2.paned.position();
                            if pos >= SNAP_PX { *i2.saved_panel_width.borrow_mut() = pos; }
                        }
                        playback::schedule_config_save(&i2);
                    },
                );
                *i.settle_timer.borrow_mut() = Some(id);
            }
        });

        {
            let drag_ctrl = gtk::EventControllerLegacy::new();
            drag_ctrl.connect_event({
                let i    = Rc::downgrade(&inner);
                let held = Rc::clone(&paned_btn_held);
                move |_, event| {
                    let Some(i) = i.upgrade() else { return glib::Propagation::Proceed };
                    match event.event_type() {
                        gtk::gdk::EventType::ButtonPress => {
                            *held.borrow_mut() = true;
                        }
                        gtk::gdk::EventType::ButtonRelease => {
                            *held.borrow_mut() = false;
                            if let Some(id) = i.settle_timer.borrow_mut().take() { id.remove(); }
                            let shown = i.left_pane.is_visible();
                            if i.sidebar_btn.is_active() != shown {
                                *i.panel_collapsing.borrow_mut() = true;
                                i.sidebar_btn.set_active(shown);
                                *i.panel_collapsing.borrow_mut() = false;
                            }
                            if shown {
                                let pos = i.paned.position();
                                if pos >= SNAP_PX { *i.saved_panel_width.borrow_mut() = pos; }
                            }
                            playback::schedule_config_save(&i);
                        }
                        _ => {}
                    }
                    glib::Propagation::Proceed
                }
            });
            inner.paned.add_controller(drag_ctrl);
        }

        inner.sidebar_btn.connect_toggled({
            let i = Rc::downgrade(&inner);
            move |btn| {
                let Some(i) = i.upgrade() else { return };
                if *i.panel_collapsing.borrow() { return; }
                if let Some(id) = i.settle_timer.borrow_mut().take() { id.remove(); }
                if btn.is_active() {
                    let w = *i.saved_panel_width.borrow();
                    playback::animate_panel_to(&i, w);
                } else {
                    playback::animate_panel_to(&i, 0);
                }
            }
        });


        // ── Transport / control signal handlers ───────────────────────────────────
        inner.pw.btn_play.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.ds.do_play_pause(); } }
        });

        inner.pw.btn_prev.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.ds.do_prev(); } }
        });

        inner.pw.btn_next.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.ds.do_next(); } }
        });

        inner.pw.btn_bt_pair.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.ds.bt_enter_pairing(); } }
        });

        // ── Keyboard shortcuts ───────────────────────────────────────────────────
        // Capture phase: must win over a focused seek/volume Scale's own
        // Left/Right/Up/Down handling, since the whole point is a global
        // shortcut that works regardless of what has focus. One controller on
        // the one shared window now (previously one per window) — which
        // panel's transport buttons get the key-flash is picked live from
        // `mini_mode` on every keypress, rather than being fixed per controller.
        {
            let key_ctrl = gtk::EventControllerKey::new();
            key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
            key_ctrl.connect_key_pressed({
                let i = Rc::downgrade(&inner);
                move |_, keyval, _keycode, state| {
                    let Some(i) = i.upgrade() else { return glib::Propagation::Proceed };
                    let (prev, next, play) = if *i.mini_mode.borrow() {
                        (i.mini.btn_prev.clone(), i.mini.btn_next.clone(), i.mini.btn_play.clone())
                    } else {
                        (i.pw.btn_prev.clone(), i.pw.btn_next.clone(), i.pw.btn_play.clone())
                    };
                    playback::handle_transport_key(&i, keyval, state, &prev, &next, &play)
                }
            });
            window.add_controller(key_ctrl);
        }

        inner.pw.shuffle.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| {
                let Some(i) = i.upgrade() else { return };
                let ps = i.ds.playback_state();
                i.ds.do_set_loop_mode(loop_api_mode(!ps.shuffle, ps.repeat));
            }
        });

        inner.pw.repeat.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| {
                let Some(i) = i.upgrade() else { return };
                let ps = i.ds.playback_state();
                i.ds.do_set_loop_mode(loop_api_mode(ps.shuffle, ps.repeat.next()));
            }
        });

        inner.pw.seek.connect_change_value({
            let i = Rc::downgrade(&inner);
            move |_, _, value| {
                if let Some(i) = i.upgrade() {
                    if let Some(c) = i.ds.client() {
                        i.ds.rt().spawn(async move { let _ = c.seek(value as u32).await; });
                    }
                }
                glib::Propagation::Proceed
            }
        });

        inner.sw.dropdown.connect_selected_notify({
            let i = Rc::downgrade(&inner);
            move |dd| {
                let Some(i) = i.upgrade() else { return };
                if *i.sw.updating.borrow() { return; }
                let idx = dd.selected() as usize;
                let ids = i.sw.ids.borrow();
                if let Some(src) = ids.get(idx).cloned() {
                    i.ds.switch_input(src);
                }
            }
        });

        inner.ow.dropdown.connect_selected_notify({
            let i = Rc::downgrade(&inner);
            move |dd| {
                let Some(i) = i.upgrade() else { return };
                if *i.ow.updating.borrow() { return; }
                let idx = dd.selected() as usize;
                let modes = i.ow.modes.borrow();
                if let Some(&mode) = modes.get(idx) {
                    i.ds.set_audio_output(mode);
                }
            }
        });

        // ── Mini player signals ───────────────────────────────────────────────────
        // A plain click, not a toggle — this button only ever lives in the
        // full panel's header, so clicking it can only ever mean "switch to
        // mini mode" (going the other way is restore_btn/double-click/M
        // below, none of which are this button).
        inner.mini_btn.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_btn| {
                let Some(i) = i.upgrade() else { return };
                i.enter_mini_mode();
                playback::schedule_config_save(&i);
            }
        });

        inner.mini.restore_btn.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| {
                let Some(i) = i.upgrade() else { return };
                i.exit_mini_mode();
                playback::schedule_config_save(&i);
            }
        });

        // The mini panel's own close (X) button — same "close this device"
        // meaning as win.close below, just with no native titlebar button to
        // trigger it from (mini mode is undecorated).
        inner.mini.close_btn.connect_clicked({
            clone!(@strong window => move |_| {
                gtk::prelude::WidgetExt::realize(&window); // close() is a no-op on an unrealized window
                window.close();
            })
        });

        {
            let gesture = gtk::GestureClick::builder().button(1).build();
            gesture.connect_pressed({
                let i = Rc::downgrade(&inner);
                move |_, n_press, _, _| {
                    let Some(i) = i.upgrade() else { return };
                    if n_press >= 2 {
                        i.exit_mini_mode();
                        playback::schedule_config_save(&i);
                    }
                }
            });
            inner.mini.root.add_controller(gesture);
        }

        inner.mini.btn_play.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.ds.do_play_pause(); } }
        });

        inner.mini.btn_prev.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.ds.do_prev(); } }
        });

        inner.mini.btn_next.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.ds.do_next(); } }
        });

        inner.mini.btn_bt_pair.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.ds.bt_enter_pairing(); } }
        });

        // ── Window actions ────────────────────────────────────────────────────────
        // win.close (Ctrl-W), win.devices, win.about, win.settings — one
        // registration each now, on the one shared window (previously
        // duplicated onto a separate mini_win too).
        let close_action = gio::SimpleAction::new("close", None);
        {
            let win_for_close = window.clone();
            close_action.connect_activate(move |_, _| {
                gtk::prelude::WidgetExt::realize(&win_for_close); // close() is a no-op on an unrealized window
                win_for_close.close();
            });
        }
        window.add_action(&close_action);

        let devices_action = gio::SimpleAction::new("devices", None);
        {
            let i = Rc::downgrade(&inner);
            devices_action.connect_activate(move |_, _| {
                if let Some(i) = i.upgrade() { (i.show_devices_fn)(); }
            });
        }
        window.add_action(&devices_action);

        wire_window_actions(&window, Some(ds.clone()), open_settings);

        // close-request fires on any close attempt: the native titlebar X
        // button (full mode only — mini mode is undecorated), win.close()
        // (Ctrl-W / the mini panel's close_btn), Alt+F4, a compositor-level
        // close, or the quit action closing every window on the way out.
        // Always means "close this device", regardless of which panel is
        // currently showing — mini mode has its own dedicated "go back to
        // full mode without closing" affordances (the restore button,
        // double-click, the header mini-toggle, the M shortcut); close-request
        // itself isn't one of them, in any mode. cleanup() is idempotent so
        // calling it here AND in connect_destroy is safe.
        window.connect_close_request({
            let i = Rc::downgrade(&inner);
            move |_win| {
                dbg_ui("window close-request");
                if let Some(i) = i.upgrade() { i.cleanup(); }
                glib::Propagation::Proceed
            }
        });

        // destroy: single place for all cleanup.  Fires on every destruction path:
        //   • user close  (close-request → Proceed → GTK destroys → destroy)
        //   • win.destroy() from quit action (skips close-request, fires destroy directly)
        //   • app.quit()   (GTK destroys all windows during shutdown → destroy)
        // A second connect_destroy added later in open_device_spec clears the registry
        // (fires after this one, in connection order), which drops the last Rc<Inner> → Drop.
        window.connect_destroy({
            let i = Rc::downgrade(&inner);
            move |_win| {
                if let Some(i) = i.upgrade() {
                    i.cleanup();
                } else {
                    dbg_ui("main window destroyed (inner already freed)");
                }
            }
        });

        if init_dev_cfg.mini_mode {
            // `mini_mode` itself is already correctly seeded above (before
            // `populate_all()` ran), so this is just the rest of the
            // restore that needs `inner`/already-built widgets to exist —
            // not calling the full `enter_mini_mode()` (which would treat
            // this as a live transition, e.g. trying to read `window`'s
            // current size as the full-mode size to restore later). Swaps
            // the window straight to its mini chrome/content before it's
            // ever presented, so `DeviceWindow::present()` shows it already
            // looking right. mini_btn itself needs no update — it's a plain,
            // stateless button (see its own doc comment in widgets.rs).
            // Seed full_mode_size from saved config so exit_mini_mode() can
            // restore the right full-panel size even before the mini panel
            // has ever been shown (full_mode_maximized is already seeded
            // the same way, straight in the struct literal above).
            *inner.full_mode_size.borrow_mut() = (win_w, win_h);
            // Mini mode is never maximized — see the matching unmaximize() in
            // enter_mini_mode(). `window.maximize()` above already ran
            // unconditionally off `init_dev_cfg.window_maximized` before
            // this block knew whether mini_mode was also set; undo it here
            // for the same reason enter_mini_mode() insists on it live.
            inner.window.unmaximize();
            inner.apply_window_chrome(true);
            // Same width-resolve + measured-height sizing as the live
            // `enter_mini_mode()` transition, shared so the two can't drift.
            inner.apply_mini_window_size();
        }

        Self { window, inner }
    }

    pub fn present(&self) {
        self.window.present();
    }
}

// ── AppState ──────────────────────────────────────────────────────────────────
// Owns all top-level window state.  Every signal-handler closure captures
// either a strong Rc<AppState> or a Weak clone for the close-request handlers.

fn dbg_state(msg: &str) {
    if DEBUG_STATE.load(Ordering::Relaxed) {
        println!("[app] {msg}");
    }
}

pub(crate) struct AppState {
    app:            adw::Application,
    disc_mgr:       DiscoveryManager,
    device_manager: DeviceManager,
    registry:       RefCell<Vec<DeviceWindow>>,
    settings_reg:   RefCell<Vec<settings::SettingsWindow>>,
    disc_win:       RefCell<Option<devlist::DiscoveryWindow>>,
}

impl AppState {
    // `disc_svc.start()` must run inside `connect_activate` so that
    // `glib::spawn_future_local` has an active main context.
    //
    // Skipped entirely under `--connect`: that mode exists to point the app
    // at an isolated target (e.g. `wiim-simulator`) without touching the
    // real network, so starting SSDP discovery in the background would
    // defeat the purpose (and send real traffic) even though `activate()`
    // never shows its results.
    pub(crate) fn new(app: &adw::Application, rt: Arc<tokio::runtime::Runtime>) -> Rc<Self> {
        let disc_svc = DiscoveryService::new(rt.clone());
        if DIRECT_CONNECT.get().is_none() {
            disc_svc.start();
        }
        let device_manager = DeviceManager::new(rt.clone());

        // `device_manager` construction is inert (no side effects) —
        // connecting `configure-device` this early, before anything else
        // touches `device_manager`, means there's no window where a
        // `DeviceState` could be created before this handler exists to
        // configure it. Resolves per-device config overrides (device/
        // can't read config itself) and pushes them onto the fresh
        // `DeviceState` before `create_and_configure()` lets it make first
        // contact.
        device_manager.connect_configure_device(|_, ds| {
            let uuid = ds.uuid();
            if uuid.is_empty() { return; }
            let (access_override, mute_access_override) = config::with(|cfg| {
                let d = cfg.device(&uuid);
                (d.playback_access_override, d.mute_access_override)
            });
            dbg_state(&format!(
                "configure-device: {} ({uuid}) access_override={access_override:?} mute_access_override={mute_access_override:?}",
                ds.ip(),
            ));
            ds.set_playback_access_override(access_override);
            ds.set_mute_access_override(mute_access_override);
        });

        // `disc_mgr` now owns the *entire* known-device registry (SSDP
        // consumption, pinned/config-remembered devices, presence — see
        // `device::discovery_manager`'s module doc comment) — it holds
        // `device_manager` directly rather than through a hook/callback
        // pair, since both live in `device/` now.
        let disc_mgr = DiscoveryManager::new(rt, disc_svc.clone(), device_manager.clone());

        Rc::new(Self {
            app:            app.clone(),
            disc_mgr,
            device_manager,
            registry:       RefCell::new(Vec::new()),
            settings_reg:   RefCell::new(Vec::new()),
            disc_win:       RefCell::new(None),
        })
    }

    /// Open (or re-present) the settings window for `ds`, deduplicating by UUID.
    fn open_settings(self_rc: &Rc<Self>, ds: Option<DeviceState>) {
        let ds_uuid = ds.as_ref()
            .and_then(|d| d.device_info())
            .map(|i| i.uuid.clone())
            .filter(|u| !u.is_empty());
        {
            let reg = self_rc.settings_reg.borrow();
            for sw in reg.iter() {
                if sw.device_uuid() == ds_uuid {
                    dbg_state(&format!("settings: presenting existing for {:?}", ds_uuid));
                    sw.present();
                    return;
                }
            }
        }
        dbg_state(&format!("settings: opening new for {:?}", ds_uuid));
        let s = settings::SettingsWindow::new(ds, &self_rc.disc_mgr);
        let win_clone  = s.window_ref().clone();
        let weak_self  = Rc::downgrade(self_rc);
        let close_uuid = ds_uuid.clone();
        s.window_ref().connect_close_request(move |win| {
            dbg_state(&format!("settings: closed for {:?}", close_uuid));
            if let Some(state) = weak_self.upgrade() {
                state.settings_reg.borrow_mut().retain(|w| w.window_ref() != &win_clone);
            }
            // Explicit, rather than relying on close()'s default handler to
            // do it — this is what actually frees the page widgets
            // (ComboRows etc.) and, with them, any strong refs their signal
            // closures hold (e.g. the Advanced page's access-method rows,
            // even after those were fixed to hold `ds` weakly — see
            // `wire_access_row()`'s doc comment). Without an explicit
            // destroy() here nothing actually confirmed the window's widget
            // tree itself was ever torn down, only that `settings_reg`
            // dropped its own reference to it.
            win.destroy();
            glib::Propagation::Proceed
        });
        s.present();
        self_rc.settings_reg.borrow_mut().push(s);
    }

    /// Show (or lazily create) the device-list window.
    fn show_devices(self_rc: &Rc<Self>) {
        let mut dw = self_rc.disc_win.borrow_mut();
        if dw.is_none() {
            dbg_state("device list: creating window");
            let open_device_fn = {
                let state = Rc::clone(self_rc);
                Rc::new(move |entry: &ManagedEntry| Self::open_device(&state, entry))
                    as Rc<dyn Fn(&ManagedEntry)>
            };
            let open_settings_fn = {
                let state = Rc::clone(self_rc);
                Rc::new(move |ds| Self::open_settings(&state, ds))
                    as Rc<dyn Fn(Option<DeviceState>)>
            };
            *dw = Some(devlist::DiscoveryWindow::new(
                &self_rc.app,
                &self_rc.disc_mgr,
                open_device_fn,
                open_settings_fn,
            ));
        }
        dbg_state("device list: presenting");
        dw.as_ref().unwrap().present();
    }

    /// Present the existing device window for `entry`, or open a new one.
    fn open_device(self_rc: &Rc<Self>, entry: &ManagedEntry) {
        {
            let reg = self_rc.registry.borrow();
            for w in reg.iter() {
                if w.uuid().map_or(false, |u| u == entry.uuid) {
                    dbg_state(&format!("device window: presenting existing for {} ({})", entry.name, entry.uuid));
                    w.present();
                    return;
                }
            }
        }
        dbg_state(&format!("device window: opening {} ({}) @ {}", entry.name, entry.uuid, entry.ip));
        if !entry.uuid.is_empty() {
            config::update(|cfg| cfg.device_mut(&entry.uuid).window_open = true);
        }
        Self::open_device_spec(self_rc, DeviceSpec {
            ip:          entry.ip.clone(),
            uuid:        entry.uuid.clone(),
            tls_mode:    entry.tls_mode,
            try_connect: entry.presence == DevicePresence::Active,
        });
    }

    /// Create a device window for `spec`, register it, and present it.
    fn open_device_spec(self_rc: &Rc<Self>, spec: DeviceSpec) {
        let log_uuid = spec.uuid.clone();
        let log_ip   = spec.ip.clone();
        dbg_state(&format!("device window: creating uuid={log_uuid} @ {log_ip}"));
        let show_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move || Self::show_devices(&state)) as Rc<dyn Fn()>
        };
        let open_settings_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move |ds| Self::open_settings(&state, ds)) as Rc<dyn Fn(Option<DeviceState>)>
        };
        let dw = DeviceWindow::new_for_device(
            &self_rc.app,
            self_rc.device_manager.clone(),
            show_fn,
            open_settings_fn,
            spec,
        );
        let gtk_win   = dw.window.clone();
        dw.present();
        self_rc.registry.borrow_mut().push(dw);
        // Exempts this device from devlist's do_prune() for as long as this
        // window is open — see DeviceRecord::has_open_window's doc comment.
        // No-op if log_uuid is empty (uuid not resolved yet) or unknown to
        // devlist.
        self_rc.disc_mgr.set_window_open(&log_uuid, true);
        let win_key   = gtk_win.clone();
        let weak_self = Rc::downgrade(self_rc);
        gtk_win.connect_close_request({
            let log_uuid = log_uuid.clone();
            let win_key = win_key.clone();
            let weak_self = weak_self.clone();
            move |_| {
                dbg_state(&format!("device window: close-request uuid={log_uuid}"));
                if let Some(s) = weak_self.upgrade() {
                    let live_uuid = s.registry.borrow().iter()
                        .find(|w| w.window == win_key)
                        .and_then(|w| w.uuid());
                    s.registry.borrow_mut().retain(|w| w.window != win_key);
                    // Also close any Settings window open for this device.
                    // SettingsWindow holds a *strong* DeviceState clone
                    // (settings_reg, until the settings window itself
                    // closes) — without this, closing the device window
                    // leaves that strong clone alive, the DeviceState
                    // GObject never disposes, and polling keeps running
                    // indefinitely even though no window looks associated
                    // with the device anymore. Clone the window handle and
                    // drop the settings_reg borrow before calling close() —
                    // close() re-enters this same RefCell synchronously via
                    // its own close-request handler.
                    if let Some(uuid) = live_uuid.filter(|u| !u.is_empty()) {
                        let target = s.settings_reg.borrow().iter()
                            .find(|sw| sw.device_uuid().as_deref() == Some(uuid.as_str()))
                            .map(|sw| sw.window_ref().clone());
                        if let Some(win) = target {
                            win.close();
                        }
                    }
                }
                glib::Propagation::Proceed
            }
        });
        // Second connect_destroy: fires after new_inner's handler (connection order).
        // Removing from registry drops the last Rc<DeviceWindowInner>, triggering Drop.
        gtk_win.connect_destroy(move |_| {
            dbg_state(&format!("device window: destroyed uuid={log_uuid}"));
            if let Some(s) = weak_self.upgrade() {
                s.registry.borrow_mut().retain(|w| w.window != win_key);
                s.disc_mgr.set_window_open(&log_uuid, false);
            }
        });
    }

    /// Called once from `app.connect_activate`.
    pub(crate) fn activate(self_rc: &Rc<Self>) {
        {
            // update() only writes to disk if migrate() actually changed
            // something, so no need to check its return value here.
            config::update(|cfg| { cfg.migrate(); });
            let theme = config::with(|cfg| cfg.theme);
            init_css(theme);
            init_icon_resource();
        }

        // Replace the app.quit action (set up in main.rs) with one that explicitly
        // destroys every device window first so connect_destroy fires (saving
        // config, cancelling timers). win.close() is a no-op on unrealized
        // windows (e.g. a window never shown when starting in mini mode), and app.quit()
        // on its own destroys windows after the main loop exits where cleanup is unreliable.
        {
            let s = Rc::downgrade(self_rc);
            let app = self_rc.app.clone();
            let quit_action = gio::SimpleAction::new("quit", None);
            quit_action.connect_activate(move |_, _| {
                dbg_ui("quit action fired");
                QUITTING.store(true, Ordering::Relaxed);
                if let Some(s) = s.upgrade() {
                    // Collect first so connect_destroy (which mutates registry) doesn't
                    // invalidate the iterator.
                    let wins: Vec<_> = s.registry.borrow().iter()
                        .map(|dw| dw.window.clone())
                        .collect();
                    dbg_ui(&format!("quit: closing {} window(s)", wins.len()));
                    for win in wins {
                        // realize() first: close() is a no-op on unrealized windows
                        // (e.g. main window never shown when starting in mini mode).
                        gtk::prelude::WidgetExt::realize(&win);
                        win.close();
                    }
                } else {
                    dbg_ui("quit: AppState already freed");
                }
                app.quit();
            });
            self_rc.app.add_action(&quit_action);
        }

        // `--connect` override: skip discovery/config-restored windows entirely
        // and open exactly one device window straight at the given address.
        // uuid is empty (unresolved until getStatusEx) — DeviceManager::get()
        // and DeviceWindow::new_inner() already handle that case (a brand new,
        // not-yet-deduplicated DeviceState), same as for a manually-added device.
        if let Some((ip, tls_mode)) = DIRECT_CONNECT.get() {
            dbg_state(&format!("activate: --connect direct to {ip} via {tls_mode:?}"));
            Self::open_device_spec(self_rc, DeviceSpec {
                ip: ip.clone(),
                uuid: String::new(),
                tls_mode: *tls_mode,
                try_connect: true,
            });
            return;
        }

        // Reconnecting an already-open window to a corrected IP happens
        // directly inside `device::discovery_manager`'s own
        // `track_device()` the moment it detects a move (which then
        // triggers `list-changed`, persisting the correction via this
        // file's own listener above) — no separate `list-changed`-driven
        // pass needed here anymore (an earlier version of this
        // reconstructed "did the IP change" from a `list-changed` snapshot
        // diff, which is exactly the pattern that caused a real flapping
        // `Disconnected`/`Connecting…` bug for presence; not resurrecting
        // that shape for IP changes either).

        // Show the device list (if it should appear at all) *before*
        // starting discovery/restoring per-device windows below, so it
        // ends up at the bottom of the window stack instead of on top of
        // (potentially hiding) smaller device windows that open right
        // after it — GTK/GNOME gives no direct stacking-order control,
        // but a newly-presented window consistently lands above ones
        // already presented, so ordering these calls is the only lever
        // available. Reading `discovery_open`/`has_pending_windows`
        // directly from config rather than via `disc_mgr` — neither
        // depends on `start()` having run yet.
        let (discovery_open, has_pending_windows) = config::with(|cfg| (
            cfg.discovery_open,
            cfg.devices.values().any(|d| d.window_open),
        ));
        if discovery_open || !has_pending_windows {
            dbg_state("activate: showing device list");
            Self::show_devices(self_rc);
        }

        // Restore windows from config on startup.  initial-load fires once,
        // synchronously inside start(), so open_device() here is safe — no
        // risk of raising already-open windows on subsequent list changes.
        {
            let s = Rc::downgrade(self_rc);
            self_rc.disc_mgr.connect_initial_load(move |mgr| {
                let Some(self_rc) = s.upgrade() else { return };
                let entries = mgr.entries();
                let to_open: Vec<_> = config::with(|cfg| {
                    entries.into_iter()
                        .filter(|entry| !entry.uuid.is_empty()
                            && cfg.devices.get(&entry.uuid).map_or(false, |d| d.window_open))
                        .collect()
                });
                for entry in &to_open {
                    Self::open_device(&self_rc, entry);
                }
            });
        }

        // Seed the manager from config — it can't read config itself (same
        // rule `device::manager::DeviceManager` already follows). Must
        // happen before `start()`, which eagerly tracks the pinned/
        // window_open subset of this synchronously.
        let seed: Vec<SeedEntry> = config::with(|cfg| {
            cfg.devices.iter().map(|(uuid, d)| SeedEntry {
                uuid:        uuid.clone(),
                name:        d.name.clone(),
                model:       d.model.clone(),
                project:     d.project.clone(),
                firmware:    d.firmware.clone(),
                pinned:      d.pinned == Some(true),
                last_ip:     d.last_ip.clone(),
                tls_mode:    d.tls_mode.map(|n| TlsMode::from_usize(n as usize)).unwrap_or(TlsMode::HttpsWiiM),
                window_open: d.window_open,
            }).collect()
        });
        let devlist_song_info = config::with(|cfg| cfg.devlist_song_info);
        self_rc.disc_mgr.load_seed(seed, devlist_song_info);

        // `disc_mgr` can't persist to config itself either — this is the
        // "report out" half of the same rule, replacing what used to be an
        // internal `persist_pinned()` call scattered across several of its
        // own methods. Fires unconditionally on every `list-changed`
        // (pin toggle, identity update, presence flip, ...) rather than
        // being selectively triggered — cheap and safe since
        // `config::update()` already diffs the whole `Config` before
        // deciding whether to actually write to disk.
        self_rc.disc_mgr.connect_list_changed(|mgr| {
            let entries = mgr.entries();
            config::update(|cfg| {
                for e in &entries {
                    if e.uuid.is_empty() { continue; }
                    let dev = cfg.device_mut(&e.uuid);
                    dev.pinned = Some(e.pinned); // Explicit Some(true/false) ends legacy treatment.
                    dev.last_ip = Some(e.ip.clone());
                    dev.tls_mode = Some(e.tls_mode as u8);
                    dev.name = Some(e.name.clone());
                    if !e.model.is_empty()    { dev.model = Some(e.model.clone()); }
                    if !e.project.is_empty()  { dev.project = Some(e.project.clone()); }
                    if !e.firmware.is_empty() { dev.firmware = Some(e.firmware.clone()); }
                }
            });
        });

        self_rc.disc_mgr.start();
    }
}

// Private helper used within mod.rs new() — also accessible from child modules.
fn loop_api_mode(shuffle: bool, repeat: RepeatMode) -> i32 {
    match (shuffle, repeat) {
        (false, RepeatMode::Off) => 4,
        (false, RepeatMode::All) => 0,
        (false, RepeatMode::One) => 1,
        (true,  RepeatMode::Off) => 3,
        (true,  RepeatMode::All) => 2,
        (true,  RepeatMode::One) => 5,
    }
}
