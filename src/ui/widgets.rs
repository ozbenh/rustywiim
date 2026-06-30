#![allow(deprecated)] // glib clone! old-style @strong syntax

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;
use gtk::{Align, Box as GtkBox, Button, Label, Orientation, Scale};

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

#[derive(Clone)]
pub(crate) struct PlaybackWidgets {
    pub title:      ScrollFadeLabel,
    pub artist:     ScrollFadeLabel,
    pub album:      ScrollFadeLabel,
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
    pub vol_btn:     Button,
    pub vol_popover: gtk::Popover,
    pub mute_btn:    Button,
    pub artwork:     gtk::Picture,
    pub art_stack:   gtk::Stack,
    pub input_icon:  gtk::Image,
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
    pub art_stack:     gtk::Stack,
    pub artwork:       gtk::Image,
    pub input_icon:    gtk::Image,
    pub device_label:  Label,
    #[allow(dead_code)] // owned for lifetime; the widget is parented to the top bar
    pub menu_btn:      gtk::MenuButton,
    pub restore_btn:   Button,
    pub close_btn:     Button,
    pub title_label:   ScrollFadeLabel,
    pub artist_label:  ScrollFadeLabel,
    pub status_label:  Label,
    pub btn_prev:      Button,
    pub btn_play:      Button,
    pub btn_next:      Button,
    pub vol_btn:       Button,
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
        let btn = Button::builder().child(&tile).css_classes(["flat"]).build();
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
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let hbox = GtkBox::builder()
            .orientation(Orientation::Horizontal).spacing(6).build();
        hbox.append(&gtk::Image::builder().pixel_size(16).build());
        hbox.append(&Label::builder().halign(Align::Start).build());
        item.set_child(Some(&hbox));
    });
    factory.connect_bind(clone!(
        @strong sw, @strong icons
            => move |_, obj| {
                let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
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
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
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
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let hbox = GtkBox::builder()
            .orientation(Orientation::Horizontal).spacing(6).build();
        hbox.append(&gtk::Image::builder().pixel_size(16).build());
        hbox.append(&Label::builder().halign(Align::Start).build());
        item.set_child(Some(&hbox));
    });
    factory.connect_bind(clone!(@strong ow, @strong icons => move |_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
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

    let left_pane = GtkBox::builder().orientation(Orientation::Vertical).build();
    left_pane.append(presets_scroll);
    left_pane.append(&io_box);
    left_pane
}

fn build_vol_popover() -> (Button, Scale, Button, gtk::Popover) {
    // vol_btn must exist before we can set it as the popover's parent.
    let vol_btn = Button::builder()
        .icon_name("audio-volume-high-symbolic")
        .css_classes(["transport-btn", "circular", "flat", "vol-btn"])
        .tooltip_text("Volume")
        .build();

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

    (vol_btn, vol_scale, mute_btn, vol_popover)
}

pub(super) fn build_playback_widgets() -> (PlaybackWidgets, Scale) {
    let (vol_btn, vol_scale, mute_btn, vol_popover) = build_vol_popover();

    let pw = PlaybackWidgets {
        artwork:    gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain).can_shrink(true)
            .halign(Align::Center).vexpand(true).build(),
        input_icon: gtk::Image::builder()
            .pixel_size(128).halign(Align::Center).valign(Align::Center).build(),
        art_stack: {
            let s = gtk::Stack::new();
            s.set_vexpand(true);
            s.set_transition_type(gtk::StackTransitionType::Crossfade);
            s.set_transition_duration(200);
            s
        },
        title:  { let l = ScrollFadeLabel::new("Not connected");
                   l.add_label_css_class("track-title"); l.set_hexpand(true); l },
        artist: { let l = ScrollFadeLabel::new("");
                   l.add_label_css_class("track-artist"); l.set_hexpand(true); l },
        album:  { let l = ScrollFadeLabel::new("");
                   l.add_label_css_class("track-album");  l.set_hexpand(true); l },
        status:   Label::builder().css_classes(["status-badge"]).halign(Align::Center).build(),
        quality:  Label::builder().css_classes(["quality-label"]).halign(Align::Center)
            .visible(false).build(),
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
        vol_popover,
        mute_btn,
    };

    pw.art_stack.add_named(&pw.artwork, Some("artwork"));
    pw.art_stack.add_named(&pw.input_icon, Some("icon"));
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
    art_overlay.set_child(Some(&pw.art_stack));
    let art_frame = GtkBox::builder()
        .hexpand(true).vexpand(true)
        .css_classes(["art-frame"])
        .can_target(false)
        .build();
    art_overlay.add_overlay(&art_frame);

    let right_pane = GtkBox::builder()
        .orientation(Orientation::Vertical).spacing(8).hexpand(true)
        .margin_top(8).margin_bottom(8).margin_start(12).margin_end(16)
        .build();
    right_pane.append(&art_overlay);
    right_pane.append(&pw.title);
    right_pane.append(&pw.artist);
    right_pane.append(&pw.album);
    right_pane.append(&pw.status);
    right_pane.append(&pw.quality);
    right_pane.append(&seek_row);
    right_pane.append(&transport);

    right_pane
}

fn build_mini_art_stack() -> (gtk::Image, gtk::Image, gtk::Stack) {
    let mini_artwork = gtk::Image::builder()
        .pixel_size(64)
        .halign(Align::Fill).valign(Align::Fill)
        .build();
    let mini_input_icon = gtk::Image::builder()
        .pixel_size(36).halign(Align::Center).valign(Align::Center)
        .build();
    let mini_art_stack = {
        let s = gtk::Stack::new();
        s.set_hexpand(false);
        s.set_vexpand(false);
        s.set_valign(Align::Center);
        s.add_css_class("mini-art");
        s.set_overflow(gtk::Overflow::Hidden); // clips border-radius corners
        s.set_transition_type(gtk::StackTransitionType::Crossfade);
        s.set_transition_duration(200);
        s
    };
    mini_art_stack.add_named(&mini_artwork, Some("artwork"));
    mini_art_stack.add_named(&mini_input_icon, Some("icon"));
    (mini_artwork, mini_input_icon, mini_art_stack)
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
        .build();
    mini_top_bar.append(&mini_device_label);
    mini_top_bar.append(&mini_restore_btn);
    mini_top_bar.append(&mini_menu_btn);
    mini_top_bar.append(&mini_close_btn);
    (mini_device_label, mini_menu_btn, mini_restore_btn, mini_close_btn, mini_top_bar)
}

fn build_mini_transport() -> (Label, Button, Button, Button, Button, Scale, Button, gtk::Popover, GtkBox) {
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
    let mini_vol_btn = Button::builder()
        .icon_name("audio-volume-high-symbolic")
        .css_classes(["mini-transport-btn", "mini-vol-btn", "flat"])
        .tooltip_text("Volume")
        .build();
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
        .orientation(Orientation::Horizontal).spacing(6).build();
    mini_transport_center.append(&mini_btn_prev);
    mini_transport_center.append(&mini_btn_play);
    mini_transport_center.append(&mini_btn_next);

    mini_vol_btn.set_margin_end(6);
    let mini_vol_end = GtkBox::builder()
        .hexpand(true).halign(Align::End).valign(Align::Center).build();
    mini_vol_end.append(&mini_vol_btn);

    let mini_transport = GtkBox::builder()
        .orientation(Orientation::Horizontal).hexpand(true).build();
    mini_transport.append(&mini_status_label);
    mini_transport.append(&mini_transport_center);
    mini_transport.append(&mini_vol_end);

    (mini_status_label, mini_btn_prev, mini_btn_play, mini_btn_next,
     mini_vol_btn, mini_vol_scale, mini_mute_btn, mini_vol_popover, mini_transport)
}

pub(super) fn build_mini_window(app: &adw::Application) -> (MiniWidgets, gtk::ApplicationWindow) {
    let (mini_artwork, mini_input_icon, mini_art_stack) = build_mini_art_stack();
    let (mini_device_label, mini_menu_btn, mini_restore_btn, mini_close_btn, mini_top_bar) = build_mini_top_bar();
    let (mini_status_label, mini_btn_prev, mini_btn_play, mini_btn_next,
         mini_vol_btn, mini_vol_scale, mini_mute_btn, mini_vol_popover, mini_transport) = build_mini_transport();

    let mini_title_label = { let l = ScrollFadeLabel::new("—");
                              l.add_label_css_class("mini-title"); l.set_hexpand(true);
                              l.set_center_when_fits(false); l };
    let mini_artist_label = { let l = ScrollFadeLabel::new("");
                               l.add_label_css_class("mini-artist"); l.set_hexpand(true);
                               l.set_center_when_fits(false); l };

    let mini_info_box = GtkBox::builder()
        .orientation(Orientation::Vertical).spacing(4)
        .valign(Align::Center).hexpand(true)
        .build();
    mini_info_box.append(&mini_title_label);
    mini_info_box.append(&mini_artist_label);
    mini_info_box.append(&mini_transport);

    let mini_main_row = GtkBox::builder()
        .orientation(Orientation::Horizontal).spacing(12)
        .margin_start(14).margin_end(14).margin_bottom(14)
        .build();
    // Explicit background fills the vertical centering gap that appears above
    // mini_info_box (valign=Center, shorter than the art stack).  Without it
    // the NGL renderer can leave stale GPU buffer pixels there.
    mini_main_row.add_css_class("mini-main-row");
    mini_main_row.append(&mini_art_stack);
    mini_main_row.append(&mini_info_box);

    let mini_outer = GtkBox::builder()
        .orientation(Orientation::Vertical).spacing(0)
        .build();
    mini_outer.append(&mini_top_bar);
    mini_outer.append(&mini_main_row);

    let mini_root = gtk::WindowHandle::new();
    mini_root.set_child(Some(&mini_outer));

    let mini_win = gtk::ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .resizable(false)
        .default_width(360)
        .title("RustyWiiM")
        .child(&mini_root)
        .build();
    mini_win.add_css_class("mini-window");

    let mini = MiniWidgets {
        root:          mini_root,
        art_stack:     mini_art_stack,
        artwork:       mini_artwork,
        input_icon:    mini_input_icon,
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
        vol_popover:   mini_vol_popover,
        mute_btn:      mini_mute_btn,
        vol_scale:     mini_vol_scale,
    };

    (mini, mini_win)
}
