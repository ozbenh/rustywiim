//! # PlayQueueView
//!
//! The live play-queue list (artwork + title/artist/album per track,
//! current-track highlighted, click to jump playback to that track) —
//! UPnP-only, see `device::upnp`'s `QueueTrackEntry`/`browse_current_queue()`
//! Follows the view lifecycle contract (see `views/mod.rs`): subscribes to
//! `queue-changed`/`device-changed` itself, early-returns while inactive,
//! full refresh on activation — including the offline rendering.
//!
//! Unlike `PresetsView`'s fixed 12-slot widget pool, the queue's length is
//! unbounded, so `refresh()` keeps a `Vec<Row>` sized to the current entry
//! count instead — but it **patches existing rows in place rather than
//! rebuilding them**: now that a refetch can be triggered by a live GENA
//! NOTIFY (frequent-ish on some devices — e.g. every track selection on an
//! Audio Pro unit), tearing down and recreating every
//! row's widgets on every `queue-changed` would mean real, avoidable churn.
//! `refresh()` only touches a row's widgets when that row's actual rendered
//! content (`Row::rendered`, compared via `QueueTrackEntry`'s `PartialEq`)
//! or current-track highlight actually differs from last time; rows are
//! only added/removed at the end when the entry count itself changes. A
//! virtualized `gtk::ListView` would scale better still for a very long
//! queue, but adds real complexity that isn't worth it until a real queue
//! is seen large enough to matter — including `SignalListItemFactory`'s
//! `setup`/`bind` split, where `bind` fires repeatedly over a `ListItem`'s
//! life, so child widgets must be built once in `setup` and only mutated
//! in `bind` (constructing fresh ones per `bind` races GTK's own hover
//! tracking on the discarded widget).
//!
//! Holds a `QueueWatchGuard` for as long as it's active — nothing polls
//! `BrowseQueue` at all while no `PlayQueueView` anywhere is active (see
//! `DeviceState::acquire_queue_watch()`).
//!
//! The header row (currently just a track count) is deliberately its own
//! `gtk::Box` rather than folded into the "PLAY QUEUE" label — Ben's plan
//! is to grow it with actual controls later (queue reorder, etc), so it's
//! built as a row that can take more children rather than a single label
//! that would need restructuring then.

pub mod imp {
    use std::cell::{Cell, OnceCell, RefCell};
    use std::rc::Rc;

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use gtk::glib;
    use gtk::{Button, Label};

    use crate::device::state::{DeviceState, QueueWatchGuard};
    use crate::device::upnp::QueueTrackEntry;

    /// One rendered row's widgets plus enough state to know whether a
    /// `refresh()` pass actually needs to touch them — see this module's
    /// top doc comment. `index_cell` is a separate `Rc<Cell<u32>>` (not a
    /// plain field the click handler captures by value) because a row's
    /// widgets are reused across refreshes for whatever entry now occupies
    /// that position, which can be a different track than when the row was
    /// first built — the click handler must always read the *current*
    /// index at click time, not the one captured when the closure was
    /// created.
    pub(super) struct Row {
        pub btn:      Button,
        pub pic:      gtk::Image,
        pub title:    Label,
        pub subtitle: Label,
        pub index_cell: Rc<Cell<u32>>,
        /// Last entry actually rendered into this row's widgets — `None`
        /// for a freshly built row that hasn't been painted yet.
        pub rendered: Option<QueueTrackEntry>,
        pub is_current: bool,
        /// The `art_uri` this row's `pic` currently shows real art for (as
        /// opposed to the fallback icon) — `None` means either no art URI,
        /// or one that hasn't resolved (fetched, or fetch failed) yet.
        pub art_applied_for: Option<String>,
    }

    #[derive(Default)]
    pub struct PlayQueueView {
        pub(super) ds:       OnceCell<DeviceState>,
        pub(super) handlers: RefCell<Vec<glib::SignalHandlerId>>,
        pub(super) active:   Cell<bool>,
        pub(super) watch:    RefCell<Option<QueueWatchGuard>>,
        pub(super) scroll:      OnceCell<gtk::ScrolledWindow>,
        pub(super) rows_box:    OnceCell<gtk::Box>,
        pub(super) count_label: OnceCell<Label>,
        pub(super) rows:        RefCell<Vec<Row>>,
        /// The `current_index` last seen by `refresh()` — compared on each
        /// call to decide whether the current-track row actually *changed*
        /// (as opposed to some other field triggering a redundant
        /// `queue-changed`), which is what `refresh()` uses to decide
        /// whether to scroll it into view. `set_active(true)` scrolls
        /// unconditionally instead (opening the tab should always show the
        /// current track, whether or not the index happened to change
        /// since it was last open).
        pub(super) last_current_index: Cell<Option<u32>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PlayQueueView {
        const NAME: &'static str = "PlayQueueView";
        type Type = super::PlayQueueView;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for PlayQueueView {
        fn dispose(&self) {
            if let Some(ds) = self.ds.get() {
                for id in self.handlers.take() {
                    ds.disconnect(id);
                }
            }
            // Dropping the guard (if any) stops queue polling — same
            // cleanup `set_active(false)` does, just on teardown paths that
            // skip that call.
            self.watch.take();
        }
    }
    impl WidgetImpl for PlayQueueView {}
    impl BinImpl for PlayQueueView {}
}

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use gtk::{Align, Box as GtkBox, Button, Label, Orientation};

use crate::device::state::DeviceState;

glib::wrapper! {
    pub struct PlayQueueView(ObjectSubclass<imp::PlayQueueView>)
        @extends adw::Bin, gtk::Widget;
}

impl PlayQueueView {
    /// Build the queue list bound to `ds`. The widget itself is the
    /// scrollable list (`gtk::ScrolledWindow` root) the owner packs
    /// directly. Starts **inactive** — the owner's first `set_active(true)`
    /// performs the initial render and acquires the queue watch.
    pub(crate) fn new(ds: &DeviceState) -> Self {
        let obj: Self = glib::Object::new();
        obj.build(ds);
        obj
    }

    fn build(&self, ds: &DeviceState) {
        let imp = self.imp();
        imp.ds.set(ds.clone()).unwrap();

        let outer = GtkBox::builder()
            .orientation(Orientation::Vertical).spacing(2)
            .margin_top(8).margin_bottom(4).margin_start(8).margin_end(8)
            .build();

        let header_row = GtkBox::builder()
            .orientation(Orientation::Horizontal)
            .margin_bottom(4)
            .build();
        header_row.append(
            &Label::builder()
                .label("PLAY QUEUE").css_classes(["section-label"])
                .halign(Align::Start).hexpand(true)
                .build(),
        );
        let count_label = Label::builder()
            .label("").css_classes(["queue-track-count", "dim-label"])
            .halign(Align::End).margin_end(8).margin_top(2)
            .build();
        header_row.append(&count_label);
        imp.count_label.set(count_label).unwrap();
        outer.append(&header_row);

        let rows_box = GtkBox::builder()
            .orientation(Orientation::Vertical).spacing(2)
            .build();
        outer.append(&rows_box);
        imp.rows_box.set(rows_box).unwrap();

        let scroll = gtk::ScrolledWindow::builder()
            .child(&outer)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        imp.scroll.set(scroll.clone()).unwrap();
        self.set_child(Some(&scroll));

        let id = ds.connect_queue_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                if !obj.imp().active.get() { return; }
                obj.refresh();
            }
        });
        imp.handlers.borrow_mut().push(id);

        // Covers both directions: connect (a re-shown view after the
        // device reconnected) and disconnect (clear the list — device_info()
        // is None then).
        let id = ds.connect_device_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                if !obj.imp().active.get() { return; }
                obj.refresh();
            }
        });
        imp.handlers.borrow_mut().push(id);
    }

    /// See the view lifecycle contract (`views/mod.rs`). Acquires/releases
    /// the `QueueWatchGuard` in step with activation — nothing polls the
    /// live queue while this view isn't the one asking for it. Always
    /// scrolls the current track into view on activation (unconditionally
    /// — see `imp::PlayQueueView::last_current_index`'s doc comment).
    pub(crate) fn set_active(&self, active: bool) {
        let imp = self.imp();
        let was = imp.active.replace(active);
        if active == was { return; }
        if active {
            let Some(ds) = imp.ds.get() else { return };
            *imp.watch.borrow_mut() = Some(ds.acquire_queue_watch());
            self.refresh();
            self.schedule_scroll_to_current();
        } else {
            imp.watch.borrow_mut().take();
        }
    }

    /// Render the queue from the `DeviceState` cache, patching existing
    /// rows in place rather than rebuilding — see this module's top doc
    /// comment. While offline (`device_info()` is `None`) or unsupported,
    /// the entry list is empty, which naturally trims every row away.
    /// Scrolls the current-track row into view if the current index
    /// actually changed since the last call (item 3 — item 2, "always
    /// scroll on tab open", is `set_active(true)`'s own job).
    fn refresh(&self) {
        let imp = self.imp();
        let Some(ds) = imp.ds.get() else { return };
        let rows_box = imp.rows_box.get().unwrap();
        let count_label = imp.count_label.get().unwrap();
        let mut rows = imp.rows.borrow_mut();

        let (entries, current_index) = if ds.device_info().is_some() && ds.play_queue_supported() {
            ds.queue()
        } else {
            (Vec::new(), None)
        };

        count_label.set_label(&if entries.is_empty() { String::new() } else { format!("{} tracks", entries.len()) });

        while rows.len() > entries.len() {
            if let Some(row) = rows.pop() {
                rows_box.remove(&row.btn);
            }
        }
        while rows.len() < entries.len() {
            let row = self.build_row();
            rows_box.append(&row.btn);
            rows.push(row);
        }

        for (row, entry) in rows.iter_mut().zip(entries.iter()) {
            if row.rendered.as_ref() != Some(entry) {
                row.index_cell.set(entry.index);
                row.title.set_label(&entry.title);
                row.subtitle.set_label(&subtitle_text(entry));
                row.btn.set_tooltip_text(Some(&entry.title));
                // Content changed — any art already shown belonged to the
                // old entry; clear to the fallback and let the block below
                // re-resolve `entry`'s own `art_uri` from scratch.
                row.pic.set_paintable(None::<&gtk::gdk::Paintable>);
                row.pic.set_icon_name(Some("audio-x-generic-symbolic"));
                row.art_applied_for = None;
                row.rendered = Some(entry.clone());
            }

            let is_current = Some(entry.index) == current_index;
            if row.is_current != is_current {
                if is_current { row.btn.add_css_class("current-track"); }
                else { row.btn.remove_css_class("current-track"); }
                row.is_current = is_current;
            }

            match &entry.art_uri {
                Some(uri) if row.art_applied_for.as_deref() != Some(uri.as_str()) => {
                    if let Some(bytes) = ds.queue_art_bytes(uri) {
                        // Resolved (successfully or not) — either way, stop
                        // re-checking this URI every refresh; a genuine
                        // change back to needing another fetch only happens
                        // via the content-changed branch above, which
                        // clears `art_applied_for` again.
                        row.art_applied_for = Some(uri.clone());
                        if !bytes.is_empty() {
                            let gbytes = glib::Bytes::from(&bytes);
                            if let Ok(tex) = gtk::gdk::Texture::from_bytes(&gbytes) {
                                row.pic.set_paintable(Some(&tex));
                            }
                        }
                    }
                    // `None` (fetch still in flight): leave the fallback
                    // showing, retry on the next refresh (`queue-changed`
                    // fires again once `process_queue_art_result()` lands).
                }
                _ => {}
            }
        }
        drop(rows);

        let index_changed = imp.last_current_index.replace(current_index) != current_index;
        if index_changed {
            self.schedule_scroll_to_current();
        }
    }

    fn build_row(&self) -> imp::Row {
        let pic = gtk::Image::builder()
            .pixel_size(40).icon_name("audio-x-generic-symbolic")
            .build();
        pic.add_css_class("preset-art");
        pic.set_overflow(gtk::Overflow::Hidden);

        let title = Label::builder()
            .label("").css_classes(["preset-name"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .halign(Align::Start).hexpand(true).width_chars(0)
            .build();
        let subtitle = Label::builder()
            .label("").css_classes(["dim-label", "caption"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .halign(Align::Start).hexpand(true).width_chars(0)
            .build();

        let text_col = GtkBox::builder().orientation(Orientation::Vertical).spacing(0).build();
        text_col.append(&title);
        text_col.append(&subtitle);

        let tile = GtkBox::builder()
            .orientation(Orientation::Horizontal).spacing(6)
            .css_classes(["preset-tile"]).overflow(gtk::Overflow::Hidden)
            .build();
        tile.append(&pic);
        tile.append(&text_col);

        let btn = Button::builder().child(&tile).css_classes(["flat", "preset-btn"]).build();

        let index_cell = Rc::new(Cell::new(0u32));
        btn.connect_clicked({
            let weak = self.downgrade();
            let index_cell = Rc::clone(&index_cell);
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                let Some(ds) = obj.imp().ds.get() else { return };
                ds.play_queue_track(index_cell.get());
            }
        });

        imp::Row {
            btn, pic, title, subtitle, index_cell,
            rendered: None, is_current: false, art_applied_for: None,
        }
    }

    /// Scrolls the current-track row into view, deferred one idle cycle
    /// (`glib::idle_add_local_once` — same pattern `geometry.rs` uses for
    /// "run after whatever GTK layout is already queued has drained")
    /// since this is often called right after the row was just
    /// created/made visible, before GTK has necessarily computed its
    /// allocation yet.
    fn schedule_scroll_to_current(&self) {
        let weak = self.downgrade();
        glib::idle_add_local_once(move || {
            let Some(obj) = weak.upgrade() else { return };
            obj.scroll_current_into_view();
        });
    }

    fn scroll_current_into_view(&self) {
        let imp = self.imp();
        let Some(scroll) = imp.scroll.get() else { return };
        let Some(content) = scroll.child() else { return };
        let rows = imp.rows.borrow();
        let Some(row) = rows.iter().find(|r| r.is_current) else { return };
        let Some(bounds) = row.btn.compute_bounds(&content) else { return };
        let vadj = scroll.vadjustment();
        vadj.clamp_page(bounds.y() as f64, (bounds.y() + bounds.height()) as f64);
    }
}

fn subtitle_text(entry: &crate::device::upnp::QueueTrackEntry) -> String {
    match (entry.artist.is_empty(), entry.album.is_empty()) {
        (false, false) => format!("{} \u{00b7} {}", entry.artist, entry.album),
        (false, true)  => entry.artist.clone(),
        (true, false)  => entry.album.clone(),
        (true, true)   => String::new(),
    }
}
