#![allow(deprecated)] // glib clone! @strong syntax

/// Device-list window — renders `device::discovery_manager::DiscoveryManager`'s
/// tracked-device list and lets the user pin/unpin entries, open device windows
/// by double-clicking, and add devices manually by IP. Owns no tracking state
/// of its own; see that module's doc comment for the full backend story
/// (SSDP consumption, presence, config seed-in/report-out).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;
use gtk::{glib, Orientation};

use crate::config;
use crate::device::discovery::DiscoveredDevice;
use crate::device::discovery_manager::{DevicePresence, DiscoveryManager, ManagedEntry, device_key};
use crate::device::state::DeviceState;
use crate::ui::icons::IconSet;
use super::flip_cover::FlipCover;
use super::views::common::vol_icon;
use super::scroll_fade_label::ScrollFadeLabel;

/// "Title · Artist" (round-dot separator, same as the mini window's
/// artist/album line) when now-playing content is available; falls back
/// to the model name otherwise. Shared by `build_row()` (initial render)
/// and the `song-info-changed` handler (in-place update).
fn subtitle_text_for(entry: &ManagedEntry) -> String {
    match &entry.now_playing {
        Some(np) if !np.title.is_empty() && !np.artist.is_empty() => format!("{} \u{00b7} {}", np.title, np.artist),
        Some(np) if !np.title.is_empty()  => np.title.clone(),
        Some(np) if !np.artist.is_empty() => np.artist.clone(),
        _ => entry.model.clone(),
    }
}

/// Applies `entry.now_playing` to an existing `FlipCover` — real art when
/// available, the input/mode icon otherwise, cleared when there's no
/// now-playing content at all. Shared by `build_row()` (initial render,
/// where it never flips — a freshly constructed `FlipCover` has no
/// "previous real art" to flip from) and the `song-info-changed` handler
/// (in-place update on a persistent widget, where a real track-to-track
/// change *does* flip — see `flip_cover.rs`'s own `set_content()`).
fn apply_now_playing(flip: &FlipCover, icons: &IconSet, entry: &ManagedEntry) {
    match &entry.now_playing {
        Some(np) => {
            let tex = np.artwork.as_ref().and_then(|bytes| {
                let gbytes = glib::Bytes::from(bytes.as_ref().as_slice());
                gtk::gdk::Texture::from_bytes(&gbytes).ok()
            });
            // Keys must vary with the actual content (matching
            // `ui/playback.rs`'s `update_artwork()`), never a constant
            // per-device value like `entry.uuid` — `FlipCover` treats a
            // repeated key as "nothing changed" and no-ops, which on this
            // persistent-per-row widget would silently drop every update
            // after the first.
            match &tex {
                Some(t) => flip.set_art(Some(t), np.art_url.as_deref().unwrap_or("")),
                None    => flip.set_icon(icons.source_paintable(&np.icon_key), 32.0, &format!("icon:{}", np.icon_key)),
            }
        }
        None => flip.clear(),
    }
}

/// Volume button + popover slider + mute button, same shape as the mini
/// window's own (`widgets.rs`'s `build_mini_flip_cover()`'s sibling —
/// see `.devlist-vol-*` CSS) sized for a compact row rather than a
/// standalone window. Caller wires the actual click/drag/mute handlers.
fn build_devlist_vol_popover() -> (gtk::Button, gtk::Image, gtk::Label, gtk::Scale, gtk::Button, gtk::Popover) {
    let vol_icon_img = gtk::Image::builder()
        .icon_name("audio-volume-high-symbolic")
        .pixel_size(13) // ~20% bigger than the mini-window-derived 11px original
        .build();
    let vol_label = gtk::Label::builder()
        .label("—")
        .width_chars(3)
        .xalign(1.0)
        .css_classes(["devlist-vol-label"])
        .build();
    let btn_box = gtk::Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(1)
        .build();
    btn_box.append(&vol_icon_img);
    btn_box.append(&vol_label);
    let vol_btn = gtk::Button::builder()
        .css_classes(["devlist-vol-btn", "flat"])
        .tooltip_text("Volume")
        .valign(gtk::Align::Center)
        .build();
    vol_btn.set_child(Some(&btn_box));

    let vol_scale = gtk::Scale::with_range(Orientation::Vertical, 0.0, 100.0, 1.0);
    vol_scale.set_inverted(true);
    vol_scale.set_vexpand(true);
    vol_scale.set_height_request(120);
    vol_scale.set_draw_value(false);
    vol_scale.set_width_request(20);
    vol_scale.set_round_digits(0);
    vol_scale.add_css_class("devlist-vol-pop");
    vol_scale.set_increments(5.0, 20.0);

    let mute_btn = gtk::Button::builder()
        .icon_name("audio-volume-muted-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Mute")
        .halign(gtk::Align::Center)
        .build();

    let vol_pop_box = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .margin_top(4).margin_bottom(4).margin_start(4).margin_end(4)
        .spacing(4)
        .build();
    vol_pop_box.append(&vol_scale);
    vol_pop_box.append(&mute_btn);
    let vol_popover = gtk::Popover::new();
    vol_popover.add_css_class("devlist-vol-popover");
    vol_popover.set_child(Some(&vol_pop_box));
    vol_popover.set_parent(&vol_btn);

    (vol_btn, vol_icon_img, vol_label, vol_scale, mute_btn, vol_popover)
}

/// Syncs a row's volume icon/label/slider from `ds`'s live state — called
/// both at row construction and from the `song-info-changed` handler.
/// Skips repositioning the slider while `drag_timer` is set (same
/// protection the main/mini windows use), so a poll update arriving
/// mid-drag doesn't fight the user's own gesture.
fn sync_devlist_vol_display(rw: &RowWidgets, ds: &DeviceState) {
    let vol = ds.get_vol() as f64;
    let muted = ds.muted();
    if rw.vol_drag_timer.borrow().is_none() {
        rw.vol_scale.set_value(vol);
    }
    rw.vol_icon_img.set_icon_name(Some(vol_icon(muted, vol)));
    rw.vol_label.set_label(&format!("{}", vol as u32));
    rw.mute_btn.set_icon_name(if muted { "audio-volume-muted-symbolic" } else { "audio-volume-high-symbolic" });
}

// ── DiscoveryWindow ───────────────────────────────────────────────────────────

/// The subset of a row's widgets `song-info-changed` needs to update in
/// place — keyed by `device_key()`'s result in a map alongside
/// `current_entries`, rebuilt (not incrementally patched) whenever
/// `rebuild_list()` runs. Only present for rows built while
/// `entry.song_info_enabled` was true (see `build_row()`).
struct RowWidgets {
    flip:         FlipCover,
    subtitle:     ScrollFadeLabel,
    vol_icon_img: gtk::Image,
    vol_label:    gtk::Label,
    vol_scale:    gtk::Scale,
    mute_btn:     gtk::Button,
    /// `gtk::Popover::set_parent()` (not box-packing) attaches this to
    /// `vol_btn` — GTK4 doesn't unparent a `set_parent()`-attached child
    /// automatically when the parent widget itself is torn down, so
    /// without an explicit `unparent()` call first, destroying old rows
    /// on every `rebuild_list()` logs "Finalizing GtkButton but still has
    /// children left: GtkPopover" (confirmed live, first SSDP response —
    /// exactly the first time old rows get torn down). Kept here so
    /// `rebuild_list()` can unparent every outgoing row's popover before
    /// removing it from the list. `widgets.rs`'s main/mini window
    /// `vol_popover`s have the identical latent gap — just never exercised
    /// repeatedly there (built once, torn down once at window close).
    vol_popover: gtk::Popover,
    /// Same drag-protection pattern as the main/mini windows'
    /// `DeviceWindowInner::ui_state.drag_timer` — while set, a live poll
    /// update (`song-info-changed`) skips repositioning the slider so it
    /// doesn't fight an in-progress drag.
    vol_drag_timer: Rc<RefCell<Option<glib::SourceId>>>,
}

pub struct DiscoveryWindow {
    window: adw::ApplicationWindow,
}

impl DiscoveryWindow {
    pub fn new(
        app:           &adw::Application,
        manager:       &DiscoveryManager,
        open_device:   Rc<dyn Fn(&ManagedEntry)>,
        open_settings: Rc<dyn Fn(Option<DeviceState>)>,
    ) -> Self {
        let (init_w, init_h) = config::with(|cfg| (
            if cfg.discovery_window_width  > 0 { cfg.discovery_window_width  } else { 500 },
            if cfg.discovery_window_height > 0 { cfg.discovery_window_height } else { 440 },
        ));

        // One IconSet for the whole window, shared by every row — same
        // pattern as a device window's own `icons` field, not rebuilt per
        // row/rebuild.
        let icons = Rc::new(IconSet::load());
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("RustyWiiM")
            .default_width(init_w)
            .default_height(init_h)
            .build();

        let header = adw::HeaderBar::new();

        // Custom title widget so the spinner can sit immediately to the right
        // of the "Scanning…" subtitle text.  adw::WindowTitle has no widget
        // slot in its subtitle row so we build the layout by hand.
        let title_label = gtk::Label::builder()
            .label("RustyWiiM")
            .css_classes(["title"])
            .build();

        let subtitle_label = gtk::Label::builder()
            .label("Scanning\u{2026}")
            .css_classes(["subtitle"])
            .build();

        let spinner = gtk::Spinner::builder()
            .spinning(true)
            .build();
        spinner.set_size_request(12, 12);

        let subtitle_row = gtk::Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(4)
            .halign(gtk::Align::Center)
            .build();
        subtitle_row.append(&subtitle_label);
        subtitle_row.append(&spinner);

        let title_box = gtk::Box::builder()
            .orientation(Orientation::Vertical)
            .valign(gtk::Align::Center)
            .build();
        title_box.append(&title_label);
        title_box.append(&subtitle_row);

        header.set_title_widget(Some(&title_box));

        let add_btn = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add device by IP address…")
            .build();
        header.pack_end(&add_btn);
        header.pack_end(&super::menu::build_menu_button(false));
        super::wire_window_actions(&window, None, open_settings);

        // Device list
        let list_box = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_top(12).margin_bottom(12)
            .margin_start(12).margin_end(12)
            .build();

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        scroll.set_child(Some(&list_box));

        let content = gtk::Box::builder()
            .orientation(Orientation::Vertical)
            .build();
        content.append(&scroll);

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header);
        toolbar_view.set_content(Some(&content));
        window.set_content(Some(&toolbar_view));

        // The entries backing the currently-rendered rows, kept in the same
        // order they were appended in — `list_box.connect_row_activated()`
        // below looks a clicked/Enter-activated row up by index into this
        // rather than each row closing over its own entry, since GtkListBox
        // already fires `row-activated` for any child row type (not just
        // AdwActionRow) on click or keyboard activation, and reusing that
        // is simpler than reimplementing it per row.
        let current_entries: Rc<RefCell<Vec<ManagedEntry>>> = Rc::new(RefCell::new(Vec::new()));
        // Live-updatable widgets for rows currently showing song info —
        // see `song-info-changed`'s doc comment (`signals()`) for why
        // this exists instead of just rebuilding the list.
        let row_widgets: Rc<RefCell<HashMap<String, RowWidgets>>> = Rc::new(RefCell::new(HashMap::new()));

        // Populate list and subscribe to manager changes.
        Self::rebuild_list(&list_box, &manager.entries(), &current_entries, &row_widgets, manager, &icons);

        // List rebuild: structural changes only (device added/removed/
        // renamed/pinned/moved, presence flips) — see `signals()`.
        manager.connect_list_changed(clone!(
            @strong list_box, @strong current_entries, @strong row_widgets, @strong icons
                => move |mgr| {
                    Self::rebuild_list(&list_box, &mgr.entries(), &current_entries, &row_widgets, mgr, &icons);
                }
        ));

        // One device's now-playing content changed — update just its row
        // in place instead of rebuilding the whole list.
        // Only touches the widget(s) whose own bit actually fired — in
        // particular, never re-evaluates artwork on a bare title/artist
        // tick (matching `ui/playback.rs`'s `update_playback_ui()`, which
        // only calls `update_artwork()` when `mask & ARTWORK != 0`).
        // Title/artist and artwork don't necessarily land on the same poll
        // (artwork is fetched asynchronously after the metadata that
        // announces a track change), so applying every update unconditionally
        // here would catch that gap — artwork transiently absent — and flash
        // the fallback icon before the real flip instead of holding the old
        // art until the new art is actually ready to flip to.
        manager.connect_song_info_changed(clone!(@strong row_widgets, @strong icons => move |mgr, key, mask| {
            use crate::device::state::playback_changed as PC;
            let Some(entry) = mgr.entry_for(key) else { return };
            let widgets = row_widgets.borrow();
            let Some(rw) = widgets.get(key) else { return };
            if mask & (PC::TITLE | PC::ARTIST) != 0 {
                rw.subtitle.set_text(&subtitle_text_for(&entry));
            }
            if mask & PC::ARTWORK != 0 {
                apply_now_playing(&rw.flip, &icons, &entry);
            }
            if mask & PC::VOLUME != 0 {
                if let Some(ds) = mgr.device_state_for(key) {
                    sync_devlist_vol_display(rw, &ds);
                }
            }
        }));

        list_box.connect_row_activated(clone!(@strong current_entries, @strong open_device => move |_, row| {
            let idx = row.index();
            if idx < 0 { return; }
            if let Some(entry) = current_entries.borrow().get(idx as usize) {
                open_device(entry);
            }
        }));

        // Scanning indicator: clear only when the SSDP scan cycle reports in.
        let scanning = Rc::new(std::cell::Cell::new(true));
        manager.connect_scan_complete(clone!(
            @strong subtitle_row, @strong scanning, @strong spinner
                => move || {
                    if scanning.replace(false) {
                        spinner.set_spinning(false);
                        subtitle_row.set_visible(false);
                    }
                }
        ));

        // "Add device" button.
        add_btn.connect_clicked(clone!(@strong window, @strong manager => move |_| {
            Self::show_add_dialog(&window, &manager);
        }));

        // win.close action — lets Ctrl-W (set app-wide) close this window.
        {
            let close_act = gtk::gio::SimpleAction::new("close", None);
            close_act.connect_activate(clone!(@strong window => move |_, _| { window.close(); }));
            window.add_action(&close_act);
        }

        // Hide when other windows are visible; quit (propagate) when last.
        window.connect_close_request(clone!(@strong window => move |w| {
            let (ww, wh) = (w.width(), w.height());
            config::update(|cfg| {
                cfg.discovery_open = false;
                if ww > 0 { cfg.discovery_window_width  = ww; }
                if wh > 0 { cfg.discovery_window_height = wh; }
            });

            let others_visible = w.application().map_or(false, |app| {
                app.windows().iter().any(|other| {
                    other.upcast_ref::<gtk::Widget>() != w.upcast_ref::<gtk::Widget>()
                        && other.is_visible()
                })
            });
            if others_visible {
                w.hide();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }));

        Self { window }
    }

    pub fn present(&self) {
        config::update(|cfg| cfg.discovery_open = true);
        self.window.present();
    }

    fn rebuild_list(
        list_box:        &gtk::ListBox,
        entries:         &[ManagedEntry],
        current_entries: &Rc<RefCell<Vec<ManagedEntry>>>,
        row_widgets:     &Rc<RefCell<HashMap<String, RowWidgets>>>,
        manager:         &DiscoveryManager,
        icons:           &Rc<IconSet>,
    ) {
        // Explicit `unparent()` before the row widgets themselves are torn
        // down below — `gtk::Popover::set_parent()` (used for each row's
        // volume popover) isn't automatically unparented when its owning
        // button is destroyed, so skipping this logs "Finalizing GtkButton
        // but still has children left: GtkPopover" on every rebuild that
        // drops a row with song info on (confirmed live, first SSDP
        // response). See `RowWidgets::vol_popover`'s doc comment.
        for rw in row_widgets.borrow().values() {
            rw.vol_popover.unparent();
        }
        while let Some(child) = list_box.first_child() {
            list_box.remove(&child);
        }
        // Must match row append order exactly — `list_box.connect_row_activated`
        // looks a clicked row up by index into this.
        *current_entries.borrow_mut() = entries.to_vec();
        // Rebuilt from scratch alongside the rows themselves (structural
        // change — every widget is new) — `song-info-changed`'s handler
        // only ever updates a row that's still in here.
        row_widgets.borrow_mut().clear();
        if entries.is_empty() {
            let placeholder = adw::ActionRow::builder()
                .title("No devices found")
                .sensitive(false)
                .build();
            list_box.append(&placeholder);
            return;
        }
        for entry in entries {
            list_box.append(&Self::build_row(entry, row_widgets, manager, icons));
        }
    }

    fn build_row(
        entry:       &ManagedEntry,
        row_widgets: &Rc<RefCell<HashMap<String, RowWidgets>>>,
        manager:     &DiscoveryManager,
        icons:       &Rc<IconSet>,
    ) -> gtk::ListBoxRow {
        let key = device_key(&entry.uuid, &entry.ip);

        let hbox = gtk::Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(12)
            .margin_top(8).margin_bottom(8)
            .margin_start(12).margin_end(12)
            .build();

        // Artwork/icon slot — reserved at a fixed size (`.devlist-art`'s
        // CSS min-width/min-height) whenever song-info display is on
        // globally, regardless of whether *this* device currently has
        // anything to show there, so the row's right-hand side (volume
        // control, pin button) never shifts as devices update. Same
        // FlipCover widget (flip/crossfade between real art and the
        // fallback icon) the main and mini windows use, not a separate
        // plain-image path.
        let flip = entry.song_info_enabled.then(|| {
            let flip = FlipCover::new();
            flip.set_hexpand(false);
            flip.set_vexpand(false);
            flip.set_valign(gtk::Align::Center);
            flip.add_css_class("devlist-art");
            flip.set_overflow(gtk::Overflow::Hidden);
            apply_now_playing(&flip, icons, entry);
            hbox.append(&flip);
            flip
        });

        let text_box = gtk::Box::builder()
            .orientation(Orientation::Vertical)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();

        let title_label = gtk::Label::builder()
            .label(&entry.name)
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        text_box.append(&title_label);

        // "Title · Artist" (round-dot separator, same as the mini window's
        // artist/album line) when now-playing content is available; falls
        // back to the model name otherwise. A ScrollFadeLabel — not a
        // plain Label, and deliberately not AdwActionRow's own `subtitle`
        // property — for two reasons: it scrolls long text instead of
        // silently truncating, and it sets plain Pango text rather than
        // markup (`subtitle` interprets markup, which broke on a literal
        // "&" in a device or track name — moot here since ScrollFadeLabel
        // never parses its text as markup at all).
        let subtitle = ScrollFadeLabel::new(&subtitle_text_for(entry));
        subtitle.set_halign(gtk::Align::Start);
        subtitle.set_hexpand(true);
        subtitle.add_label_css_class("dim-label");
        subtitle.add_label_css_class("caption");
        text_box.append(&subtitle);

        hbox.append(&text_box);

        // IP address moved from a permanently-visible label to a hover
        // tooltip on the whole row — freeing that space for the volume
        // control below.
        let status_suffix = match entry.presence {
            DevicePresence::Active => String::new(),
            DevicePresence::Ghost | DevicePresence::Dead => " · offline".to_string(),
        };
        let row_tooltip = format!("{}{}", entry.ip, status_suffix);

        // Volume button + popover slider + mute, same widget shape as the
        // mini window's own. Reserved alongside the artwork slot (same
        // `song_info_enabled` gate — volume data is only kept fresh while
        // Simple-mode's fuller poll is active, same as title/artist), and
        // greyed out for a device that isn't `Active` right now rather
        // than hidden, so the row's layout doesn't shift either way. A
        // click on it doesn't open the device window — a `GtkButton`
        // child claims its own click before the row's own click-to-
        // activate gesture sees it, same as the pin button already relies
        // on (never special-cased, just how GTK widgets nest).
        let vol_widgets = entry.song_info_enabled.then(|| {
            let (vol_btn, vol_icon_img, vol_label, vol_scale, mute_btn, vol_popover) = build_devlist_vol_popover();
            vol_btn.set_sensitive(entry.presence == DevicePresence::Active);
            vol_btn.connect_clicked(clone!(@weak vol_popover => move |_| {
                if vol_popover.is_visible() { vol_popover.popdown(); } else { vol_popover.popup(); }
            }));
            hbox.append(&vol_btn);
            (vol_icon_img, vol_label, vol_scale, mute_btn, vol_popover)
        });

        if let (Some(flip), Some((vol_icon_img, vol_label, vol_scale, mute_btn, vol_popover))) = (flip, vol_widgets) {
            let vol_drag_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

            if let Some(ds) = manager.device_state_for(&key) {
                let rw_for_sync = RowWidgets {
                    flip: flip.clone(), subtitle: subtitle.clone(),
                    vol_icon_img: vol_icon_img.clone(), vol_label: vol_label.clone(),
                    vol_scale: vol_scale.clone(), mute_btn: mute_btn.clone(),
                    vol_popover: vol_popover.clone(),
                    vol_drag_timer: vol_drag_timer.clone(),
                };
                sync_devlist_vol_display(&rw_for_sync, &ds);

                mute_btn.connect_clicked(clone!(@strong ds => move |_| {
                    ds.do_set_mute(!ds.muted());
                }));
                vol_scale.connect_change_value(clone!(
                    @strong ds, @strong vol_icon_img, @strong vol_label, @strong vol_drag_timer
                        => move |_, _, vol| {
                            let icon = vol_icon(ds.muted(), vol);
                            vol_icon_img.set_icon_name(Some(icon));
                            vol_label.set_label(&format!("{}", vol as u32));
                            ds.do_set_volume(vol as u32);
                            if let Some(id) = vol_drag_timer.borrow_mut().take() { id.remove(); }
                            let timer_cell = Rc::clone(&vol_drag_timer);
                            let id = glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                                timer_cell.borrow_mut().take();
                            });
                            *vol_drag_timer.borrow_mut() = Some(id);
                            glib::Propagation::Proceed
                        }
                ));
            }

            row_widgets.borrow_mut().insert(key, RowWidgets {
                flip, subtitle: subtitle.clone(),
                vol_icon_img, vol_label, vol_scale, mute_btn,
                vol_popover,
                vol_drag_timer,
            });
        }

        // Pin / unpin toggle button.
        let pin_btn = gtk::ToggleButton::builder()
            .icon_name(if entry.pinned { "starred-symbolic" } else { "non-starred-symbolic" })
            .tooltip_text(if entry.pinned { "Unpin device" } else { "Pin device" })
            .active(entry.pinned)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        if entry.pinned {
            pin_btn.add_css_class("accent");
        }
        let uuid_for_pin = entry.uuid.clone();
        pin_btn.connect_toggled(clone!(@strong manager => move |btn| {
            manager.set_pinned(&uuid_for_pin, btn.is_active());
        }));
        hbox.append(&pin_btn);

        let row = gtk::ListBoxRow::builder()
            .activatable(true)
            .child(&hbox)
            .tooltip_text(&row_tooltip)
            .build();
        if entry.presence != DevicePresence::Active {
            row.add_css_class("dim-label");
        }
        row
    }

    fn show_add_dialog(parent: &adw::ApplicationWindow, manager: &DiscoveryManager) {
        let ip_entry = gtk::Entry::builder()
            .placeholder_text("192.168.1.x")
            .activates_default(true)
            .build();

        let dialog = adw::AlertDialog::builder()
            .heading("Add Device")
            .body("Enter the IP address of a WiiM device:")
            .close_response("cancel")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("add", "Add");
        dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("add"));
        dialog.set_extra_child(Some(&ip_entry));

        dialog.connect_response(None, clone!(
            @strong manager, @strong ip_entry
                => move |_dlg, resp| {
                    if resp != "add" { return; }
                    let ip = ip_entry.text().to_string();
                    if ip.is_empty() { return; }

                    let rt = manager.rt();
                    let (tx, rx) = async_channel::bounded::<Option<DiscoveredDevice>>(1);
                    let ip2 = ip.clone();
                    rt.spawn(async move {
                        let result = crate::device::discovery::DiscoveryService::probe_device(&ip2).await;
                        let _ = tx.send(result).await;
                    });

                    glib::spawn_future_local(clone!(@strong manager => async move {
                        if let Ok(Some(dev)) = rx.recv().await {
                            manager.add_manual(dev.name, dev.ip, dev.uuid, dev.tls_mode);
                        } else {
                            eprintln!("[devlist-ui] Could not reach device at {ip}");
                        }
                    }));
                }
        ));

        dialog.present(Some(parent));
    }
}
