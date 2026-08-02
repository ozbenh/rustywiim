//! # DeviceListView
//!
//! Renders `device::manager::DeviceManager`'s tracked-device
//! list: artwork/icon, title/subtitle, a compact volume popover, and an
//! offline-only "forget this device" trashcan per row — the exact same
//! widget shapes `DiscoveryWindow` always built, just relocated here so more
//! than one host can embed the list. Owns no window chrome (header, "Add
//! device" dialog, menu, settings/close actions) — those stay with whichever
//! window hosts it.
//!
//! Clicking a row doesn't open anything itself — it emits `device-selected`
//! (the row's uuid) so each host decides what "selected" means:
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

    use crate::device::manager::{DeviceManager, ManagedEntry};
    use crate::ui::icons::IconSet;
    use super::RowWidgets;

    #[derive(Default)]
    pub struct DeviceListView {
        pub(super) manager:         OnceCell<DeviceManager>,
        pub(super) icons:           OnceCell<Rc<IconSet>>,
        pub(super) list_box:        OnceCell<gtk::ListBox>,
        pub(super) current_entries: RefCell<Vec<ManagedEntry>>,
        pub(super) row_widgets:     RefCell<HashMap<String, RowWidgets>>,
        pub(super) member_widgets:  RefCell<HashMap<String, super::MemberWidgets>>,
        /// One key per appended `ListBoxRow`, in order — what a click
        /// resolves to. Separate from `current_entries` because a group
        /// contributes several entries but only one row.
        pub(super) row_keys:        RefCell<Vec<String>>,
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
                    // row's uuid rather than a pre-built DeviceSpec/
                    // ManagedEntry — GObject signal params must be
                    // glib::Value-able, and this mirrors DeviceManager's
                    // own song-info-changed convention (host resolves the
                    // key back via manager.entry_for()).
                    Signal::builder("device-selected")
                        .param_types([String::static_type()])
                        .build(),
                    // Host request: open settings scoped to one device.
                    // Carries the same uuid as `device-selected`.
                    // A group member has no device window of its own — only
                    // the group does — so its member line's cog is the only
                    // route to that device's own settings.
                    Signal::builder("device-settings")
                        .param_types([String::static_type()])
                        .build(),
                    // Host request: forget a standalone, offline device —
                    // the trashcan button (see `build_device_content()`).
                    // Carries the same uuid convention as the other two
                    // signals here; the host owns the actual removal
                    // (closing windows, dropping config), this view only
                    // ever reports the click.
                    Signal::builder("device-forget")
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
            for mw in self.member_widgets.borrow().values() {
                mw.vol_popover.unparent();
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

use crate::device::manager::{DeviceManager, DevicePresence, EntryGroupRole, ManagedEntry};
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
/// place — keyed by uuid, rebuilt (not incrementally
/// patched) whenever `rebuild_list()` runs. Only present for rows built
/// while `entry.song_info_enabled` was true (see `build_row()`).
/// A member line's volume controls. Kept apart from `RowWidgets` because a
/// leader occupies two rows — a group header *and* a member line — so one
/// map keyed by device cannot hold both, and because a member line has no
/// artwork or subtitle to update.
struct MemberWidgets {
    vol_icon_img: gtk::Image,
    vol_label:    gtk::Label,
    vol_scale:    gtk::Scale,
    mute_btn:     gtk::Button,
    /// Same `set_parent()` teardown caveat as `RowWidgets::vol_popover`.
    vol_popover:  gtk::Popover,
    vol_drag_timer: Rc<RefCell<Option<glib::SourceId>>>,
}

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
/// to the model name otherwise — including while idle, where `np.title` is
/// a real placeholder ("No music selected") rather than empty, so `is_idle`
/// (not `title`'s content) is what gates the fallback here. Shared by
/// `build_row()` (initial render) and the `song-info-changed` handler
/// (in-place update).
fn subtitle_text_for(entry: &ManagedEntry) -> String {
    let base = match &entry.now_playing {
        Some(np) if np.is_idle => entry.model.clone(),
        Some(np) if !np.title.is_empty() && !np.artist.is_empty() => format!("{} \u{00b7} {}", np.title, np.artist),
        Some(np) if !np.title.is_empty()  => np.title.clone(),
        Some(np) if !np.artist.is_empty() => np.artist.clone(),
        _ => entry.model.clone(),
    };
    // `now_playing` is only ever populated for an `Active` device (see
    // `build_managed_entry()`), so `base` here is always `entry.model` in
    // practice — but this stays a presence check, not an emptiness check,
    // so it keeps meaning "offline" if that invariant ever changes.
    if entry.presence == DevicePresence::Offline {
        if base.is_empty() { "Offline".to_string() } else { format!("{base} (offline)") }
    } else {
        base
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
/// click/drag/mute handlers. 
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
/// `sync_devlist_vol_display()`'s counterpart for a member line, which has
/// the same volume widgets but none of the artwork/subtitle ones.
fn sync_member_vol_display(mw: &MemberWidgets, ds: &DeviceState) {
    apply_member_vol_display(mw, ds.playback_state().volume as f64, ds.muted());
}

/// `sync_member_vol_display()`'s raw-value counterpart, for a relay-only
/// member: it has no `DeviceState` of its own to read, only whatever its
/// leader's `member_level()` reports from the cached slave list.
fn apply_member_vol_display(mw: &MemberWidgets, vol: f64, muted: bool) {
    if mw.vol_drag_timer.borrow().is_some() {
        return;
    }
    mw.vol_scale.set_value(vol);
    mw.vol_label.set_label(&format!("{}", vol as u32));
    mw.vol_icon_img.set_icon_name(Some(vol_icon(muted, vol)));
    mw.mute_btn.set_icon_name(if muted { "audio-volume-muted-symbolic" } else { "audio-volume-high-symbolic" });
}

fn sync_devlist_vol_display(rw: &RowWidgets, ds: &DeviceState) {
    // Group-aware, same as the shared `VolumeControl`: this row is either a
    // standalone device or a group's own row, and `group_volume()` covers
    // both. Member lines use `sync_member_vol_display()` instead, which is
    // deliberately per-device.
    let vol = ds.group_volume() as f64;
    let muted = ds.group_muted();
    if rw.vol_drag_timer.borrow().is_none() {
        rw.vol_scale.set_value(vol);
    }
    rw.vol_icon_img.set_icon_name(Some(vol_icon(muted, vol)));
    rw.vol_label.set_label(&format!("{}", vol as u32));
    rw.mute_btn.set_icon_name(if muted { "audio-volume-muted-symbolic" } else { "audio-volume-high-symbolic" });
}

impl DeviceListView {
    pub(crate) fn new(manager: &DeviceManager, icons: &Rc<IconSet>) -> Self {
        let obj: Self = glib::Object::new();
        obj.build(manager, icons);
        obj
    }

    pub(crate) fn connect_device_settings<F: Fn(&Self, &str) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("device-settings", false, move |args| {
            let this = args[0].get::<Self>().unwrap();
            let key  = args[1].get::<String>().unwrap();
            f(&this, &key);
            None
        })
    }

    pub(crate) fn connect_device_forget<F: Fn(&Self, &str) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("device-forget", false, move |args| {
            let this = args[0].get::<Self>().unwrap();
            let key  = args[1].get::<String>().unwrap();
            f(&this, &key);
            None
        })
    }

    pub(crate) fn connect_device_selected<F: Fn(&Self, &str) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("device-selected", false, move |args| {
            let obj = args[0].get::<Self>().unwrap();
            let key = args[1].get::<String>().unwrap();
            f(&obj, &key);
            None
        })
    }

    fn build(&self, manager: &DeviceManager, icons: &Rc<IconSet>) {
        let imp = self.imp();
        imp.manager.set(manager.clone()).unwrap();
        let _ = imp.icons.set(Rc::clone(icons));

        // "devlist-rows" (styled per-theme, see system.css/dark.css) replaces
        // libadwaita's own ".boxed-list" — that draws one solid card with
        // thin dividers between rows, which gives a group's card (header +
        // member lines, all inside one row) no visual boundary distinct
        // from an adjacent standalone row. Individually rounded, gapped
        // rows make a group's own edges legible among plain entries.
        // `valign(Start)` (GtkListBox otherwise fills the viewport's full
        // height) is what lets the empty space below the last row stay
        // transparent instead of "boxed-list"'s solid card background
        // extending to fill it — scrolling is unaffected once there are
        // enough rows to actually need it.
        let list_box = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["devlist-rows"])
            .valign(gtk::Align::Start)
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
        // renamed/moved, presence flips) — see DeviceManager's
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
                // Rows and entries are no longer one-to-one — a group's
                // member lines live inside their group's row — so the key
                // is tracked alongside the rows as they are appended rather
                // than looked up positionally. Activating anywhere in a
                // group's row opens the group.
                let key = this.imp().row_keys.borrow().get(idx as usize).cloned();
                if let Some(key) = key {
                    this.emit_by_name::<()>("device-selected", &[&key]);
                }
            }
        });
    }

    fn apply_song_info(&self, mgr: &DeviceManager, key: &str, mask: u32) {
        use crate::device::state::playback_changed as PC;
        let imp = self.imp();
        let Some(entry) = mgr.entry_for(key) else { return };
        if mask & PC::VOLUME != 0 {
            self.apply_member_volume(mgr, key);
            self.apply_group_levels_for_member(mgr, key);
            self.refresh_relay_member_lines(mgr, key);
        }
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

    /// A device can occupy a member line as well as (or instead of) a
    /// device row, so its volume has to be synced in both places.
    fn apply_member_volume(&self, mgr: &DeviceManager, key: &str) {
        let imp = self.imp();
        let widgets = imp.member_widgets.borrow();
        let Some(mw) = widgets.get(key) else { return };
        let Some(ds) = mgr.device_state_for(key) else { return };
        sync_member_vol_display(mw, &ds);
    }

    /// Refreshes the group row that owns `key`'s member line, if any.
    ///
    /// A group's volume is the loudest member's and its mute is the AND of
    /// all of them, so changing *any* member changes what the group row
    /// should read — but the change arrives keyed by the member, and the
    /// group row is keyed by its leader. Without this the group row only
    /// updates when the leader itself happens to change.
    fn apply_group_levels_for_member(&self, mgr: &DeviceManager, key: &str) {
        let imp = self.imp();
        let leader_key = imp.current_entries.borrow().iter()
            .find(|e| e.uuid == key)
            .and_then(|e| match &e.group_role {
                EntryGroupRole::Member { leader_key, .. } => Some(leader_key.clone()),
                _ => None,
            });
        let Some(leader_key) = leader_key else { return };
        let widgets = imp.row_widgets.borrow();
        let Some(rw) = widgets.get(&leader_key) else { return };
        let Some(leader_ds) = mgr.device_state_for(&leader_key) else { return };
        sync_devlist_vol_display(rw, &leader_ds);
    }

    /// Refreshes every relay-only member line under `leader_key`, when
    /// `leader_key`'s *own* volume just changed.
    ///
    /// A relay-only member (no `DeviceState`, see `build_member_content()`)
    /// has no way to emit its own `song-info-changed` — only its leader
    /// does, whether that's a whole-group `set_group_volume()`/
    /// `set_group_muted()` shifting every member at once, or
    /// `DeviceState::set_member_volume()`/`set_member_muted()` driving just
    /// this one. So unlike `apply_group_levels_for_member()` (member →
    /// header), this direction has to walk from the header back out to its
    /// member lines instead of relying on a per-member signal.
    fn refresh_relay_member_lines(&self, mgr: &DeviceManager, leader_key: &str) {
        let imp = self.imp();
        let Some(leader_ds) = mgr.device_state_for(leader_key) else { return };
        let member_uuids: Vec<String> = imp.current_entries.borrow().iter()
            .filter(|e| matches!(&e.group_role,
                EntryGroupRole::Member { leader_key: lk, tracked: false } if lk == leader_key))
            .map(|e| e.uuid.clone())
            .collect();
        let widgets = imp.member_widgets.borrow();
        for uuid in member_uuids {
            let Some(mw) = widgets.get(&uuid) else { continue };
            let Some((vol, muted)) = leader_ds.member_level(&uuid) else { continue };
            apply_member_vol_display(mw, vol as f64, muted);
        }
    }

    fn rebuild_list(&self) {
        let imp = self.imp();
        let manager = imp.manager.get().unwrap().clone();
        let list_box = imp.list_box.get().unwrap().clone();
        let entries = manager.entries(&crate::ui::name_resolver_for(&manager));

        // Explicit `unparent()` before the row widgets themselves are torn
        // down below — see `RowWidgets::vol_popover`'s doc comment.
        for rw in imp.row_widgets.borrow().values() {
            rw.vol_popover.unparent();
        }
        for mw in imp.member_widgets.borrow().values() {
            mw.vol_popover.unparent();
        }
        while let Some(child) = list_box.first_child() {
            list_box.remove(&child);
        }
        *imp.current_entries.borrow_mut() = entries.clone();
        imp.row_keys.borrow_mut().clear();
        // Rebuilt from scratch alongside the rows themselves (structural
        // change — every widget is new) — `apply_song_info()` only ever
        // updates a row that's still in here.
        imp.row_widgets.borrow_mut().clear();
        imp.member_widgets.borrow_mut().clear();
        if entries.is_empty() {
            let placeholder = adw::ActionRow::builder()
                .title("No devices found")
                .sensitive(false)
                .build();
            list_box.append(&placeholder);
            return;
        }
        // A group is one row containing its member lines, not a run of
        // sibling rows — otherwise the boxed-list styling draws a separator
        // between each and the group reads as several unrelated devices.
        let mut i = 0;
        while i < entries.len() {
            let entry = &entries[i];
            let (row, key) = match entry.group_role {
                EntryGroupRole::GroupHeader { .. } => {
                    let start = i + 1;
                    let mut end = start;
                    while end < entries.len()
                        && matches!(entries[end].group_role, EntryGroupRole::Member { .. })
                    {
                        end += 1;
                    }
                    i = end;
                    let key = entry.uuid.clone();
                    (self.build_group_card(entry, &entries[start..end], &manager), key)
                }
                _ => {
                    i += 1;
                    let key = entry.uuid.clone();
                    (self.build_device_row(entry, &manager), key)
                }
            };
            list_box.append(&row);
            imp.row_keys.borrow_mut().push(key);
        }
    }

    /// One group, rendered as a single row: the group's own content on top,
    /// then a stacked line per member inside the same row. Keeping it to
    /// one `ListBoxRow` is what makes the boxed list draw a single outline
    /// around the whole group rather than separating every line.
    fn build_group_card(
        &self, header: &ManagedEntry, members: &[ManagedEntry], manager: &DeviceManager,
    ) -> gtk::ListBoxRow {
        // Line the member names up with the group's own name rather than
        // with its artwork, so the block reads as one indented list. The
        // group content's name starts past its artwork slot, which is only
        // present when song info is on: row margin + art width + spacing.
        let indent = if header.song_info_enabled { 12 + 32 + 12 } else { 12 };

        let vbox = gtk::Box::builder().orientation(Orientation::Vertical).build();
        vbox.append(&self.build_group_header_content(header, manager));
        for m in members {
            vbox.append(&self.build_member_content(m, manager, indent));
        }

        let row = gtk::ListBoxRow::builder()
            .activatable(true)
            .child(&vbox)
            .tooltip_text(&header.ip)
            .build();
        row.add_css_class("devlist-group");
        if header.presence != DevicePresence::Active {
            row.add_css_class("dim-label");
        }
        row
    }

    /// One member of a group: name, its own volume, and a settings cog.
    /// Deliberately much shorter than a device row — no artwork slot, no
    /// song info — since the group's own content above already shows what
    /// is playing, and repeating it per member would be noise.
    ///
    /// Returns the content box rather than a row: member lines are packed
    /// inside their group's single row, not appended as siblings.
    fn build_member_content(
        &self, entry: &ManagedEntry, manager: &DeviceManager, indent: i32,
    ) -> gtk::Box {
        let imp = self.imp();
        let key = entry.uuid.clone();

        let hbox = gtk::Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .margin_top(2).margin_bottom(2)
            .margin_start(indent).margin_end(12)
            .build();

        let name = gtk::Label::builder()
            .label(&entry.name)
            .halign(gtk::Align::Start)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        name.add_css_class("caption");
        hbox.append(&name);

        let (tracked, leader_key) = match &entry.group_role {
            EntryGroupRole::Member { tracked, leader_key } => (*tracked, leader_key.clone()),
            _ => (false, String::new()),
        };

        // A member with no `DeviceState` normally has nothing to drive a
        // slider with — except a *relay-only* one (the leader named it but
        // this host has no connection of its own to it, including the
        // unroutable WiFi-Direct case), which is still controllable by
        // relaying through its leader
        // (`DeviceState::set_member_volume()`/`set_member_muted()`).
        // `entry.presence` for such a member is always `Offline`
        // (`resolve_topology()`'s own doc comment — "the leader's word is
        // not a connection"), so it can't gate this the way a standalone
        // row's does; a reported `ip` is what `group::member_is_relayable()`
        // actually tests, mirrored here.
        let relay_only = !tracked && !entry.ip.is_empty();
        enum VolSource { Own(DeviceState), Relay(DeviceState) }
        let vol_source = if tracked {
            manager.device_state_for(&key)
                .filter(|_| entry.presence == DevicePresence::Active)
                .map(VolSource::Own)
        } else if relay_only {
            manager.device_state_for(&leader_key).map(VolSource::Relay)
        } else {
            None
        };
        if let Some(source) = vol_source {
            let (vol_btn, vol_icon_img, vol_label, vol_scale, mute_btn, vol_popover) =
                build_devlist_vol_popover();
            vol_btn.connect_clicked(clone!(#[weak] vol_popover, move |_| {
                if vol_popover.is_visible() { vol_popover.popdown(); } else { vol_popover.popup(); }
            }));
            hbox.append(&vol_btn);

            let vol_drag_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
            let mw = MemberWidgets {
                vol_icon_img: vol_icon_img.clone(), vol_label: vol_label.clone(),
                vol_scale: vol_scale.clone(), mute_btn: mute_btn.clone(),
                vol_popover: vol_popover.clone(), vol_drag_timer: vol_drag_timer.clone(),
            };

            match source {
                VolSource::Own(ds) => {
                    sync_member_vol_display(&mw, &ds);
                    mute_btn.connect_clicked(clone!(#[strong] ds, move |_| {
                        ds.do_set_mute(!ds.muted());
                    }));
                    vol_scale.connect_change_value(clone!(
                        #[strong] ds, #[strong] vol_icon_img, #[strong] vol_label, #[strong] vol_drag_timer,
                        move |_, _, vol| {
                            let icon = vol_icon(ds.muted(), vol);
                            vol_icon_img.set_icon_name(Some(icon));
                            vol_label.set_label(&format!("{}", vol as u32));
                            // A member line is that device's own volume,
                            // never the group's — the group's lives on the
                            // row above.
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
                VolSource::Relay(leader_ds) => {
                    if let Some((vol, muted)) = leader_ds.member_level(&key) {
                        apply_member_vol_display(&mw, vol as f64, muted);
                    }
                    mute_btn.connect_clicked(clone!(#[strong] leader_ds, #[strong] key, move |_| {
                        let muted = leader_ds.member_level(&key).is_none_or(|(_, m)| !m);
                        leader_ds.set_member_muted(&key, muted);
                    }));
                    vol_scale.connect_change_value(clone!(
                        #[strong] leader_ds, #[strong] key,
                        #[strong] vol_icon_img, #[strong] vol_label, #[strong] vol_drag_timer,
                        move |_, _, vol| {
                            let muted = leader_ds.member_level(&key).is_some_and(|(_, m)| m);
                            let icon = vol_icon(muted, vol);
                            vol_icon_img.set_icon_name(Some(icon));
                            vol_label.set_label(&format!("{}", vol as u32));
                            // Relayed through the leader, not driven
                            // directly — this member has no `DeviceState`
                            // of its own.
                            leader_ds.set_member_volume(&key, vol as u32);
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
            }
            imp.member_widgets.borrow_mut().insert(key.clone(), mw);
        }

        // The only route to a member's own settings: a member has no
        // device window, since opening one from this list opens the group.
        // A relay-only member has no `DeviceState` at all, so — same as
        // before this line gained a volume control — it gets none either.
        let cog = gtk::Button::builder()
            .icon_name("emblem-system-symbolic")
            .tooltip_text("Device settings")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        cog.set_sensitive(tracked);
        cog.connect_clicked(clone!(#[weak(rename_to = this)] self, #[strong] key, move |_| {
            this.emit_by_name::<()>("device-settings", &[&key]);
        }));
        hbox.append(&cog);

        hbox.add_css_class("devlist-member");
        if entry.presence != DevicePresence::Active {
            hbox.add_css_class("dim-label");
        }
        hbox.set_tooltip_text(Some(&entry.ip));
        hbox
    }

    /// A group's own content: the same treatment a device row gets, minus
    /// the pin button.
    fn build_group_header_content(&self, entry: &ManagedEntry, manager: &DeviceManager) -> gtk::Box {
        self.build_device_content(entry, manager)
    }

    fn build_device_row(&self, entry: &ManagedEntry, manager: &DeviceManager) -> gtk::ListBoxRow {
        let hbox = self.build_device_content(entry, manager);
        let status_suffix = match entry.presence {
            DevicePresence::Active  => String::new(),
            DevicePresence::Offline => " · offline".to_string(),
        };
        let row = gtk::ListBoxRow::builder()
            .activatable(true)
            .child(&hbox)
            .tooltip_text(&format!("{}{}", entry.ip, status_suffix))
            .build();
        if entry.presence != DevicePresence::Active {
            row.add_css_class("dim-label");
        }
        row
    }

    fn build_device_content(&self, entry: &ManagedEntry, manager: &DeviceManager) -> gtk::Box {
        let imp = self.imp();
        let icons = imp.icons.get().unwrap();
        let key = entry.uuid.clone();

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
        // control, trashcan button) never shifts as devices update. Same
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

        // The IP address is a hover tooltip rather than a visible label,
        // freeing that space for the volume control. Applied by whichever
        // caller wraps this content, since a group's tooltip belongs on the
        // whole card.

        // Volume button + popover slider + mute, same widget shape as the
        // mini window's own. Reserved alongside the artwork slot (same
        // `song_info_enabled` gate — volume data is only kept fresh while
        // Simple-mode's fuller poll is active, same as title/artist), and
        // hidden outright rather than merely greyed out while the device
        // isn't `Active` — an offline device has no live level to show, so
        // a stale/zeroed reading is worse than no control at all; a
        // presence flip already rebuilds the whole row (see
        // `on_tracked_device_changed()`), so there's no live-toggle case
        // to handle here. A click on it doesn't open the device window —
        // a `GtkButton` child claims its own click before the row's own
        // click-to-activate gesture sees it, same as the trashcan button
        // already relies on (never special-cased, just how GTK widgets
        // nest).
        let vol_widgets = (entry.song_info_enabled && entry.presence == DevicePresence::Active).then(|| {
            let (vol_btn, vol_icon_img, vol_label, vol_scale, mute_btn, vol_popover) = build_devlist_vol_popover();
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
                    // Group-aware: on a group's own row this mutes every
                    // member, and on a standalone device it falls through
                    // to that device's own mute.
                    ds.set_group_muted(!ds.group_muted());
                }));
                vol_scale.connect_change_value(clone!(
                    #[strong] ds, #[strong] vol_icon_img, #[strong] vol_label, #[strong] vol_drag_timer
                       , move |_, _, vol| {
                            let icon = vol_icon(ds.group_muted(), vol);
                            vol_icon_img.set_icon_name(Some(icon));
                            vol_label.set_label(&format!("{}", vol as u32));
                            // Group-aware: on a group's own row this shifts
                            // every member, and on a standalone device it
                            // falls through to that device's own volume.
                            ds.set_group_volume(vol as u32);
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

        // Offline-only "forget this device" trashcan. A group header gets
        // none — a group isn't a device, so it has nothing to forget (its
        // member lines are a separate, standalone-only concept too: a
        // member is reachable through its leader, so "offline" doesn't
        // apply to it the way this button means). Restricted to `Offline`
        // rather than shown unconditionally: removing a reachable device
        // would just have it reappear at the next discovery pass, and a
        // button whose effect visibly undoes itself seconds later reads as
        // broken. Tooltip deliberately doesn't say "delete"/"remove
        // permanently" for the same reason — the device really can come
        // back if it's seen again.
        if matches!(entry.group_role, EntryGroupRole::GroupHeader { .. }) {
            return hbox;
        }
        if entry.presence == DevicePresence::Offline {
            let forget_btn = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Remove from list")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            forget_btn.connect_clicked(clone!(#[weak(rename_to = this)] self, #[strong] key, move |_| {
                this.emit_by_name::<()>("device-forget", &[&key]);
            }));
            hbox.append(&forget_btn);
        }

        // Settings cog — every standalone row gets one (a group header
        // does not, same reasoning as the trashcan above: a group isn't a
        // device). Reuses the same `device-settings` signal a group
        // member's own line already emits (`build_member_content()`), so
        // both hosts (`DiscoveryWindow`, Kiosk's device popover) need only
        // the one handler.
        let cog = gtk::Button::builder()
            .icon_name("emblem-system-symbolic")
            .tooltip_text("Device settings")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        cog.connect_clicked(clone!(#[weak(rename_to = this)] self, #[strong] key, move |_| {
            this.emit_by_name::<()>("device-settings", &[&key]);
        }));
        hbox.append(&cog);

        hbox
    }
}
