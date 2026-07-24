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

const SYSTEM_CSS: &str = include_str!("themes/system/system.css");
const DARK_CSS: &str   = include_str!("themes/dark/dark.css");
// RustyWiiM Modern layers its own overrides (card panels, divider styling,
// etc.) on top of the classic dark palette rather than duplicating it.
const MODERN_CSS: &str = concat!(
    include_str!("themes/dark/dark.css"),
    include_str!("themes/modern/modern.css"),
);
// RustyWiiM Wood: same "overrides on top of dark.css" layering as Modern.
// Its background panel textures are `url("resource:///...")` references
// resolved against the embedded icon GResource bundle (see
// `init_icon_resource()`'s doc comment on load-order below), not loaded
// via `include_bytes!` here.
const WOOD_CSS: &str = concat!(
    include_str!("themes/dark/dark.css"),
    include_str!("themes/wood/wood.css"),
);

thread_local! {
    static THEME_PROVIDER: RefCell<Option<CssProvider>> = const { RefCell::new(None) };
}

fn theme_css(theme: ThemeMode) -> &'static str {
    match theme {
        ThemeMode::RustyWiiM       => DARK_CSS,
        ThemeMode::RustyWiiMModern => MODERN_CSS,
        ThemeMode::RustyWiiMWood   => WOOD_CSS,
        _                          => SYSTEM_CSS,
    }
}

// ── Tunables ─────────────────────────────────────────────────────────────────
//
// Behavioral (and, since FlipCover's theme-drawn artwork frame, small
// theme-authored data) values a theme needs Rust code — not just CSS — to
// know about. Two distinct reasons a value ends up here instead of in CSS:
// CSS alone can make an *already-decided-to-run* behavior invisible, but
// only Rust deciding not to run it at all avoids the cost of running it
// (see `update_art_background_visibility()`'s doc comment above for the
// same point about `queue_draw`/measure-and-snapshot cost) — e.g.
// `kiosk_keep_transport_visible`; or the drawing itself is Rust-side GSK code,
// not CSS, and needs its colors from somewhere — e.g. `frame_*` (GTK CSS's
// only queryable-from-Rust "custom property" channel, `@define-color` +
// `StyleContext::lookup_color()`, is deprecated since GTK 4.10 — this
// crate targets 4.12 — so theme.yaml is the mechanism for this case too,
// not a GTK CSS feature).
//
// Each theme's `theme.yaml` (absent = every field defaults) is
// `include_str!`'d and parsed once per theme switch into `CURRENT_TUNABLES`
// (below), not on every access — `FlipCover::snapshot()` reads tunables on
// every repaint (including every frame of a flip/fade transition), so
// unlike `resolved_accent_color()`'s "cheap enough to just recompute"
// choice, re-parsing YAML there really would be per-frame cost, not a one-
// off. Bundled at build time today like every other Tier-1 theme asset;
// deliberately real standalone files rather than a Rust match/struct
// literal so a future runtime-loaded theme pack can bring its own `theme.yaml`
// with no code changes here beyond swapping `include_str!`
// for `std::fs::read_to_string()`.

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ThemeTunables {
    /// Exempt this theme's transport controls (`PlaybackView::fade_group()`
    /// — transport buttons, volume, EQ) specifically from Kiosk's idle
    /// auto-hide, even when the user's own `kiosk_auto_hide_all_controls`
    /// is on — for a theme whose controls are styled to look like
    /// permanent physical hardware (knobs/buttons), fading them out on
    /// idle reads as broken hardware, not an intentional affordance.
    /// Deliberately narrower than an earlier version of this flag, which
    /// vetoed Kiosk's auto-hide entirely: the top buttons (device-select/
    /// sidebar/exit/settings) and the bottom status bar still auto-hide
    /// normally regardless of this — only the transport group opts out.
    pub kiosk_keep_transport_visible: bool,
    /// Artwork frame colors for `FlipCover`'s theme-drawn raised-edge
    /// frame (`flip_cover.rs`'s `draw_theme_frame()`) — CSS color strings
    /// (`gdk::RGBA::parse()`-compatible, e.g. `"rgba(255, 220, 180, 0.35)"`),
    /// not structured color values, so `theme.yaml` stays a plain data
    /// file with no bespoke color syntax of its own. All three `None`
    /// (every theme but Wood today) means no frame is drawn at all —
    /// `FlipCover` doesn't get a hardcoded fallback color, since "this
    /// theme has no opinion" and "this theme wants an invisible frame"
    /// need to be distinguishable, and only the former is meant here.
    pub frame_highlight: Option<String>,
    pub frame_shadow:    Option<String>,
    pub frame_glow:      Option<String>,
    /// Whether Kiosk mode's WideRight layout groups the seek bar, service/
    /// quality row, and transport controls into one shared card (styled
    /// like the normal device window's own `.controls-card`) instead of
    /// its default arrangement (transport in its own small card; service
    /// and seek bar loose, uncarded, elsewhere in the column). A plain
    /// bool rather than a "style1"/"style2" enum for now — only one
    /// alternative arrangement exists to choose; generalize to an enum if
    /// a second one is ever actually built, not preemptively. Read once,
    /// at `PlaybackView` construction time (`playback_full.rs`) — a
    /// structural widget-tree choice, not something a live CSS reload can
    /// re-apply the way color/behavior tunables can, so switching themes
    /// live doesn't restructure an already-built view; it takes effect
    /// the next time one is constructed (e.g. Kiosk's own device switch).
    pub kiosk_boxed_controls: bool,
    /// Wraps Kiosk mode's WideRight title/artist/album column in a "VFD"
    /// (vacuum-fluorescent-display) panel: `wood.css`'s own `.vfd-panel`
    /// class handles the panel's background/border/scanlines (ordinary CSS,
    /// no Rust involvement), but the glowing-text look behind title/artist/
    /// album is real GSK blur layering in `ScrollFadeLabel`
    /// (`ScrollFadeLabel::set_glow()`) — a manually-rendered widget gets no
    /// CSS `text-shadow` for free, the same reason `drop_shadow` already
    /// exists there. Off by default (every theme but Wood) so this is
    /// opt-in, not something a theme has to explicitly disable. Read once
    /// at `PlaybackView` construction, same "not live-reactive" contract as
    /// `kiosk_boxed_controls` above — and, like that field, only ever
    /// actually built when the *caller* also says it's Kiosk (`is_kiosk` in
    /// `PlaybackView::new()`), so Wood's normal-mode WideRight toggle
    /// (`device_window`'s own "L") stays unaffected.
    pub vfd_panel: bool,
    /// Phosphor glow color behind title/artist/album text when `vfd_panel`
    /// is on — `gdk::RGBA::parse()`-compatible, same convention as
    /// `frame_*` above. `None` (the default) means no glow is drawn even if
    /// `vfd_panel` is on; artist/album get a dimmer version of this same
    /// color (see `playback_full.rs`'s WideRight branch), not a second
    /// configured color, matching a real VFD's single tube color at two
    /// brightnesses.
    pub vfd_glow_color: Option<String>,
}

const WOOD_TUNABLES_YAML: &str = include_str!("themes/wood/theme.yaml");

fn tunables_yaml(theme: ThemeMode) -> Option<&'static str> {
    match theme {
        ThemeMode::RustyWiiMWood => Some(WOOD_TUNABLES_YAML),
        _                        => None,
    }
}

/// `theme`'s tunables, parsed fresh from its `theme.yaml` (see the module
/// doc comment above) — `ThemeTunables::default()` for a theme with no
/// file, or if the file fails to parse (a malformed *bundled* theme.yaml
/// is a build-time authoring mistake to fix, not something that should be
/// able to crash a running app — logged via `eprintln!` since this has no
/// dedicated debug flag of its own, unlike `--debug=` gated tracing
/// elsewhere in this codebase, on the expectation that it's rare enough
/// not to warrant one). Not the hot-path accessor — see `current_tunables()`.
fn theme_tunables(theme: ThemeMode) -> ThemeTunables {
    let Some(yaml) = tunables_yaml(theme) else { return ThemeTunables::default() };
    match serde_yaml::from_str(yaml) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("theme.rs: bad theme.yaml for {theme:?}, using defaults: {e}");
            ThemeTunables::default()
        }
    }
}

thread_local! {
    static CURRENT_TUNABLES: RefCell<ThemeTunables> = RefCell::new(ThemeTunables::default());
}

/// Re-parses `theme`'s `theme.yaml` into `CURRENT_TUNABLES` — called
/// alongside every CSS-provider reload (`init_css()`/`apply_theme()`,
/// below) since a tunables change is always a theme-switch, never
/// independent of one (unlike the accent color, which can change without
/// a theme switch, so has no equivalent cache).
fn refresh_tunables(theme: ThemeMode) {
    CURRENT_TUNABLES.with(|c| *c.borrow_mut() = theme_tunables(theme));
}

/// The active theme's tunables — cheap (a `Clone` of a few small fields,
/// no parsing), safe to call from a hot path like `FlipCover::snapshot()`.
pub(crate) fn current_tunables() -> ThemeTunables {
    CURRENT_TUNABLES.with(|c| c.borrow().clone())
}

/// The effective accent color for `theme`: the user's override
/// (`config.accent_color`) if the "Override accent color" switch in
/// Settings is on, otherwise `theme`'s own built-in default — see
/// `config::default_accent_for_theme()`'s doc comment for why that's a
/// per-theme table rather than one fixed fallback.
fn resolved_accent_color(theme: ThemeMode) -> String {
    config::with(|cfg| cfg.accent_color.clone())
        .unwrap_or_else(|| config::default_accent_for_theme(theme).to_string())
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
        ThemeMode::RustyWiiMWood   => adw::ColorScheme::ForceDark,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

/// Initialise the CSS provider for the current process.  Must be called once.
pub(super) fn init_css(theme: ThemeMode) {
    apply_color_scheme(theme);
    let accent = resolved_accent_color(theme);
    reload_css_provider(&build_css(theme, &accent));
    refresh_tunables(theme);
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
/// by name. Must be called once, after the GDK display is available, and
/// before `init_css()` — GResource registration (`gio::resources_register`)
/// is what makes `resource:///...` URIs resolvable at all, and the Wood
/// theme's stylesheet references its texture assets that way (see
/// `WOOD_CSS`'s doc comment).
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
    let accent = resolved_accent_color(theme);
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

    let accent = resolved_accent_color(theme);
    reload_css_provider(&build_css(theme, &accent));
    refresh_tunables(theme);

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
    ThemeMode::RustyWiiMWood,
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
