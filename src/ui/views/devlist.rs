//! # DeviceListView
//!
//! Renders `device::discovery_manager::DiscoveryManager`'s tracked-device
//! list: artwork/icon, title/subtitle, a compact volume popover, and a
//! pin/unpin toggle per row — the exact same widget shapes `DiscoveryWindow`
//! always built, just relocated here so more than one host can embed the
//! list. Owns no window chrome (header, "Add device" dialog, menu,
//! settings/close actions) — those stay with whichever window hosts it.
//!
//! Clicking a row doesn't open anything itself — it emits `device-selected`
//! (the row's `device_key()`) so each host decides what "selected" means:
//! `DiscoveryWindow` opens a device window; a future Kiosk-mode popover
//! rebinds its active device instead.
//!
//! No `active`-gating (unlike the playback views) — rows stay live-patched
//! regardless of whether this view is currently visible. Simpler for now;
//! revisit only if that turns out to cost something real (e.g. rebuilding
//! this view on every popover open/close in Kiosk mode being slow).

pub mod imp {
    use std::cell::{OnceCell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::sync::OnceLock;

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use gtk::glib;
    use glib::subclass::Signal;

    use crate::device::discovery_manager::{DiscoveryManager, ManagedEntry};
    use crate::ui::icons::IconSet;
    use super::RowWidgets;

    #[derive(Default)]
    pub struct DeviceListView {
        pub(super) manager:         OnceCell<DiscoveryManager>,
        pub(super) icons:           OnceCell<Rc<IconSet>>,
        pub(super) list_box:        OnceCell<gtk::ListBox>,
        pub(super) current_entries: RefCell<Vec<ManagedEntry>>,
        pub(super) row_widgets:     RefCell<HashMap<String, RowWidgets>>,
        pub(super) handlers:        RefCell<Vec<glib::SignalHandlerId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DeviceListView {
        const NAME: &'static str = "DeviceListView";
        type Type = super::DeviceListView;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for DeviceListView {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    // Host request: a row was clicked/activated. Carries the
                    // row's device_key() (uuid, or "ip:{ip}" for an
                    // unresolved manually-added device) rather than a
                    // pre-built DeviceSpec/ManagedEntry — GObject signal
                    // params must be glib::Value-able, and this mirrors
                    // DiscoveryManager's own song-info-changed convention
                    // (host resolves the key back via manager.entry_for()).
                    Signal::builder("device-selected")
                        .param_types([String::static_type()])
                        .build(),
                ]
            })
        }

        fn dispose(&self) {
            if let Some(manager) = self.manager.get() {
                for id in self.handlers.take() {
                    manager.disconnect(id);
                }
            }
            // Same concern the pre-refactor DiscoveryWindow::rebuild_list()
            // guarded against: gtk::Popover::set_parent() isn't
            // auto-unparented when its owning button is torn down.
            for rw in self.row_widgets.borrow().values() {
                rw.vol_popover.unparent();
            }
        }
    }
    impl WidgetImpl for DeviceListView {}
    impl BinImpl for DeviceListView {}
}

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::clone;
use gtk::{glib, Orientation};

use crate::device::discovery_manager::{DevicePresence, DiscoveryManager, ManagedEntry, device_key};
use crate::device::state::DeviceState;
use crate::ui::flip_cover::FlipCover;
use crate::ui::icons::IconSet;
use crate::ui::scroll_fade_label::ScrollFadeLabel;
use super::common::vol_icon;

glib::wrapper! {
    pub struct DeviceListView(ObjectSubclass<imp::DeviceListView>)
        @extends adw::Bin, gtk::Widget;
}

/// The subset of a row's widgets `song-info-changed` needs to update in
/// place — keyed by `device_key()`'s result, rebuilt (not incrementally
/// patched) whenever `rebuild_list()` runs. Only present for rows built
/// while `entry.song_info_enabled` was true (see `build_row()`).
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
    /// children left: GtkPopover".
    vol_popover: gtk::Popover,
    /// Same drag-protection pattern as the main/mini windows' own
    /// `drag_timer` — while set, a live poll update (`song-info-changed`)
    /// skips repositioning the slider so it doesn't fight an in-progress
    /// drag.
    vol_drag_timer: Rc<RefCell<Option<glib::SourceId>>>,
}

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

/// Volume button + popover slider + mute button, same shape as
/// `views::volume::VolumeControl` (see `.devlist-vol-*` CSS) sized for a
/// compact row rather than a standalone window. Caller wires the actual
/// click/drag/mute handlers. Replacing this with a real `VolumeControl` is
/// `PLAYBACK_STACKS.md` Phase 3 material — rows don't hold a `DeviceState`
/// to bind one to.
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

impl DeviceListView {
    pub(crate) fn new(manager: &DiscoveryManager, icons: &Rc<IconSet>) -> Self {
        let obj: Self = glib::Object::new();
        obj.build(manager, icons);
        obj
    }

    pub(crate) fn connect_device_selected<F: Fn(&Self, &str) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("device-selected", false, move |args| {
            let obj = args[0].get::<Self>().unwrap();
            let key = args[1].get::<String>().unwrap();
            f(&obj, &key);
            None
        })
    }

    fn build(&self, manager: &DiscoveryManager, icons: &Rc<IconSet>) {
        let imp = self.imp();
        imp.manager.set(manager.clone()).unwrap();
        let _ = imp.icons.set(Rc::clone(icons));

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
        self.set_child(Some(&scroll));
        let _ = imp.list_box.set(list_box.clone());

        self.rebuild_list();

        // List rebuild: structural changes only (device added/removed/
        // renamed/pinned/moved, presence flips) — see DiscoveryManager's
        // `signals()`.
        let h_list = manager.connect_list_changed({
            let weak = self.downgrade();
            move |_mgr| {
                let Some(this) = weak.upgrade() else { return };
                this.rebuild_list();
            }
        });

        // One device's now-playing content changed — update just its row
        // in place instead of rebuilding the whole list. Only touches the
        // widget(s) whose own bit actually fired, matching
        // `ui/playback.rs`'s `update_playback_ui()`.
        let h_song = manager.connect_song_info_changed({
            let weak = self.downgrade();
            move |mgr, key, mask| {
                let Some(this) = weak.upgrade() else { return };
                this.apply_song_info(mgr, key, mask);
            }
        });
        imp.handlers.borrow_mut().extend([h_list, h_song]);

        list_box.connect_row_activated({
            let weak = self.downgrade();
            move |_, row| {
                let Some(this) = weak.upgrade() else { return };
                let idx = row.index();
                if idx < 0 { return; }
                let key = this.imp().current_entries.borrow().get(idx as usize)
                    .map(|entry| device_key(&entry.uuid, &entry.ip));
                if let Some(key) = key {
                    this.emit_by_name::<()>("device-selected", &[&key]);
                }
            }
        });
    }

    fn apply_song_info(&self, mgr: &DiscoveryManager, key: &str, mask: u32) {
        use crate::device::state::playback_changed as PC;
        let imp = self.imp();
        let Some(entry) = mgr.entry_for(key) else { return };
        let widgets = imp.row_widgets.borrow();
        let Some(rw) = widgets.get(key) else { return };
        if mask & (PC::TITLE | PC::ARTIST) != 0 {
            rw.subtitle.set_text(&subtitle_text_for(&entry));
        }
        if mask & PC::ARTWORK != 0 {
            apply_now_playing(&rw.flip, imp.icons.get().unwrap(), &entry);
        }
        if mask & PC::VOLUME != 0 {
            if let Some(ds) = mgr.device_state_for(key) {
                sync_devlist_vol_display(rw, &ds);
            }
        }
    }

    fn rebuild_list(&self) {
        let imp = self.imp();
        let manager = imp.manager.get().unwrap().clone();
        let list_box = imp.list_box.get().unwrap().clone();
        let entries = manager.entries();

        // Explicit `unparent()` before the row widgets themselves are torn
        // down below — see `RowWidgets::vol_popover`'s doc comment.
        for rw in imp.row_widgets.borrow().values() {
            rw.vol_popover.unparent();
        }
        while let Some(child) = list_box.first_child() {
            list_box.remove(&child);
        }
        // Must match row append order exactly — `connect_row_activated`
        // above looks a clicked row up by index into this.
        *imp.current_entries.borrow_mut() = entries.clone();
        // Rebuilt from scratch alongside the rows themselves (structural
        // change — every widget is new) — `apply_song_info()` only ever
        // updates a row that's still in here.
        imp.row_widgets.borrow_mut().clear();
        if entries.is_empty() {
            let placeholder = adw::ActionRow::builder()
                .title("No devices found")
                .sensitive(false)
                .build();
            list_box.append(&placeholder);
            return;
        }
        for entry in &entries {
            list_box.append(&self.build_row(entry, &manager));
        }
    }

    fn build_row(&self, entry: &ManagedEntry, manager: &DiscoveryManager) -> gtk::ListBoxRow {
        let imp = self.imp();
        let icons = imp.icons.get().unwrap();
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
            vol_btn.connect_clicked(clone!(#[weak] vol_popover, move |_| {
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

                mute_btn.connect_clicked(clone!(#[strong] ds, move |_| {
                    ds.do_set_mute(!ds.muted());
                }));
                vol_scale.connect_change_value(clone!(
                    #[strong] ds, #[strong] vol_icon_img, #[strong] vol_label, #[strong] vol_drag_timer
                       , move |_, _, vol| {
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

            imp.row_widgets.borrow_mut().insert(key.clone(), RowWidgets {
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
        pin_btn.connect_toggled(clone!(#[strong] manager, move |btn| {
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
}
