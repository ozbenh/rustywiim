//! Theming: CSS providers (the three stylesheets + user accent color),
//! the adw color scheme, the embedded icon GResource, and the
//! widget-tree walks that re-sync theme-dependent widget state
//! (ArtBackground visibility, ScrollFadeLabel drop shadows) on a switch.
//!
//! Also `appearance_changed`/`broadcast_appearance_changed()`: a bitmask
//! broadcast mechanism any Appearance-page setting can plug a receiver
//! into, instead of growing its own plumbing (see that function's doc
//! comment).

use std::cell::RefCell;

use adw::prelude::*;
use gtk::gio;
use gtk::CssProvider;

use crate::config;
use crate::config::ThemeMode;
use super::{art_background, scroll_fade_label, APP_ID};

// ── CSS ───────────────────────────────────────────────────────────────────────

const SYSTEM_CSS: &str = include_str!("css/system.css");
const DARK_CSS: &str   = include_str!("css/dark.css");
// RustyWiiM Modern layers its own overrides (card panels, divider styling,
// etc.) on top of the classic dark palette rather than duplicating it.
const MODERN_CSS: &str = concat!(
    include_str!("css/dark.css"),
    include_str!("css/modern.css"),
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
pub(super) fn init_css(theme: ThemeMode) {
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
pub(super) fn init_icon_resource() {
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
    broadcast_appearance_changed(appearance_changed::ACCENT_COLOR);
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

/// Find every streaming-service brand-mark icon in `widget`'s subtree
/// (identified by which of the two force-polarity CSS classes it currently
/// carries — there's no dedicated widget type to downcast to here, it's a
/// plain `gtk::Image`) and re-apply the class matching the current theme's
/// light/dark polarity. Needed because `ServiceLabel::set_icon_polarity()`
/// (`ui/views/common.rs`) only runs when the icon's content changes, not on
/// a theme switch — without this, an icon set while dark stays forced-white
/// even after switching to a light theme, until the next track change.
fn set_brand_icon_polarity_recursive(widget: &gtk::Widget, dark: bool) {
    if widget.has_css_class("brand-icon-force-black") || widget.has_css_class("brand-icon-force-white") {
        if dark {
            widget.remove_css_class("brand-icon-force-black");
            widget.add_css_class("brand-icon-force-white");
        } else {
            widget.remove_css_class("brand-icon-force-white");
            widget.add_css_class("brand-icon-force-black");
        }
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        set_brand_icon_polarity_recursive(&c, dark);
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

    let dark = adw::StyleManager::default().is_dark();
    for win in gtk::Window::list_toplevels() {
        set_brand_icon_polarity_recursive(&win, dark);
    }

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

    broadcast_appearance_changed(appearance_changed::THEME);
}

// ── Appearance-change broadcast ─────────────────────────────────────────────
//
// A bitmask of which Appearance-page settings changed, so a single widget-
// tree walk can dispatch to whichever receivers care, instead of every new
// Appearance setting growing its own bespoke plumbing path. `apply_theme()`/
// `apply_accent_color()` call this alongside their own existing CSS-reload/
// art-background-visibility logic (which stays as-is, above); this function
// only owns the generic, walk-based reactions — currently just
// `SCROLL_SPEED`, but any bit can gain a receiver here later without
// touching call sites.

pub(crate) mod appearance_changed {
    pub const THEME:        u32 = 1 << 0;
    pub const ACCENT_COLOR: u32 = 1 << 1;
    pub const SCROLL_SPEED: u32 = 1 << 2;
}

pub(crate) fn broadcast_appearance_changed(mask: u32) {
    if mask & appearance_changed::SCROLL_SPEED != 0 {
        // Every ScrollFadeLabel gets the same base speed here — each
        // instance applies its own fixed `speed_multiplier` on top (see
        // `ScrollFadeLabel::with_speed_multiplier()`), so this walker
        // doesn't need to know or care which window a label lives in.
        let speed = config::with(|cfg| cfg.scroll_speed);
        for win in gtk::Window::list_toplevels() {
            set_scroll_speed_recursive(&win, speed);
        }
    }
}

fn set_scroll_speed_recursive(widget: &gtk::Widget, speed: f64) {
    if let Some(label) = widget.downcast_ref::<scroll_fade_label::ScrollFadeLabel>() {
        label.set_speed(speed);
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        set_scroll_speed_recursive(&c, speed);
        child = c.next_sibling();
    }
}

/// Fixed rotation order for the "T" quick-switch shortcut (`DeviceWindow`
/// and `KioskWindow` both call this) — same order Settings' own theme
/// dropdown displays them in, minus its purely-visual separator entry.
const CYCLE_ORDER: &[ThemeMode] = &[
    ThemeMode::System,
    ThemeMode::SystemLight,
    ThemeMode::SystemDark,
    ThemeMode::RustyWiiM,
    ThemeMode::RustyWiiMModern,
];

/// Advances `config.theme` to the next entry in `CYCLE_ORDER` (wrapping) and
/// applies it live — a quick way to eyeball a change under every theme
/// without opening Settings each time.
pub(crate) fn cycle_theme() {
    let current = config::with(|cfg| cfg.theme);
    let pos = CYCLE_ORDER.iter().position(|t| *t == current).unwrap_or(0);
    let next = CYCLE_ORDER[(pos + 1) % CYCLE_ORDER.len()];
    config::update(|cfg| cfg.theme = next);
    apply_theme(next);
}
