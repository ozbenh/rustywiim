//! `GraphicEqView` — the graphic-EQ band editor: one vertical gain slider
//! per band, in a horizontal row. See `ui/eq/mod.rs`'s doc comment for
//! why this is a pure data-in/data-out widget, not a `views/`-style
//! `DeviceState`-bound one.

pub mod imp {
    use std::cell::{Cell, OnceCell, RefCell};

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use glib::subclass::Signal;
    use gtk::glib;
    use std::sync::OnceLock;

    use crate::device::eq::GraphicBand;

    #[derive(Default)]
    pub struct GraphicEqView {
        /// One real `gtk::Grid` column per band (freq label row 0, slider
        /// row 1, gain-value label row 2) — guarantees the three actually
        /// line up (a `Grid` sizes each column to its widest cell across
        /// *all* rows) rather than three separate homogeneous `Box`es
        /// each sized independently, which drifted out of alignment
        /// whenever their natural widths differed (confirmed live).
        pub(super) band_grid: OnceCell<gtk::Grid>,
        /// Dim reference gridlines + shared axis labels, overlaid on top
        /// of `band_grid` — see `super::draw_geq_grid()`'s own doc
        /// comment for how it finds the slider row's exact extent
        /// (row 1 only, not the label rows above/below it) via
        /// `compute_bounds()` against a live slider each time it draws,
        /// rather than any fixed/guessed offset.
        pub(super) grid_area: OnceCell<gtk::DrawingArea>,
        pub(super) sliders:      RefCell<Vec<gtk::Scale>>,
        pub(super) value_labels: RefCell<Vec<gtk::Label>>,
        /// Current full band list, gain kept in sync with each slider's
        /// live value — the one source of truth `state()` reads from,
        /// rather than re-reading every slider at snapshot time.
        pub(super) bands:         RefCell<Vec<GraphicBand>>,
        pub(super) active_preset: RefCell<Option<String>>,
        /// Guards programmatic slider updates (`set_state()`'s in-place
        /// path) from re-emitting `band-changed` — same pattern
        /// `VolumeControl`/the old prototype both use.
        pub(super) updating: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for GraphicEqView {
        const NAME: &'static str = "GraphicEqView";
        type Type = super::GraphicEqView;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for GraphicEqView {
        fn constructed(&self) {
            self.parent_constructed();

            // Reserves `super::GEQ_GRID_LEFT_MARGIN` on the grid's own
            // left so `grid_area` (which matches this grid's full size,
            // margin included) has room to draw the shared axis labels
            // without overlapping the first band's column.
            let band_grid = gtk::Grid::builder()
                .row_spacing(8)
                .column_spacing(18)
                .margin_start(super::GEQ_GRID_LEFT_MARGIN as i32)
                .build();

            // Dim reference gridlines + one shared axis-label column,
            // drawn behind the sliders rather than per-slider
            // `add_mark()` text (which repeated the same 5 labels once
            // per band) — see `super::draw_geq_grid()`'s own doc comment.
            // `can_target(false)` so it never intercepts the sliders'
            // own drag/click handling.
            let grid_area = gtk::DrawingArea::new();
            grid_area.set_can_target(false);
            grid_area.set_hexpand(true);
            grid_area.set_vexpand(true);
            grid_area.set_halign(gtk::Align::Fill);
            grid_area.set_valign(gtk::Align::Fill);
            grid_area.set_draw_func({
                let weak = self.obj().downgrade();
                move |da, cr, w, h| {
                    let Some(this) = weak.upgrade() else { return };
                    let sliders = this.imp().sliders.borrow();
                    let Some(first) = sliders.first() else { return };
                    let Some(bounds) = first.compute_bounds(da) else { return };
                    super::draw_geq_grid(cr, w as f64, h as f64, bounds.y() as f64, bounds.height() as f64);
                }
            });

            let overlay = gtk::Overlay::new();
            overlay.set_child(Some(&band_grid));
            overlay.add_overlay(&grid_area);
            overlay.set_halign(gtk::Align::Center);
            overlay.set_margin_top(24);
            overlay.set_margin_bottom(24);
            overlay.set_margin_start(24);
            overlay.set_margin_end(32);

            self.obj().set_child(Some(&overlay));
            self.band_grid.set(band_grid).ok();
            self.grid_area.set(grid_area).ok();
        }

        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![Signal::builder("band-changed").param_types([u32::static_type()]).build()]
            })
        }
    }
    impl WidgetImpl for GraphicEqView {}
    impl BinImpl for GraphicEqView {}
}

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use gtk::{Align, Label, Orientation, Scale};

use crate::device::eq::{ChannelBands, EqState, GraphicBand};

glib::wrapper! {
    pub struct GraphicEqView(ObjectSubclass<imp::GraphicEqView>)
        @extends adw::Bin, gtk::Widget;
}

impl Default for GraphicEqView {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphicEqView {
    pub(crate) fn new() -> Self {
        glib::Object::new()
    }

    /// Rebuilds the band grid if the band count changed, else updates
    /// values in place. A non-`Graphic` state, or a `LeftRight` band
    /// shape (the host panel is responsible for flattening L/R to one
    /// channel before calling this — this widget only ever renders one
    /// channel), is a programmer error: `debug_assert!` and fall back
    /// sanely rather than panic in release builds.
    pub(crate) fn set_state(&self, state: &EqState) {
        let EqState::Graphic { bands, active_preset } = state else {
            debug_assert!(false, "GraphicEqView::set_state() handed a non-Graphic EqState");
            return;
        };
        let flat: Vec<GraphicBand> = match bands {
            ChannelBands::Stereo(v) => v.clone(),
            ChannelBands::LeftRight { left, .. } => {
                debug_assert!(false, "GraphicEqView never sees LeftRight directly — \
                    the host panel must flatten to one channel first");
                left.clone()
            }
        };

        let imp = self.imp();
        imp.updating.set(true);
        *imp.active_preset.borrow_mut() = active_preset.clone();

        let needs_rebuild = imp.bands.borrow().len() != flat.len();
        if needs_rebuild {
            let grid = imp.band_grid.get().expect("built in constructed()");
            while let Some(child) = grid.first_child() { grid.remove(&child); }

            let mut sliders = imp.sliders.borrow_mut();
            let mut value_labels = imp.value_labels.borrow_mut();
            sliders.clear();
            value_labels.clear();

            // Fixed, uniform *pixel* widths for both label rows — not
            // sized to each label's own current text — so a column's
            // width comes only from the slider's own constant size, never
            // from whatever text happens to be showing. Without this,
            // `gtk::Grid` sizes each column to its widest cell across all
            // three rows, and the gain-value label's width genuinely does
            // change with its text ("+0.0dB" vs "-12.0dB"), so switching
            // presets or dragging a slider visibly nudged that whole
            // column — confirmed live. Freq labels never change at
            // runtime, but are sized the same way for consistency and to
            // tolerate whatever this device's real band frequencies turn
            // out to need.
            //
            // Measured with a real `pango::Layout` against the exact
            // markup that will actually render (`measure_markup_px()`),
            // not `width_chars`/`max_width_chars` (tried first — those
            // measure against this label's *base* font size, not the
            // smaller size `small_markup()`'s `<span size="small">`
            // actually renders at, reserving visibly more room than the
            // small text ever needed — confirmed live) nor a hand-guessed
            // px-per-character constant (tried next — no character in
            // "+-.0123456789dB" renders at a uniform width in a
            // proportional font, so any single flat multiplier is either
            // too tight for some digits or too loose for others). Asking
            // Pango for the real rendered width of the specific widest
            // candidate strings is exact by construction and needs no
            // per-font/theme tuning.
            let value_px = ["+12.0dB", "-12.0dB"].iter()
                .map(|s| measure_markup_px(self, &small_markup(s)))
                .max().unwrap_or(48);
            let freq_px = flat.iter()
                .map(|b| measure_markup_px(self, &small_markup(&format!("{}Hz", b.freq_label))))
                .max().unwrap_or(40);

            for (i, band) in flat.iter().enumerate() {
                let col = i as i32;

                let freq_label = Label::builder()
                    .use_markup(true)
                    .label(small_markup(&format!("{}Hz", band.freq_label)))
                    .css_classes(["geq-label", "dim-label"])
                    .halign(Align::Center)
                    .width_request(freq_px)
                    .build();
                grid.attach(&freq_label, col, 0, 1, 1);

                let slider = Scale::with_range(Orientation::Vertical, -12.0, 12.0, 0.5);
                slider.set_inverted(true);
                slider.set_vexpand(true);
                slider.set_halign(Align::Center);
                slider.set_size_request(-1, 220);
                slider.set_draw_value(false);
                slider.set_value(band.gain_db);
                slider.add_css_class("geq-band");
                grid.attach(&slider, col, 1, 1, 1);

                let value_label = Label::builder()
                    .use_markup(true)
                    .label(small_markup(&format_gain(band.gain_db)))
                    .css_classes(["geq-value", "dim-label"])
                    .halign(Align::Center)
                    .width_request(value_px)
                    .build();
                grid.attach(&value_label, col, 2, 1, 1);

                let idx = i as u32;
                slider.connect_value_changed({
                    let weak = self.downgrade();
                    move |s| {
                        let Some(this) = weak.upgrade() else { return };
                        let imp = this.imp();
                        if imp.updating.get() { return; }
                        if let Some(band) = imp.bands.borrow_mut().get_mut(idx as usize) {
                            band.gain_db = s.value();
                        }
                        if let Some(lbl) = imp.value_labels.borrow().get(idx as usize) {
                            lbl.set_markup(&small_markup(&format_gain(s.value())));
                        }
                        this.emit_by_name::<()>("band-changed", &[&idx]);
                    }
                });

                sliders.push(slider);
                value_labels.push(value_label);
            }
        } else {
            let value_labels = imp.value_labels.borrow();
            for ((slider, band), label) in imp.sliders.borrow().iter().zip(flat.iter()).zip(value_labels.iter()) {
                slider.set_value(band.gain_db);
                label.set_markup(&small_markup(&format_gain(band.gain_db)));
            }
        }

        *imp.bands.borrow_mut() = flat;
        imp.updating.set(false);
        // The slider row's exact bounds (`draw_geq_grid()`'s reference
        // point) only change once new/removed sliders are actually
        // allocated their size, which hasn't happened yet at this exact
        // point in a rebuild — queue a redraw so the grid picks up the
        // new bounds once layout settles, rather than possibly drawing
        // against the previous band count's now-stale slider.
        if let Some(area) = imp.grid_area.get() { area.queue_draw(); }
    }

    /// Snapshot assembled from current widget state (band gains kept live
    /// in `imp.bands` as sliders move — see `set_state()`'s doc comment).
    pub(crate) fn state(&self) -> EqState {
        let imp = self.imp();
        EqState::Graphic {
            bands: ChannelBands::Stereo(imp.bands.borrow().clone()),
            active_preset: imp.active_preset.borrow().clone(),
        }
    }

    pub(crate) fn connect_band_changed<F: Fn(&Self, u32) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("band-changed", false, move |args| {
            let this = args[0].get::<Self>().unwrap();
            let idx  = args[1].get::<u32>().unwrap();
            f(&this, idx);
            None
        })
    }
}

/// "+3.5dB"/"-2.0dB"/"+0.0dB" — always signed (including zero) since this
/// is a gain readout, not a bare number.
fn format_gain(db: f64) -> String {
    format!("{db:+.1}dB")
}

/// Pango markup wrapper for the frequency/gain-value labels — both read
/// too large at the default body text size (confirmed live) next to the
/// slider they annotate.
fn small_markup(text: &str) -> String {
    format!("<span size=\"small\">{}</span>", glib::markup_escape_text(text))
}

/// The exact rendered width (px) of `markup` in `widget`'s own font — a
/// real `pango::Layout` measurement, not a guessed per-character estimate.
/// `create_pango_layout()` builds the layout against this widget's actual
/// `PangoContext` (font, hinting, the works), so this reflects precisely
/// what will render, including whatever size `markup`'s own `<span>` asks
/// for — used to pin `GraphicEqView`'s label columns to their widest
/// possible content instead of resizing with whatever text is current.
fn measure_markup_px(widget: &impl IsA<gtk::Widget>, markup: &str) -> i32 {
    let layout = widget.create_pango_layout(None::<&str>);
    layout.set_markup(markup);
    layout.pixel_size().0
}

/// Reserved on the left of `band_grid` (see `imp::GraphicEqView`'s own
/// doc comment) for `draw_geq_grid()`'s shared axis labels, mirroring
/// `parametric.rs`'s `LEFT_MARGIN`.
const GEQ_GRID_LEFT_MARGIN: f64 = 44.0;

/// The dim reference lines at 0/±6/±12dB, spanning the full width behind
/// every slider, plus one shared "0dB"/"+6dB"/... label column at the
/// left — replaces what used to be a `gtk::Scale::add_mark()` text label
/// repeated on every single slider (five labels × N bands).
///
/// `slider_top`/`slider_h` are the *slider row's own* bounds (row 1 of
/// `band_grid`) relative to this `DrawingArea`, supplied by the caller via
/// `Widget::compute_bounds()` against a live slider each time this draws —
/// not a guessed/fixed offset, so the lines land exactly against the
/// sliders regardless of how tall the frequency/gain-value label rows
/// above/below them happen to render, and stay correct across any resize.
/// Not pixel-exact to where `GtkScale` centers its own handle within that
/// row (a small, constant rendering detail of the real widget this plain
/// Cairo drawing doesn't otherwise know about) — close enough for a dim
/// reference grid, same spirit as `parametric.rs`'s own hand-drawn curve
/// not chasing pixel-perfect alignment either.
fn draw_geq_grid(cr: &gtk::cairo::Context, w: f64, h: f64, slider_top: f64, slider_h: f64) {
    let _ = h;
    let (gr, gg, gb, ga) = super::GRID_RGBA;
    let (lr, lg, lb, la) = super::LABEL_RGBA;
    cr.set_font_size(10.0);
    cr.set_line_width(0.5);
    for db in [12.0, 6.0, 0.0, -6.0, -12.0] {
        let y = slider_top + (12.0 - db) / 24.0 * slider_h;

        cr.set_source_rgba(gr, gg, gb, ga);
        cr.move_to(GEQ_GRID_LEFT_MARGIN, y);
        cr.line_to(w, y);
        let _ = cr.stroke();

        let text = if db == 0.0 { "0dB".to_string() } else { format!("{db:+.0}dB") };
        cr.set_source_rgba(lr, lg, lb, la);
        if let Ok(extents) = cr.text_extents(&text) {
            cr.move_to(GEQ_GRID_LEFT_MARGIN - 6.0 - extents.width(), y + extents.height() / 2.0);
            let _ = cr.show_text(&text);
        }
    }
}
