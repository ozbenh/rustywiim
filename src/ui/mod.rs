#![allow(deprecated)] // glib clone! old-style @strong syntax

mod art_background;
pub mod devlist;
mod flip_cover;
mod icons;
pub(crate) mod menu;
mod scroll_fade_label;
mod playback;
pub(crate) mod settings;
mod widgets;

use playback::decode_loop_mode;
use widgets::*;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use adw::prelude::*;
use glib::clone;
use gtk::gio;
use gtk::{Align, Box as GtkBox, CssProvider, Label, Orientation, Scale};

use crate::device::api::TlsMode;
use crate::config;
use crate::config::ThemeMode;
use crate::device::discovery::DiscoveryService;
use crate::device::manager::DeviceManager;
use crate::device::state::{ConnectionState, DeviceState, DEBUG_STATE};

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
            .application_icon("audio-x-generic")
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
    show_devices_fn:  Rc<dyn Fn()>,
    sw:             SourceWidgets,
    ow:             OutputWidgets,
    pw:             PlaybackWidgets,
    pp:             PresetWidgets,
    dev_info_label: Label,
    ip_label:       Label,
    net_icon:       gtk::Image,
    icons:          Rc<icons::IconSet>,
    vol_scale:      Scale,
    ui_state:       PlaybackUiState,
    // Window / panel state — kept here so device-change and close handlers
    // only need one Rc<Inner> capture.
    window:              adw::ApplicationWindow,
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
    /// SSID for which window state was last applied; guards against
    /// re-applying on every device-changed fire for the same device.
    applied_window_key: RefCell<String>,
    /// Guards `cleanup()` so its body only actually runs once, even though
    /// it's invoked from both `close-request` and `connect_destroy` for a
    /// single user-initiated close. Nothing in `cleanup()` benefits from
    /// running twice, and computing "is this the last visible window"
    /// specifically needs to *not* re-run on the second call, since by then
    /// the window may be torn down enough for that to (wrongly) come out
    /// differently — a single guard on the whole function is simpler than
    /// caching that one value.
    cleaned_up: Cell<bool>,
    // ── Mini player ───────────────────────────────────────────────────────────
    mini:              MiniWidgets,
    mini_mode:         RefCell<bool>,
    mini_toggling:     RefCell<bool>,
    pre_mini_size:     RefCell<(i32, i32)>,
    mini_btn:          gtk::ToggleButton,
    mini_win:          gtk::ApplicationWindow,
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
        // don't count as "another visible window" here — closing
        // your last device window while a Settings window happens to be
        // open still preserves it. self.mini_win is excluded too: it's the
        // *other surface of this same device window*, not a separate one —
        // closing via mini.close_btn calls window.close() while mini_win is
        // still visible (nothing hides it until this function's last line),
        // so without this exclusion that path always saw "another visible
        // window" and never recognized itself as the last one.
        let last_window = self.window.application().is_some_and(|app| {
            !app.windows().iter().any(|w| {
                w.upcast_ref::<gtk::Widget>() != self.window.upcast_ref::<gtk::Widget>()
                    && w.upcast_ref::<gtk::Widget>() != self.mini_win.upcast_ref::<gtk::Widget>()
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
        self.mini_win.destroy();
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
            Some(spec) => device_manager.get(&spec.uuid, &spec.ip, spec.tls_mode),
            None => {
                // No device spec: create a standalone state that isn't wired to
                // any device yet; polling still starts so the UI can be shown.
                let ds = DeviceState::new(device_manager.rt());
                ds.start_polling();
                ds
            }
        };

        dbg_ui(&format!("DeviceWindow creating (uuid={})", cfg_uuid));

        let (header, sidebar_btn, mini_btn) = build_header(init_dev_cfg.panel_visible);
        let (pp, presets_scroll) = build_presets_panel();
        let sw = build_source_widgets(&icons);
        let ow = build_output_widgets(&icons);
        let left_pane = build_left_pane(&sw, &ow, &presets_scroll);
        let (pw, vol_scale) = build_playback_widgets();
        let right_pane = build_right_pane(&pw);
        let (mini, mini_win) = build_mini_window(app);

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

        let bottom_bar = gtk::CenterBox::new();
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
        // Initial visibility (for both this and the mini window's own
        // art_bg) is set once both windows exist, below.
        let art_bg = art_background::ArtBackground::new();
        art_bg.set_hexpand(true);
        art_bg.set_vexpand(true);
        let window_overlay = gtk::Overlay::new();
        window_overlay.set_child(Some(&art_bg));
        window_overlay.add_overlay(&full_toolbar);

        let win_w = if init_dev_cfg.window_width  > 0 { init_dev_cfg.window_width  } else { 680 };
        let win_h = if init_dev_cfg.window_height > 0 { init_dev_cfg.window_height } else { 640 };
        let window = adw::ApplicationWindow::builder()
            .application(app).title("RustyWiiM").content(&window_overlay)
            .default_width(win_w).default_height(win_h)
            .build();
        window.add_css_class("player-window");
        if init_dev_cfg.window_maximized { window.maximize(); }

        // apply_theme() only fires on explicit runtime switches, so a window
        // (main or mini, both now built) opened after the app already
        // started on some theme needs its initial art_bg visibility set
        // directly from the live config.
        update_art_background_visibility();

        // ── Shared UI state ───────────────────────────────────────────────────────
        let ui_state = PlaybackUiState {
            is_playing:   Rc::new(RefCell::new(false)),
            drag_timer:   Rc::new(RefCell::new(None)),
        };

        let inner = Rc::new(DeviceWindowInner {
            ds: ds.clone(),
            show_devices_fn,
            sw,
            ow,
            pw,
            pp,
            dev_info_label,
            ip_label,
            net_icon,
            icons,
            vol_scale,
            ui_state,
            window: window.clone(),
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
            cleaned_up: Cell::new(false),
            mini,
            mini_mode:         RefCell::new(false),
            mini_toggling:     RefCell::new(false),
            pre_mini_size:     RefCell::new((0, 0)),
            mini_btn:          mini_btn.clone(),
            mini_win:          mini_win.clone(),
        });

        // ── DeviceState signal connections ────────────────────────────────────────
        // Use Rc::downgrade so the closures don't keep DeviceWindowInner alive
        // after the window is closed — broken upgrade() calls become no-ops.
        ds.connect_device_changed({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.populate_all(); } }
        });

        ds.connect_network_changed({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.update_network_icon(); } }
        });

        ds.connect_playback_changed({
            let i = Rc::downgrade(&inner);
            move |_, mask| {
                let Some(i) = i.upgrade() else { return };
                if *i.mini_mode.borrow() { i.update_mini_playback(mask); } else { i.update_playback_ui(mask); }
            }
        });

        ds.connect_input_changed({
            let i = Rc::downgrade(&inner);
            move |_| {
                let Some(i) = i.upgrade() else { return };
                if *i.mini_mode.borrow() { i.update_mini_playback(crate::device::state::playback_changed::ALL); } else { i.update_input_display(); }
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

        ds.connect_presets_changed({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.on_presets_changed(); } }
        });

        // Populate immediately from whatever the DeviceState already has cached.
        inner.populate_all();

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

        // ── Keyboard shortcuts (main window) ────────────────────────────────────
        // Capture phase: must win over a focused seek/volume Scale's own
        // Left/Right/Up/Down handling, since the whole point is a global
        // shortcut that works regardless of what has focus.
        {
            let key_ctrl = gtk::EventControllerKey::new();
            key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
            key_ctrl.connect_key_pressed({
                let i = Rc::downgrade(&inner);
                move |_, keyval, _keycode, state| {
                    let Some(i) = i.upgrade() else { return glib::Propagation::Proceed };
                    let (prev, next, play) = (i.pw.btn_prev.clone(), i.pw.btn_next.clone(), i.pw.btn_play.clone());
                    playback::handle_transport_key(&i, keyval, state, &prev, &next, &play)
                }
            });
            window.add_controller(key_ctrl);
        }

        inner.pw.shuffle.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| {
                let Some(i) = i.upgrade() else { return };
                let (shuf, rep) = i.ds.player_status()
                    .map(|s| decode_loop_mode(&s.loop_mode))
                    .unwrap_or((false, 0));
                i.ds.do_set_loop_mode(loop_api_mode(!shuf, rep));
            }
        });

        inner.pw.repeat.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| {
                let Some(i) = i.upgrade() else { return };
                let (shuf, rep) = i.ds.player_status()
                    .map(|s| decode_loop_mode(&s.loop_mode))
                    .unwrap_or((false, 0));
                i.ds.do_set_loop_mode(loop_api_mode(shuf, (rep + 1) % 3));
            }
        });

        inner.pw.vol_btn.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| {
                let Some(i) = i.upgrade() else { return };
                if i.pw.vol_popover.is_visible() { i.pw.vol_popover.popdown(); }
                else { i.pw.vol_popover.popup(); }
            }
        });

        inner.pw.mute_btn.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.ds.do_set_mute(!i.ds.muted()); } }
        });

        inner.vol_scale.connect_change_value({
            let i = Rc::downgrade(&inner);
            move |_, _, vol| {
                if let Some(i) = i.upgrade() { i.on_vol_changed(vol); }
                glib::Propagation::Proceed
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

        for (idx, btn) in inner.pp.btns.iter().enumerate() {
            let num = (idx + 1) as u32;
            let i = Rc::downgrade(&inner);
            btn.connect_clicked(move |_| {
                let Some(i) = i.upgrade() else { return };
                if let Some(c) = i.ds.client() {
                    i.ds.rt().spawn(async move { let _ = c.play_preset(num).await; });
                }
            });
        }

        // ── Mini player signals ───────────────────────────────────────────────────
        inner.mini_btn.connect_toggled({
            let i = Rc::downgrade(&inner);
            move |btn| {
                let Some(i) = i.upgrade() else { return };
                if *i.mini_toggling.borrow() { return; }
                if btn.is_active() { i.enter_mini_mode(); } else { i.exit_mini_mode(); }
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

        inner.mini.close_btn.connect_clicked(clone!(@strong window => move |_| {
            gtk::prelude::WidgetExt::realize(&window); // close() is a no-op on an unrealized window
            window.close();
        }));

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

        inner.mini.vol_btn.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| {
                let Some(i) = i.upgrade() else { return };
                if i.mini.vol_popover.is_visible() { i.mini.vol_popover.popdown(); }
                else { i.mini.vol_popover.popup(); }
            }
        });

        inner.mini.mute_btn.connect_clicked({
            let i = Rc::downgrade(&inner);
            move |_| { if let Some(i) = i.upgrade() { i.ds.do_set_mute(!i.ds.muted()); } }
        });

        inner.mini.vol_scale.connect_change_value({
            let i = Rc::downgrade(&inner);
            move |_, _, vol| {
                if let Some(i) = i.upgrade() { i.on_vol_changed(vol); }
                glib::Propagation::Proceed
            }
        });

        // ── Keyboard shortcuts (mini window) ────────────────────────────────────
        {
            let key_ctrl = gtk::EventControllerKey::new();
            key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
            key_ctrl.connect_key_pressed({
                let i = Rc::downgrade(&inner);
                move |_, keyval, _keycode, state| {
                    let Some(i) = i.upgrade() else { return glib::Propagation::Proceed };
                    let (prev, next, play) = (i.mini.btn_prev.clone(), i.mini.btn_next.clone(), i.mini.btn_play.clone());
                    playback::handle_transport_key(&i, keyval, state, &prev, &next, &play)
                }
            });
            inner.mini_win.add_controller(key_ctrl);
        }

        // ── Mini window signals ───────────────────────────────────────────────────
        // X / Alt+F4 on the mini window → exit mini mode (don't destroy the window).
        inner.mini_win.connect_close_request({
            let i = Rc::downgrade(&inner);
            move |_win| {
                if let Some(i) = i.upgrade() {
                    i.exit_mini_mode();
                    playback::schedule_config_save(&i);
                }
                glib::Propagation::Stop
            }
        });

        // ── Window actions ────────────────────────────────────────────────────────
        // Main window: win.close (Ctrl-W), win.devices, win.about, win.settings.
        let close_action = gio::SimpleAction::new("close", None);
        let win_for_close = window.clone();
        close_action.connect_activate(move |_, _| { win_for_close.close(); });
        window.add_action(&close_action);

        let devices_action = gio::SimpleAction::new("devices", None);
        {
            let i = Rc::downgrade(&inner);
            devices_action.connect_activate(move |_, _| {
                if let Some(i) = i.upgrade() { (i.show_devices_fn)(); }
            });
        }
        window.add_action(&devices_action);

        wire_window_actions(&window, Some(ds.clone()), Rc::clone(&open_settings));

        // Mini window is a gtk::ApplicationWindow, so app.* actions (Ctrl-Q) work
        // automatically.  Wire win.close and win.devices directly; win.about and
        // win.settings come from wire_window_actions.
        {
            let mini_close = gio::SimpleAction::new("close", None);
            let win = window.clone();
            mini_close.connect_activate(move |_, _| {
                gtk::prelude::WidgetExt::realize(&win);
                win.close();
            });
            mini_win.add_action(&mini_close);

            let mini_devices = gio::SimpleAction::new("devices", None);
            {
                let i = Rc::downgrade(&inner);
                mini_devices.connect_activate(move |_, _| {
                    if let Some(i) = i.upgrade() { (i.show_devices_fn)(); }
                });
            }
            mini_win.add_action(&mini_devices);
        }
        wire_window_actions(&mini_win, Some(ds.clone()), open_settings);

        // close-request: fires on user close (X button, win.close()).
        // cleanup() is idempotent so calling it here AND in connect_destroy is safe.
        window.connect_close_request({
            let i = Rc::downgrade(&inner);
            move |_win| {
                dbg_ui("main window close-request");
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
            // Set up mini-mode state without calling enter_mini_mode(), which would
            // try to hide a window not yet shown and call mini_win.present() too early.
            // DeviceWindow::present() (called by the caller) will show the mini window.
            *inner.mini_mode.borrow_mut() = true;
            *inner.mini_toggling.borrow_mut() = true;
            inner.mini_btn.set_active(true);
            *inner.mini_toggling.borrow_mut() = false;
            // Seed pre_mini_size from saved config so exit_mini_mode() can restore
            // the right size even before the main window has ever been realised.
            *inner.pre_mini_size.borrow_mut() = (win_w, win_h);
        }

        Self { window, inner }
    }

    pub fn present(&self) {
        if *self.inner.mini_mode.borrow() {
            self.inner.mini_win.present();
        } else {
            self.window.present();
        }
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
    disc_mgr:       devlist::DiscoveryManager,
    device_manager: DeviceManager,
    registry:       RefCell<Vec<DeviceWindow>>,
    settings_reg:   RefCell<Vec<settings::SettingsWindow>>,
    disc_win:       RefCell<Option<devlist::DiscoveryWindow>>,
}

impl AppState {
    // `disc_svc.start()` must run inside `connect_activate` so that
    // `glib::spawn_future_local` has an active main context.
    pub(crate) fn new(app: &adw::Application, rt: Arc<tokio::runtime::Runtime>) -> Rc<Self> {
        let disc_svc = DiscoveryService::new(rt.clone());
        disc_svc.start();
        let disc_mgr = devlist::DiscoveryManager::new(rt.clone(), disc_svc.clone());

        Rc::new(Self {
            app:            app.clone(),
            disc_mgr,
            device_manager: DeviceManager::new(rt),
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
        let s = settings::SettingsWindow::new(ds);
        let win_clone  = s.window_ref().clone();
        let weak_self  = Rc::downgrade(self_rc);
        let close_uuid = ds_uuid.clone();
        s.window_ref().connect_close_request(move |_| {
            dbg_state(&format!("settings: closed for {:?}", close_uuid));
            if let Some(state) = weak_self.upgrade() {
                state.settings_reg.borrow_mut().retain(|w| w.window_ref() != &win_clone);
            }
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
                Rc::new(move |entry: &devlist::ManagedEntry| Self::open_device(&state, entry))
                    as Rc<dyn Fn(&devlist::ManagedEntry)>
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
    fn open_device(self_rc: &Rc<Self>, entry: &devlist::ManagedEntry) {
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
            ip:       entry.ip.clone(),
            uuid:     entry.uuid.clone(),
            tls_mode: entry.tls_mode,
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
        let win_key   = gtk_win.clone();
        let weak_self = Rc::downgrade(self_rc);
        gtk_win.connect_close_request({
            let log_uuid = log_uuid.clone();
            let win_key = win_key.clone();
            let weak_self = weak_self.clone();
            move |_| {
                dbg_state(&format!("device window: close-request uuid={log_uuid}"));
                if let Some(s) = weak_self.upgrade() {
                    s.registry.borrow_mut().retain(|w| w.window != win_key);
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
        }

        // Replace the app.quit action (set up in main.rs) with one that explicitly
        // destroys every device window first so connect_destroy fires (saving config,
        // cancelling timers, destroying mini_win).  win.close() is a no-op on unrealized
        // windows (e.g. main window never shown when starting in mini mode), and app.quit()
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

        // Keep last_ip current, and reconnect any already-open device window
        // to a corrected IP, whenever the device list changes.  Must be
        // connected before start() so we catch the synchronous emission after
        // the initial config load.
        {
            let s = Rc::downgrade(self_rc);
            self_rc.disc_mgr.connect_list_changed(move |mgr| {
                let Some(self_rc) = s.upgrade() else { return };
                let entries = mgr.entries();
                for entry in &entries {
                    if entry.uuid.is_empty() { continue; }
                    // No-op if a live DeviceState for this UUID is already
                    // using this IP; otherwise reconnects it (see
                    // ANALYSIS.md #1 — devlist.rs refreshes entry.ip on
                    // rediscovery, this pushes the correction into any open
                    // window instead of it retrying the old dead IP forever).
                    self_rc.device_manager.update_ip(&entry.uuid, &entry.ip, entry.tls_mode);
                }
                // update() only saves if something actually changed, so no
                // need to track a separate "dirty" flag here.
                config::update(|cfg| {
                    for entry in &entries {
                        if entry.uuid.is_empty() { continue; }
                        let Some(dev) = cfg.devices.get_mut(&entry.uuid) else { continue };
                        if dev.last_ip.as_deref() != Some(entry.ip.as_str()) {
                            dev.last_ip = Some(entry.ip.clone());
                        }
                    }
                });
            });
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

        self_rc.disc_mgr.start();

        let (discovery_open, has_pending_windows) = config::with(|cfg| (
            cfg.discovery_open,
            cfg.devices.values().any(|d| d.window_open),
        ));
        if discovery_open || !has_pending_windows {
            dbg_state("activate: showing device list");
            Self::show_devices(self_rc);
        }
    }
}

// Private helper used within mod.rs new() — also accessible from child modules.
fn loop_api_mode(shuffle: bool, repeat: u32) -> i32 {
    match (shuffle, repeat) {
        (false, 0) => 4,
        (false, 1) => 0,
        (false, 2) => 1,
        (true,  0) => 3,
        (true,  1) => 2,
        (true,  2) => 5,
        _           => 4,
    }
}
