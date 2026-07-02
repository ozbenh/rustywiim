#![allow(deprecated)] // glib clone! old-style @strong syntax

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;
use gtk::{Align, Box as GtkBox, Button, Label, Orientation, Scale};

use super::art_background;
use super::flip_cover::FlipCover;
use super::icons;
use super::scroll_fade_label::ScrollFadeLabel;

// ── Widget bundles ────────────────────────────────────────────────────────────
// Grouping related widgets + associated state into structs keeps signal-handler
// signatures short and the closures easy to read.

#[derive(Clone)]
pub(crate) struct SourceWidgets {
    pub dropdown: gtk::DropDown,
    pub ids:      Rc<RefCell<Vec<String>>>,
    pub enabled:  Rc<RefCell<Vec<bool>>>,
    pub updating: Rc<RefCell<bool>>,
}

#[derive(Clone)]
pub(crate) struct OutputWidgets {
    pub dropdown:    gtk::DropDown,
    pub section:     GtkBox,
    pub modes:       Rc<RefCell<Vec<u32>>>,
    pub canon_names: Rc<RefCell<Vec<&'static str>>>,
    pub updating:    Rc<RefCell<bool>>,
}

#[derive(Clone)]
pub(crate) struct PresetWidgets {
    pub btns:   Rc<Vec<Button>>,
    pub pics:   Rc<Vec<gtk::Image>>,
    pub labels: Rc<Vec<Label>>,
}

/// Two `ScrollFadeLabel`s in a `gtk::Stack`, slid between on text change
/// instead of a hard swap (ANIMATIONS.md Tier 1). `set_text()` matches
/// `ScrollFadeLabel`'s own signature, so call sites don't change.
#[derive(Clone)]
pub(crate) struct SwipeText {
    pub stack: gtk::Stack,
    a: ScrollFadeLabel,
    b: ScrollFadeLabel,
}

impl SwipeText {
    fn new(initial: &str, css_class: &str, center_when_fits: bool, drop_shadow: bool) -> Self {
        let a = ScrollFadeLabel::new(initial);
        let b = ScrollFadeLabel::new("");
        for l in [&a, &b] {
            l.add_label_css_class(css_class);
            l.set_hexpand(true);
            l.set_center_when_fits(center_when_fits);
            l.set_drop_shadow(drop_shadow);
        }
        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.set_transition_duration(250);
        stack.add_named(&a, Some("a"));
        stack.add_named(&b, Some("b"));
        stack.set_visible_child_name("a");
        Self { stack, a, b }
    }

    /// Swap to `text`, sliding the new label in over the old one.
    /// No-op if `text` already matches what's currently shown.
    pub fn set_text(&self, text: &str) {
        let showing_a = self.stack.visible_child_name().as_deref() == Some("a");
        let (outgoing, incoming, name) =
            if showing_a { (&self.a, &self.b, "b") } else { (&self.b, &self.a, "a") };
        // Compare against what's actually on screen (outgoing), not the
        // hidden face — the hidden face still holds leftover text from
        // *two* changes ago, so comparing against it wrongly no-ops (and
        // leaves the stale visible label on screen) whenever a new title
        // happens to coincide with that stale leftover, e.g. a repeated
        // track title or a transient empty-title flicker that reverts.
        if outgoing.text() == text { return; }
        incoming.set_text(text);
        let transition = if crate::config::with(|cfg| cfg.animations) {
            gtk::StackTransitionType::SlideLeft
        } else {
            gtk::StackTransitionType::None
        };
        self.stack.set_visible_child_full(name, transition);
    }
}

#[derive(Clone)]
pub(crate) struct PlaybackWidgets {
    pub title:      SwipeText,
    pub artist:     SwipeText,
    pub album:      SwipeText,
    pub status:     Label,
    pub quality:    Label,
    pub pos:        Label,
    pub dur:        Label,
    pub seek:       Scale,
    pub btn_prev:    Button,
    pub btn_play:    Button,
    pub btn_next:    Button,
    pub shuffle:     Button,
    pub repeat:      Button,
    pub vol_btn:      Button,
    pub vol_icon_img: gtk::Image,
    pub vol_label:    gtk::Label,
    pub vol_popover:  gtk::Popover,
    pub mute_btn:     Button,
    pub artwork:     FlipCover,
}

// ── Playback UI state ─────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct PlaybackUiState {
    pub is_playing:   Rc<RefCell<bool>>,
    // Set while the user is dragging the volume slider (or within 500ms after).
    // Prevents poll updates from jumping the slider back mid-drag.
    pub drag_timer:   Rc<RefCell<Option<glib::SourceId>>>,
}

pub(crate) struct MiniWidgets {
    pub root:          gtk::WindowHandle,
    pub art_bg:        art_background::ArtBackground,
    pub artwork:       FlipCover,
    pub device_label:  Label,
    #[allow(dead_code)] // owned for lifetime; the widget is parented to the top bar
    pub menu_btn:      gtk::MenuButton,
    pub restore_btn:   Button,
    pub close_btn:     Button,
    pub title_label:   SwipeText,
    pub artist_label:  SwipeText,
    pub status_label:  Label,
    pub btn_prev:      Button,
    pub btn_play:      Button,
    pub btn_next:      Button,
    pub vol_btn:       Button,
    pub vol_icon_img:  gtk::Image,
    pub vol_label:     gtk::Label,
    pub vol_popover:   gtk::Popover,
    pub mute_btn:      Button,
    pub vol_scale:     Scale,
}

// ── Build functions ───────────────────────────────────────────────────────────

pub(super) fn build_header(init_panel_visible: bool) -> (adw::HeaderBar, gtk::ToggleButton, gtk::ToggleButton) {
    let header = adw::HeaderBar::new();

    let sidebar_btn = gtk::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .active(init_panel_visible)
        .tooltip_text("Toggle presets panel")
        .build();
    sidebar_btn.add_css_class("sidebar-toggle");
    header.pack_start(&sidebar_btn);

    header.pack_end(&super::menu::build_menu_button(true));

    let mini_btn = gtk::ToggleButton::builder()
        .icon_name("view-restore-symbolic")
        .tooltip_text("Mini player")
        .build();
    header.pack_end(&mini_btn);

    (header, sidebar_btn, mini_btn)
}

pub(super) fn build_presets_panel() -> (PresetWidgets, gtk::ScrolledWindow) {
    let presets_box = GtkBox::builder()
        .orientation(Orientation::Vertical).spacing(2)
        .margin_top(8).margin_bottom(4).margin_start(8).margin_end(8)
        .build();
    presets_box.append(
        &Label::builder()
            .label("PRESETS").css_classes(["section-label"])
            .halign(Align::Start).margin_bottom(4)
            .build(),
    );

    let mut preset_btns:   Vec<Button>     = Vec::new();
    let mut preset_pics:   Vec<gtk::Image> = Vec::new();
    let mut preset_labels: Vec<Label>      = Vec::new();

    for i in 1..=12u32 {
        let badge = Label::builder()
            .label(&i.to_string()).css_classes(["preset-badge"])
            .halign(Align::Center).valign(Align::Center)
            .build();
        let pic = gtk::Image::builder()
            .pixel_size(40).icon_name("audio-x-generic-symbolic")
            .build();
        pic.add_css_class("preset-art");
        pic.set_overflow(gtk::Overflow::Hidden);
        let lbl = Label::builder()
            .label("").css_classes(["preset-name"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .halign(Align::Start).hexpand(true).width_chars(0)
            .build();
        let tile = GtkBox::builder()
            .orientation(Orientation::Horizontal).spacing(6)
            .css_classes(["preset-tile"]).overflow(gtk::Overflow::Hidden)
            .build();
        tile.append(&badge);
        tile.append(&pic);
        tile.append(&lbl);
        // "preset-btn" only styled under RustyWiiM Modern (see modern.css),
        // to trim its default flat-button horizontal padding — inert
        // elsewhere, same pattern as "panel-card"/"controls-card".
        let btn = Button::builder().child(&tile).css_classes(["flat", "preset-btn"]).build();
        btn.set_tooltip_text(Some(&format!("Preset {i}")));
        btn.set_visible(false);
        presets_box.append(&btn);
        preset_btns.push(btn);
        preset_pics.push(pic);
        preset_labels.push(lbl);
    }

    let pp = PresetWidgets {
        btns:   Rc::new(preset_btns),
        pics:   Rc::new(preset_pics),
        labels: Rc::new(preset_labels),
    };

    let presets_scroll = gtk::ScrolledWindow::builder()
        .child(&presets_box)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    (pp, presets_scroll)
}

pub(super) fn build_source_widgets(icons: &Rc<icons::IconSet>) -> SourceWidgets {
    let icons = Rc::clone(icons);
    let sw = SourceWidgets {
        dropdown: gtk::DropDown::from_strings(&["—"]),
        ids:      Rc::new(RefCell::new(Vec::new())),
        enabled:  Rc::new(RefCell::new(Vec::new())),
        updating: Rc::new(RefCell::new(false)),
    };
    sw.dropdown.add_css_class("panel-dropdown");
    sw.dropdown.set_sensitive(false);

    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, obj| {
        let Some(item) = obj.downcast_ref::<gtk::ListItem>() else { return };
        let hbox = GtkBox::builder()
            .orientation(Orientation::Horizontal).spacing(6).build();
        hbox.append(&gtk::Image::builder().pixel_size(16).build());
        hbox.append(&Label::builder().halign(Align::Start).build());
        item.set_child(Some(&hbox));
    });
    factory.connect_bind(clone!(
        @strong sw, @strong icons
            => move |_, obj| {
                let Some(item) = obj.downcast_ref::<gtk::ListItem>() else { return };
                let pos  = item.position() as usize;
                if let Some(hbox) = item.child().and_downcast::<GtkBox>() {
                    let enabled = sw.enabled.borrow().get(pos).copied().unwrap_or(true);
                    let ids     = sw.ids.borrow();
                    let id      = ids.get(pos).map(String::as_str).unwrap_or("");
                    if let Some(img) = hbox.first_child().and_downcast::<gtk::Image>() {
                        img.set_paintable(Some(icons.source_paintable(id)));
                    }
                    if let Some(lbl) = hbox.last_child().and_downcast::<Label>() {
                        if let Some(so) = item.item().and_downcast::<gtk::StringObject>() {
                            lbl.set_label(&so.string());
                        }
                        lbl.set_sensitive(enabled);
                    }
                    item.set_activatable(enabled);
                }
            }
    ));
    factory.connect_unbind(|_, obj| {
        let Some(item) = obj.downcast_ref::<gtk::ListItem>() else { return };
        item.set_activatable(true);
        if let Some(hbox) = item.child().and_downcast::<GtkBox>() {
            if let Some(lbl) = hbox.last_child().and_downcast::<Label>() {
                lbl.set_sensitive(true);
            }
        }
    });
    sw.dropdown.set_factory(Some(&factory));
    sw
}

pub(super) fn build_output_widgets(icons: &Rc<icons::IconSet>) -> OutputWidgets {
    let icons = Rc::clone(icons);
    let ow = OutputWidgets {
        dropdown:    gtk::DropDown::from_strings(&["—"]),
        section:     GtkBox::builder()
            .orientation(Orientation::Vertical).spacing(4).visible(false).build(),
        modes:       Rc::new(RefCell::new(Vec::new())),
        canon_names: Rc::new(RefCell::new(Vec::new())),
        updating:    Rc::new(RefCell::new(false)),
    };
    ow.dropdown.add_css_class("panel-dropdown");
    ow.dropdown.set_sensitive(false);

    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, obj| {
        let Some(item) = obj.downcast_ref::<gtk::ListItem>() else { return };
        let hbox = GtkBox::builder()
            .orientation(Orientation::Horizontal).spacing(6).build();
        hbox.append(&gtk::Image::builder().pixel_size(16).build());
        hbox.append(&Label::builder().halign(Align::Start).build());
        item.set_child(Some(&hbox));
    });
    factory.connect_bind(clone!(@strong ow, @strong icons => move |_, obj| {
        let Some(item) = obj.downcast_ref::<gtk::ListItem>() else { return };
        let pos  = item.position() as usize;
        if let Some(hbox) = item.child().and_downcast::<GtkBox>() {
            let names = ow.canon_names.borrow();
            let canon = names.get(pos).copied().unwrap_or("");
            if let Some(img) = hbox.first_child().and_downcast::<gtk::Image>() {
                img.set_paintable(Some(icons.output_paintable(canon)));
            }
            if let Some(lbl) = hbox.last_child().and_downcast::<Label>() {
                if let Some(so) = item.item().and_downcast::<gtk::StringObject>() {
                    lbl.set_label(&so.string());
                }
            }
        }
    }));
    ow.dropdown.set_factory(Some(&factory));

    ow.section.append(
        &Label::builder()
            .label("OUTPUT").css_classes(["section-label"]).halign(Align::Start).build(),
    );
    ow.section.append(&ow.dropdown);

    ow
}

pub(super) fn build_left_pane(sw: &SourceWidgets, ow: &OutputWidgets, presets_scroll: &gtk::ScrolledWindow) -> gtk::Box {
    let io_box = GtkBox::builder()
        .orientation(Orientation::Vertical).spacing(4)
        .margin_top(4).margin_bottom(8).margin_start(8).margin_end(8)
        .build();
    io_box.append(&gtk::Separator::new(Orientation::Horizontal));
    io_box.append(
        &Label::builder()
            .label("INPUT").css_classes(["section-label"])
            .halign(Align::Start).margin_top(6).build(),
    );
    io_box.append(&sw.dropdown);
    io_box.append(&ow.section);

    // "panel-card" is only ever styled under the RustyWiiM Modern theme
    // (see modern.css) — inert everywhere else, so no theme branching here.
    let left_pane = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .css_classes(["panel-card"])
        .build();
    left_pane.append(presets_scroll);
    left_pane.append(&io_box);
    left_pane
}

fn build_vol_popover() -> (Button, gtk::Image, gtk::Label, Scale, Button, gtk::Popover) {
    // vol_btn must exist before we can set it as the popover's parent.
    // Use a custom child so we can show both the icon and the volume number.
    let vol_icon_img = gtk::Image::builder()
        .icon_name("audio-volume-high-symbolic")
        .pixel_size(16)
        .build();
    let vol_label = gtk::Label::builder()
        .label("—")
        .width_chars(3)
        .xalign(1.0)
        .build();
    let btn_box = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(2)
        .build();
    btn_box.append(&vol_icon_img);
    btn_box.append(&vol_label);
    let vol_btn = Button::builder()
        .css_classes(["transport-btn", "flat", "vol-btn"])
        .tooltip_text("Volume")
        .build();
    vol_btn.set_child(Some(&btn_box));

    let vol_scale = Scale::with_range(Orientation::Vertical, 0.0, 100.0, 1.0);
    vol_scale.set_inverted(true);
    vol_scale.set_vexpand(true);
    vol_scale.set_height_request(150);
    vol_scale.set_draw_value(false);
    vol_scale.set_width_request(24);
    vol_scale.set_round_digits(0);
    vol_scale.add_css_class("vol-pop");
    vol_scale.set_increments(5.0, 20.0);

    let mute_btn = Button::builder()
        .icon_name("audio-volume-muted-symbolic")
        .css_classes(["transport-btn", "circular"])
        .tooltip_text("Mute")
        .halign(Align::Center)
        .build();

    let vol_pop_box = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .margin_top(6).margin_bottom(6).margin_start(6).margin_end(6)
        .spacing(4)
        .build();
    vol_pop_box.append(&vol_scale);
    vol_pop_box.append(&mute_btn);
    let vol_popover = gtk::Popover::new();
    vol_popover.set_child(Some(&vol_pop_box));
    vol_popover.set_parent(&vol_btn);

    (vol_btn, vol_icon_img, vol_label, vol_scale, mute_btn, vol_popover)
}

pub(super) fn build_playback_widgets() -> (PlaybackWidgets, Scale) {
    let (vol_btn, vol_icon_img, vol_label, vol_scale, mute_btn, vol_popover) = build_vol_popover();

    let pw = PlaybackWidgets {
        // hexpand+vexpand (default Fill alignment) so the widget always gets
        // the full art area to work with — it does its own aspect-preserving
        // "contain"/fixed-size centering internally (draw_content() in
        // flip_cover.rs), so unlike gtk::Picture it doesn't need a
        // content-derived natural size for halign(Center) to center against.
        // It also renders both real art AND the fallback icon itself now
        // (crossfading between them), so no separate art_stack/input_icon.
        artwork:    { let f = FlipCover::new();
                      f.set_hexpand(true); f.set_vexpand(true); f },
        // drop_shadow starts false regardless of theme — it's only wanted
        // for legibility against Modern's blurred background, and gets
        // toggled live by update_art_background_visibility() in ui/mod.rs
        // (called once more right after window construction, so this
        // initial value only matters for the instant before that runs).
        title:  SwipeText::new("Not connected", "track-title",  true, false),
        artist: SwipeText::new("",              "track-artist", true, false),
        album:  SwipeText::new("",              "track-album",  true, false),
        status:   Label::builder().css_classes(["status-badge"]).halign(Align::Center).build(),
        // Always visible (never `.set_visible(false)`) so its line-height is
        // permanently reserved in the layout — otherwise the artwork above it
        // resizes whenever quality info appears/disappears (e.g. no bitrate
        // data for the current source). Empty text still keeps its line
        // height in Pango's logical extents, same as the other labels here.
        quality:  Label::builder().css_classes(["quality-label"]).halign(Align::Center).build(),
        pos:      Label::builder().label("0:00").css_classes(["dim-label"]).build(),
        dur:      Label::builder().label("0:00").css_classes(["dim-label"]).build(),
        seek:     Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 1.0),
        btn_prev: Button::builder()
            .icon_name("media-skip-backward-symbolic")
            .css_classes(["transport-btn", "circular", "flat"]).build(),
        btn_play: Button::builder()
            .icon_name("media-playback-start-symbolic")
            .css_classes(["play-btn", "circular", "suggested-action"]).build(),
        btn_next: Button::builder()
            .icon_name("media-skip-forward-symbolic")
            .css_classes(["transport-btn", "circular", "flat"]).build(),
        shuffle:  Button::builder()
            .icon_name("media-playlist-shuffle-symbolic")
            .css_classes(["loop-btn", "circular", "flat"]).tooltip_text("Shuffle: Off").build(),
        repeat:   Button::builder()
            .icon_name("media-playlist-repeat-symbolic")
            .css_classes(["loop-btn", "circular", "flat"]).tooltip_text("Repeat: Off").build(),
        vol_btn,
        vol_icon_img,
        vol_label,
        vol_popover,
        mute_btn,
    };

    pw.seek.set_hexpand(true);
    pw.seek.set_draw_value(false);
    pw.seek.add_css_class("seek-scale");
    pw.seek.set_round_digits(0);

    (pw, vol_scale)
}

pub(super) fn build_right_pane(pw: &PlaybackWidgets) -> gtk::Box {
    // Transport buttons are centred.
    let transport = GtkBox::builder()
        .orientation(Orientation::Horizontal).spacing(12).halign(Align::Center).build();
    transport.prepend(&pw.shuffle);
    transport.append(&pw.btn_prev);
    transport.append(&pw.btn_play);
    transport.append(&pw.btn_next);
    transport.append(&pw.repeat);

    // Vol button sits at the right edge of the seek row, aligned with the bar's right end.
    let seek_row = GtkBox::builder().orientation(Orientation::Horizontal).spacing(8).build();
    seek_row.append(&pw.pos);
    seek_row.append(&pw.seek);
    seek_row.append(&pw.dur);
    pw.vol_btn.set_margin_start(4);
    seek_row.append(&pw.vol_btn);

    // Overlay adds a radial vignette frame over the artwork that fades into the panel background.
    let art_overlay = gtk::Overlay::new();
    art_overlay.set_vexpand(true);
    art_overlay.set_child(Some(&pw.artwork));
    let art_frame = GtkBox::builder()
        .hexpand(true).vexpand(true)
        .css_classes(["art-frame"])
        .can_target(false)
        .build();
    art_overlay.add_overlay(&art_frame);

    // Seek row + transport grouped into one card under RustyWiiM Modern
    // (see modern.css); inert everywhere else, same as "panel-card" above.
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
    right_pane.append(&pw.title.stack);
    right_pane.append(&pw.artist.stack);
    right_pane.append(&pw.album.stack);
    right_pane.append(&pw.status);
    right_pane.append(&pw.quality);
    right_pane.append(&controls_card);

    right_pane
}

fn build_mini_flip_cover() -> FlipCover {
    let f = FlipCover::new();
    f.set_hexpand(false);
    f.set_vexpand(false);
    f.set_valign(Align::Center);
    f.add_css_class("mini-art");
    // Defensive clip to the widget's own box (e.g. in case the 3D flip's
    // perspective transform renders very slightly outside its bounds at
    // extreme angles) — no rounded corners here, so nothing to clip normally.
    f.set_overflow(gtk::Overflow::Hidden);
    f
}

fn build_mini_top_bar() -> (Label, gtk::MenuButton, Button, Button, GtkBox) {
    let mini_device_label = Label::builder()
        .label("").css_classes(["mini-device-label"])
        .halign(Align::Start).hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let mini_restore_btn = Button::builder()
        .icon_name("view-fullscreen-symbolic")
        .css_classes(["mini-restore-btn"])
        .tooltip_text("Restore to full window")
        .build();
    let mini_menu_btn = super::menu::build_menu_button(true);
    mini_menu_btn.add_css_class("mini-restore-btn");
    mini_menu_btn.add_css_class("flat");
    let mini_close_btn = Button::builder()
        .icon_name("window-close-symbolic")
        .css_classes(["mini-restore-btn"])
        .tooltip_text("Close")
        .build();
    let mini_top_bar = GtkBox::builder()
        .orientation(Orientation::Horizontal).spacing(4)
        .margin_start(14).margin_end(12).margin_top(10).margin_bottom(4)
        .css_classes(["mini-top-bar"])
        .build();
    mini_top_bar.append(&mini_device_label);
    mini_top_bar.append(&mini_restore_btn);
    mini_top_bar.append(&mini_menu_btn);
    mini_top_bar.append(&mini_close_btn);
    (mini_device_label, mini_menu_btn, mini_restore_btn, mini_close_btn, mini_top_bar)
}

fn build_mini_transport() -> (Label, Button, Button, Button, Button, gtk::Image, gtk::Label, Scale, Button, gtk::Popover, GtkBox) {
    let mini_status_label = Label::builder()
        .label("").css_classes(["mini-status-label"])
        .halign(Align::Start).hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();

    let mini_btn_prev = Button::builder()
        .icon_name("media-skip-backward-symbolic")
        .css_classes(["mini-transport-btn", "flat"]).build();
    let mini_btn_play = Button::builder()
        .icon_name("media-playback-start-symbolic")
        .css_classes(["mini-play-btn", "suggested-action"]).build();
    let mini_btn_next = Button::builder()
        .icon_name("media-skip-forward-symbolic")
        .css_classes(["mini-transport-btn", "flat"]).build();

    // Volume button with popover — must be created before mini_transport append.
    let mini_vol_icon_img = gtk::Image::builder()
        .icon_name("audio-volume-high-symbolic")
        .pixel_size(11)
        .build();
    let mini_vol_label = gtk::Label::builder()
        .label("—")
        .width_chars(3)
        .xalign(1.0)
        .css_classes(["mini-vol-label"])
        .build();
    let mini_btn_box = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(1)
        .build();
    mini_btn_box.append(&mini_vol_icon_img);
    mini_btn_box.append(&mini_vol_label);
    let mini_vol_btn = Button::builder()
        .css_classes(["mini-transport-btn", "mini-vol-btn", "flat"])
        .tooltip_text("Volume")
        .build();
    mini_vol_btn.set_child(Some(&mini_btn_box));
    let mini_vol_scale = Scale::with_range(Orientation::Vertical, 0.0, 100.0, 1.0);
    mini_vol_scale.set_inverted(true);
    mini_vol_scale.set_vexpand(true);
    mini_vol_scale.set_height_request(120);
    mini_vol_scale.set_draw_value(false);
    mini_vol_scale.set_width_request(20);
    mini_vol_scale.set_round_digits(0);
    mini_vol_scale.add_css_class("mini-vol-pop");
    mini_vol_scale.set_increments(5.0, 20.0);
    let mini_mute_btn = Button::builder()
        .icon_name("audio-volume-muted-symbolic")
        .css_classes(["mini-transport-btn"])
        .tooltip_text("Mute")
        .halign(Align::Center)
        .build();
    let mini_vol_pop_box = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .margin_top(4).margin_bottom(4).margin_start(4).margin_end(4)
        .spacing(4)
        .build();
    mini_vol_pop_box.append(&mini_vol_scale);
    mini_vol_pop_box.append(&mini_mute_btn);
    let mini_vol_popover = gtk::Popover::new();
    mini_vol_popover.add_css_class("mini-vol-popover");
    mini_vol_popover.set_child(Some(&mini_vol_pop_box));
    mini_vol_popover.set_parent(&mini_vol_btn);

    let mini_transport_center = GtkBox::builder()
        .orientation(Orientation::Horizontal).spacing(2).build();
    mini_transport_center.append(&mini_btn_prev);
    mini_transport_center.append(&mini_btn_play);
    mini_transport_center.append(&mini_btn_next);

    mini_vol_btn.set_margin_end(0);
    let mini_vol_end = GtkBox::builder()
        .valign(Align::Center).build();
    mini_vol_end.append(&mini_vol_btn);

    // Card wraps only the actual playback controls (prev/play/next +
    // volume) — mini_status_label sits outside it. mini_status_label's own
    // hexpand(true) already pushes this group to the row's trailing edge
    // (the only hexpand child in mini_transport, so it absorbs all the
    // leftover width), so mini_vol_end no longer needs its own
    // hexpand/halign(End) to get there.
    let mini_controls_card = GtkBox::builder()
        .orientation(Orientation::Horizontal).spacing(6)
        .css_classes(["mini-transport-card"])
        .build();
    mini_controls_card.append(&mini_transport_center);
    mini_controls_card.append(&mini_vol_end);

    let mini_transport = GtkBox::builder()
        .orientation(Orientation::Horizontal).hexpand(true)
        .build();
    mini_transport.append(&mini_status_label);
    mini_transport.append(&mini_controls_card);

    (mini_status_label, mini_btn_prev, mini_btn_play, mini_btn_next,
     mini_vol_btn, mini_vol_icon_img, mini_vol_label,
     mini_vol_scale, mini_mute_btn, mini_vol_popover, mini_transport)
}

/// A thin, invisible, full-height strip along `edge` that starts an
/// interactive horizontal resize when pressed — see the comment at its call
/// site in `build_mini_window()` for why an undecorated window needs this at
/// all. `align` is the strip's own edge (Start for West, End for East).
fn build_mini_resize_edge(edge: gtk::gdk::SurfaceEdge, align: Align) -> GtkBox {
    let handle = GtkBox::builder()
        .width_request(6)
        .hexpand(false).vexpand(true)
        .halign(align)
        .build();
    handle.set_cursor_from_name(Some("ew-resize"));

    let gesture = gtk::GestureClick::new();
    gesture.set_button(1); // primary button only
    gesture.connect_pressed(glib::clone!(@weak handle => move |gesture, _n_press, x, y| {
        let Some(surface) = handle.native().and_then(|n| n.surface()) else { return };
        let Some(toplevel) = surface.downcast::<gtk::gdk::Toplevel>().ok() else { return };
        let device = gesture.current_event_device();
        let button = gesture.current_button() as i32;
        let time   = gesture.current_event_time();
        toplevel.begin_resize(edge, device.as_ref(), button, x, y, time);
    }));
    handle.add_controller(gesture);

    handle
}

pub(super) fn build_mini_window(app: &adw::Application) -> (MiniWidgets, gtk::ApplicationWindow) {
    let mini_artwork = build_mini_flip_cover();
    let (mini_device_label, mini_menu_btn, mini_restore_btn, mini_close_btn, mini_top_bar) = build_mini_top_bar();
    let (mini_status_label, mini_btn_prev, mini_btn_play, mini_btn_next,
         mini_vol_btn, mini_vol_icon_img, mini_vol_label,
         mini_vol_scale, mini_mute_btn, mini_vol_popover, mini_transport) = build_mini_transport();

    let mini_title_label  = SwipeText::new("—", "mini-title",  false, false);
    let mini_artist_label = SwipeText::new("",  "mini-artist", false, false);

    let mini_info_box = GtkBox::builder()
        .orientation(Orientation::Vertical).spacing(4)
        .valign(Align::Center).hexpand(true)
        .build();
    mini_info_box.append(&mini_title_label.stack);
    mini_info_box.append(&mini_artist_label.stack);
    mini_info_box.append(&mini_transport);

    let mini_main_row = GtkBox::builder()
        .orientation(Orientation::Horizontal).spacing(12)
        .margin_start(14).margin_end(14).margin_bottom(14)
        .build();
    // Explicit background fills the vertical centering gap that appears above
    // mini_info_box (valign=Center, shorter than the art stack).  Without it
    // the NGL renderer can leave stale GPU buffer pixels there.
    mini_main_row.add_css_class("mini-main-row");
    mini_main_row.append(&mini_artwork);
    mini_main_row.append(&mini_info_box);

    let mini_content = GtkBox::builder()
        .orientation(Orientation::Vertical).spacing(0)
        .build();
    mini_content.append(&mini_top_bar);
    mini_content.append(&mini_main_row);

    // ArtBackground sits *inside* mini-outer (not wrapping the whole
    // window) so mini-outer's own overflow(Hidden) + border-radius clips
    // both the background layer and the foreground content to the same
    // rounded shape — wrapping the whole window instead would let the
    // (rectangular) blur peek out past the rounded corners, where the
    // window itself is otherwise fully transparent to the real desktop.
    let mini_art_bg = art_background::ArtBackground::new();
    mini_art_bg.set_hexpand(true);
    mini_art_bg.set_vexpand(true);
    mini_art_bg.set_visible(false); // gated live — see update_art_background_visibility()

    let mini_outer = gtk::Overlay::new();
    mini_outer.set_child(Some(&mini_art_bg));
    mini_outer.add_overlay(&mini_content);
    // ArtBackground (the main/measured child) reports no intrinsic size — it's
    // meant to be sized by whatever allocates it — so without this the Overlay
    // sizes itself off a 0×0 child instead of mini_content, and the window's
    // actual height (there is no explicit default_height, only default_width)
    // ends up wrong. mini_content is the widget that should drive sizing here.
    mini_outer.set_measure_overlay(&mini_content, true);
    mini_outer.add_css_class("mini-outer");
    mini_outer.set_overflow(gtk::Overflow::Hidden);

    // Resizable(true) alone (below) only permits the window manager/
    // compositor to resize the surface — it doesn't give an undecorated
    // window any UI to *trigger* that, since decorated(false) means there's
    // no server-side titlebar/border providing the usual edge hit-testing.
    // These are thin invisible strips along the left/right edges, added as
    // the topmost overlay children so they receive the press before
    // mini_content underneath, that manually kick off an interactive resize
    // via the surface's own begin_resize() — the same mechanism GTK's own
    // CSD border-drag logic uses internally.
    mini_outer.add_overlay(&build_mini_resize_edge(gtk::gdk::SurfaceEdge::West, Align::Start));
    mini_outer.add_overlay(&build_mini_resize_edge(gtk::gdk::SurfaceEdge::East, Align::End));

    let mini_root = gtk::WindowHandle::new();
    mini_root.set_child(Some(&mini_outer));

    let mini_win = gtk::ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .resizable(true)
        .default_width(380)
        .title("RustyWiiM")
        .child(&mini_root)
        .build();
    mini_win.add_css_class("mini-window");

    let mini = MiniWidgets {
        root:          mini_root,
        art_bg:        mini_art_bg,
        artwork:       mini_artwork,
        device_label:  mini_device_label,
        menu_btn:      mini_menu_btn,
        restore_btn:   mini_restore_btn,
        close_btn:     mini_close_btn,
        title_label:   mini_title_label,
        artist_label:  mini_artist_label,
        status_label:  mini_status_label,
        btn_prev:      mini_btn_prev,
        btn_play:      mini_btn_play,
        btn_next:      mini_btn_next,
        vol_btn:       mini_vol_btn,
        vol_icon_img:  mini_vol_icon_img,
        vol_label:     mini_vol_label,
        vol_popover:   mini_vol_popover,
        mute_btn:      mini_mute_btn,
        vol_scale:     mini_vol_scale,
    };

    (mini, mini_win)
}
