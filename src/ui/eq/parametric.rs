//! `ParametricEqView` — the parametric-EQ band editor: a frequency-
//! response curve plus one row (mode/freq/Q/gain) per band. See
//! `ui/eq/mod.rs`'s doc comment for why this is a pure data-in/data-out
//! widget, not a `views/`-style `DeviceState`-bound one.
//!
//! The curve math is a cleaned-up port of
//! `~/hackplace/rustywiim-old/src/eq.rs`'s `band_response()`/
//! `draw_peq_curve()`, with one real fix (not just cleanup): the old code
//! only implemented `LowShelf`/`Peak`/`HighShelf`, silently rendering
//! `LowPass`/`HighPass` bands (confirmed real, live, on a WiiM Ultra) as
//! a flat line. Both are added below using the standard RBJ
//! Audio-EQ-Cookbook biquad forms, which — unlike the other three modes
//! — don't depend on gain at all; the old code's near-zero-gain
//! early-return is accordingly only applied to the gain-driven modes.

use std::f64::consts::PI;

pub mod imp {
    use std::cell::{Cell, OnceCell, RefCell};

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use glib::subclass::Signal;
    use gtk::glib;
    use std::sync::OnceLock;

    use crate::device::eq::{ParametricBand, PeqBandMode};

    #[derive(Default)]
    pub struct ParametricEqView {
        pub(super) curve:       OnceCell<gtk::DrawingArea>,
        pub(super) band_list:   OnceCell<gtk::Grid>,
        /// One `SizeGroup` per column (badge/mode/freq/Q/gain), each
        /// containing that column's header cell plus every row's widget
        /// currently in that column — see `build_header_row()`'s doc
        /// comment for why this is needed at all.
        pub(super) col_groups:  OnceCell<[gtk::SizeGroup; 5]>,
        pub(super) mode_dds:    RefCell<Vec<gtk::DropDown>>,
        pub(super) freq_spins:  RefCell<Vec<gtk::SpinButton>>,
        pub(super) q_spins:     RefCell<Vec<gtk::SpinButton>>,
        pub(super) gain_scales: RefCell<Vec<gtk::Scale>>,
        pub(super) bands:         RefCell<Vec<ParametricBand>>,
        pub(super) active_preset: RefCell<Option<String>>,
        /// Which filter modes this device/mechanism actually offers —
        /// set via `set_filters()` before the first `set_state()`; drives
        /// the mode dropdown's option list. Never a hardcoded fixed set.
        pub(super) filters: RefCell<Vec<PeqBandMode>>,
        pub(super) updating: Cell<bool>,
        pub(super) drag_band:       RefCell<Option<usize>>,
        pub(super) drag_start_freq: Cell<f64>,
        pub(super) drag_start_gain: Cell<f64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ParametricEqView {
        const NAME: &'static str = "ParametricEqView";
        type Type = super::ParametricEqView;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for ParametricEqView {
        fn constructed(&self) {
            self.parent_constructed();
            let outer = gtk::Box::new(gtk::Orientation::Vertical, 8);
            outer.set_margin_top(16);
            outer.set_margin_bottom(16);
            outer.set_margin_start(16);
            outer.set_margin_end(16);

            let curve = gtk::DrawingArea::new();
            // 160 -> 184 (+15%, Ben's ask, all themes) — more room to see
            // and drag band handles precisely.
            curve.set_size_request(-1, 184);
            curve.add_css_class("peq-curve");
            // Extra horizontal inset on top of `outer`'s own 16px side
            // margins, specifically for this widget — themes that give
            // ".peq-curve" a real bevel/border (RustyWiiM Wood; every
            // other theme leaves it unstyled, so this is a few harmless
            // extra px of blank margin there) need more breathing room
            // between that box's own edge and the window edge than a
            // plain unboxed DrawingArea did, so the box itself — and the
            // dB/frequency axis labels drawn inside it, which already
            // reserve their own separate inset via LEFT_MARGIN/
            // BOTTOM_MARGIN below — don't read as flush against the
            // window edge.
            curve.set_margin_start(12);
            curve.set_margin_end(12);
            outer.append(&curve);
            self.curve.set(curve).ok();

            let (header, col_groups) = super::build_header_row();
            outer.append(&header);
            self.col_groups.set(col_groups).ok();

            let band_list = gtk::Grid::builder().row_spacing(4).column_spacing(8).build();
            let scroll = gtk::ScrolledWindow::builder()
                .child(&band_list)
                .vexpand(true)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .build();
            outer.append(&scroll);
            self.band_list.set(band_list).ok();

            self.obj().set_child(Some(&outer));

            // Curve drag interaction + draw func: set up exactly once here,
            // not in `rebuild_rows()` (which can run repeatedly as the band
            // count changes) — see this module's own history: registering
            // these on every rebuild was silently piling up duplicate
            // `GestureDrag` controllers.
            super::wire_curve(&self.obj());
        }

        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![Signal::builder("band-changed").param_types([u32::static_type()]).build()]
            })
        }
    }
    impl WidgetImpl for ParametricEqView {}
    impl BinImpl for ParametricEqView {}
}

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use gtk::{Align, DropDown, Label, Orientation, Scale, SpinButton, StringList};

use crate::device::eq::{ChannelBands, EqState, ParametricBand, PeqBandMode};

glib::wrapper! {
    pub struct ParametricEqView(ObjectSubclass<imp::ParametricEqView>)
        @extends adw::Bin, gtk::Widget;
}

impl Default for ParametricEqView {
    fn default() -> Self {
        Self::new()
    }
}

const BAND_COLORS: [(f64, f64, f64); 10] = [
    (0.31, 0.80, 0.77), (0.90, 0.35, 0.35), (0.35, 0.85, 0.35),
    (0.90, 0.85, 0.30), (0.40, 0.40, 0.90), (0.85, 0.40, 0.85),
    (0.55, 0.55, 0.90), (0.90, 0.60, 0.30), (0.30, 0.85, 0.85),
    (0.80, 0.80, 0.80),
];

fn mode_label(mode: &PeqBandMode) -> String {
    match mode {
        PeqBandMode::Off => "Off".to_string(),
        PeqBandMode::LowShelf => "Low Shelf".to_string(),
        PeqBandMode::Peak => "Peak".to_string(),
        PeqBandMode::HighShelf => "High Shelf".to_string(),
        PeqBandMode::LowPass => "Low Pass".to_string(),
        PeqBandMode::HighPass => "High Pass".to_string(),
        PeqBandMode::Other(s) => format!("Mode {s}"),
    }
}

/// Column header row above the `gtk::Grid` of band rows. A *separate*
/// `gtk::Grid` from `band_list` (rows are rebuilt on every band-count
/// change, the header never is) — each `GtkGrid` sizes its own columns
/// independently from its own content, so without linking them explicitly
/// the header's columns (sized off short header text) drifted out of
/// alignment with the band rows' columns (sized off the actual widgets:
/// a `DropDown`, `SpinButton`s, a `Scale`) — confirmed live. Returns one
/// `SizeGroup` per column alongside the header itself so `rebuild_rows()`
/// can add each new row widget to the matching group, keeping every
/// column's width shared across both grids.
fn build_header_row() -> (gtk::Grid, [gtk::SizeGroup; 5]) {
    let header = gtk::Grid::builder().column_spacing(8).margin_bottom(2).build();
    let cell = |text: &str| Label::builder().label(text).css_classes(["dim-label", "caption"]).halign(gtk::Align::Start).build();
    let groups: [gtk::SizeGroup; 5] = std::array::from_fn(|_| gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal));
    for (i, text) in ["", "Mode", "Freq (Hz)", "Q", "Gain (dB)"].into_iter().enumerate() {
        let widget = cell(text);
        header.attach(&widget, i as i32, 0, 1, 1);
        groups[i].add_widget(&widget);
    }
    (header, groups)
}

/// Small filled-circle badge matching the curve's per-band dot color,
/// with the band's letter drawn inside — same colors as `BAND_COLORS`,
/// so a band's row and its curve trace/handle are visually tied together.
fn band_badge(color: (f64, f64, f64), letter: char) -> gtk::DrawingArea {
    let badge = gtk::DrawingArea::new();
    badge.set_content_width(22);
    badge.set_content_height(22);
    badge.set_valign(Align::Center);
    let (r, g, b) = color;
    badge.set_draw_func(move |_da, cr, w, h| {
        let (cx, cy, radius) = (w as f64 / 2.0, h as f64 / 2.0, (w.min(h) as f64) / 2.0 - 1.0);
        cr.arc(cx, cy, radius, 0.0, 2.0 * PI);
        cr.set_source_rgb(r, g, b);
        let _ = cr.fill();

        // Light or dark letter, whichever contrasts more with this band's color.
        let luminance = 0.299 * r + 0.587 * g + 0.114 * b;
        if luminance > 0.6 { cr.set_source_rgb(0.1, 0.1, 0.1); } else { cr.set_source_rgb(0.95, 0.95, 0.95); }
        cr.set_font_size(12.0);
        let text = letter.to_string();
        if let Ok(extents) = cr.text_extents(&text) {
            cr.move_to(cx - extents.width() / 2.0 - extents.x_bearing(), cy - extents.height() / 2.0 - extents.y_bearing());
            let _ = cr.show_text(&text);
        }
    });
    badge
}

/// The curve's frequency axis spans exactly this range — matches
/// `PEQ_RANGE`'s own `freqMin`/`freqMax` (20Hz/20000Hz), the real range a
/// band's own frequency is clamped to elsewhere in this codebase. Used
/// to be `22000.0` for the upper bound, an arbitrary bit of padding left
/// over from the original prototype port — confirmed live (a real
/// screenshot compared against the official app) that this made the
/// plotted curve/grid extend visibly past the "20k" label instead of
/// ending exactly at it.
const FREQ_MIN: f64 = 20.0;
const FREQ_MAX: f64 = 20000.0;

/// Reserved space (widget pixels) for axis labels, *outside* the actual
/// plot area — previously there was no reserved margin at all, so the
/// dB labels (drawn at a fixed `x = 2.0`) sat on top of the curve/grid
/// rather than beside it, and the curve itself ran flush to every edge
/// of the widget with no breathing room. `draw_peq_curve()` computes a
/// `plot_w`/`plot_h` inset by these before doing any actual plotting.
const LEFT_MARGIN: f64 = 34.0;
const BOTTOM_MARGIN: f64 = 18.0;
/// Extra padding *inside* the plot area, above the `+12` line and below
/// `-12` — so the highest/lowest gridlines don't touch the plot's own
/// top/bottom edge, matching the official app's own breathing room.
const VERTICAL_PAD: f64 = 6.0;

fn db_to_y(db: f64, plot_h: f64) -> f64 {
    let usable = (plot_h - 2.0 * VERTICAL_PAD).max(1.0);
    VERTICAL_PAD + usable / 2.0 - (db / 12.0) * (usable / 2.0)
}

fn freq_to_x(freq: f64, plot_w: f64) -> f64 {
    let (lo, hi) = (FREQ_MIN.ln(), FREQ_MAX.ln());
    ((freq.clamp(FREQ_MIN, FREQ_MAX).ln() - lo) / (hi - lo)) * plot_w
}

fn x_to_freq(x: f64, plot_w: f64) -> f64 {
    let (lo, hi) = (FREQ_MIN.ln(), FREQ_MAX.ln());
    (lo + (x.clamp(0.0, plot_w) / plot_w) * (hi - lo)).exp()
}

/// One band's contribution (dB) at `eval_freq`, given its own
/// `freq_hz`/`q`/`gain_db`. See this module's top doc comment for the
/// `LowPass`/`HighPass` fix.
fn band_response(mode: &PeqBandMode, freq_hz: f64, q: f64, gain_db: f64, eval_freq: f64) -> f64 {
    let gain_independent = matches!(mode, PeqBandMode::LowPass | PeqBandMode::HighPass);
    if matches!(mode, PeqBandMode::Off | PeqBandMode::Other(_)) {
        return 0.0;
    }
    if !gain_independent && gain_db.abs() < 0.001 {
        return 0.0;
    }

    let w0 = 2.0 * PI * freq_hz / 48000.0;
    let w  = 2.0 * PI * eval_freq / 48000.0;
    let a  = 10.0f64.powf(gain_db / 40.0);
    let alpha = w0.sin() / (2.0 * q.max(0.01));

    let (b0, b1, b2, a0, a1, a2) = match mode {
        PeqBandMode::Peak => (
            1.0 + alpha * a, -2.0 * w0.cos(), 1.0 - alpha * a,
            1.0 + alpha / a, -2.0 * w0.cos(), 1.0 - alpha / a,
        ),
        PeqBandMode::LowShelf => {
            let sq = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) - (a - 1.0) * w0.cos() + sq),
                2.0 * a * ((a - 1.0) - (a + 1.0) * w0.cos()),
                a * ((a + 1.0) - (a - 1.0) * w0.cos() - sq),
                (a + 1.0) + (a - 1.0) * w0.cos() + sq,
                -2.0 * ((a - 1.0) + (a + 1.0) * w0.cos()),
                (a + 1.0) + (a - 1.0) * w0.cos() - sq,
            )
        }
        PeqBandMode::HighShelf => {
            let sq = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) + (a - 1.0) * w0.cos() + sq),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * w0.cos()),
                a * ((a + 1.0) + (a - 1.0) * w0.cos() - sq),
                (a + 1.0) - (a - 1.0) * w0.cos() + sq,
                2.0 * ((a - 1.0) - (a + 1.0) * w0.cos()),
                (a + 1.0) - (a - 1.0) * w0.cos() - sq,
            )
        }
        // RBJ cookbook low-pass: no `a`/gain term at all — a pure cutoff
        // filter, not a boost/cut. Real Ultra captures show `gain: 0.0`
        // on LowPass/HighPass bands as a matter of course; it's simply
        // not a meaningful field for these two modes.
        PeqBandMode::LowPass => (
            (1.0 - w0.cos()) / 2.0, 1.0 - w0.cos(), (1.0 - w0.cos()) / 2.0,
            1.0 + alpha,            -2.0 * w0.cos(), 1.0 - alpha,
        ),
        PeqBandMode::HighPass => (
            (1.0 + w0.cos()) / 2.0, -(1.0 + w0.cos()), (1.0 + w0.cos()) / 2.0,
            1.0 + alpha,             -2.0 * w0.cos(),   1.0 - alpha,
        ),
        PeqBandMode::Off | PeqBandMode::Other(_) => return 0.0,
    };

    let cw = w.cos();
    let c2w = (2.0 * w).cos();
    let num = b0 * b0 + b1 * b1 + b2 * b2 + 2.0 * (b0 * b1 + b1 * b2) * cw + 2.0 * b0 * b2 * c2w;
    let den = a0 * a0 + a1 * a1 + a2 * a2 + 2.0 * (a0 * a1 + a1 * a2) * cw + 2.0 * a0 * a2 * c2w;
    if den <= 0.0 { 0.0 } else { 10.0 * (num / den).log10() }
}

use super::{GRID_RGBA, LABEL_RGBA};

fn draw_peq_curve(cr: &gtk::cairo::Context, w: f64, h: f64, bands: &[ParametricBand]) {
    // No painted background at all — this used to be a hardcoded
    // near-black rectangle (fine for the old prototype's own fixed dark
    // theme, out of place against this app's actual theme system, which
    // this widget doesn't otherwise participate in). Leaving the Cairo
    // surface untouched lets the widget's real CSS background (whatever
    // `.peq-curve` resolves to under the active theme) show through.
    let plot_w = (w - LEFT_MARGIN).max(1.0);
    let plot_h = (h - BOTTOM_MARGIN).max(1.0);
    let (lr, lg, lb, la) = LABEL_RGBA;

    // dB axis labels — drawn in the reserved left margin, in absolute
    // widget coordinates (not translated below), so they never overlap
    // the plot area itself.
    cr.set_source_rgba(lr, lg, lb, la);
    cr.set_font_size(10.0);
    for &db in &[-12, -6, 0, 6, 12] {
        cr.move_to(2.0, db_to_y(db as f64, plot_h) + 3.0);
        let _ = cr.show_text(&format!("{db:+}"));
    }

    // Everything else is drawn relative to the plot area's own origin —
    // translating once here means the rest of this function can treat
    // (0,0)..(plot_w,plot_h) as if it were the whole surface.
    let _ = cr.save();
    cr.translate(LEFT_MARGIN, 0.0);

    let (gr, gg, gb, ga) = GRID_RGBA;
    cr.set_source_rgba(gr, gg, gb, ga * 0.4);
    cr.set_line_width(0.5);
    for db in [-12.0, -6.0, 0.0, 6.0, 12.0] {
        let y = db_to_y(db, plot_h);
        cr.move_to(0.0, y);
        cr.line_to(plot_w, y);
        let _ = cr.stroke();
    }
    // Dense per-decade gridlines (1x..9x each of 10/100/1000/10000 Hz,
    // clipped to FREQ_MIN..FREQ_MAX) — matches the official app's own
    // grid density, not just the handful of labeled frequencies.
    for decade in [10.0, 100.0, 1000.0, 10000.0] {
        for mult in 1..10 {
            let f = decade * mult as f64;
            if f < FREQ_MIN || f > FREQ_MAX { continue; }
            let x = freq_to_x(f, plot_w);
            cr.move_to(x, 0.0);
            cr.line_to(x, plot_h);
            let _ = cr.stroke();
        }
    }

    // Frequency axis labels along the bottom, in the reserved bottom
    // margin (translated coordinates, so `plot_h` here lands inside it).
    cr.set_source_rgba(lr, lg, lb, la);
    cr.set_font_size(10.0);
    for &(f, label) in &[
        (20.0, "20"), (50.0, "50"), (100.0, "100"), (200.0, "200"), (500.0, "500"),
        (1000.0, "1k"), (2000.0, "2k"), (5000.0, "5k"), (10000.0, "10k"), (20000.0, "20k"),
    ] {
        let x = freq_to_x(f, plot_w);
        cr.move_to((x - 10.0).max(0.0).min(plot_w - 18.0), plot_h + 13.0);
        let _ = cr.show_text(label);
    }

    let n = plot_w as usize;
    let mut response = vec![0.0f64; n];
    for band in bands {
        if matches!(band.mode, PeqBandMode::Off) { continue; }
        for (i, r) in response.iter_mut().enumerate() {
            *r += band_response(&band.mode, band.freq_hz, band.q, band.gain_db, x_to_freq(i as f64, plot_w));
        }
    }

    for (bi, band) in bands.iter().enumerate() {
        if matches!(band.mode, PeqBandMode::Off) { continue; }
        let (r, g, b) = BAND_COLORS[bi % BAND_COLORS.len()];
        let zero_y = db_to_y(0.0, plot_h);

        cr.set_source_rgba(r, g, b, 0.12);
        cr.move_to(0.0, zero_y);
        for i in 0..n {
            cr.line_to(i as f64, db_to_y(band_response(&band.mode, band.freq_hz, band.q, band.gain_db, x_to_freq(i as f64, plot_w)), plot_h));
        }
        cr.line_to(plot_w, zero_y);
        cr.close_path();
        let _ = cr.fill();

        let hx = freq_to_x(band.freq_hz, plot_w);
        let hy = db_to_y(band.gain_db, plot_h);
        cr.set_source_rgba(r, g, b, 0.9);
        cr.arc(hx, hy, 6.0, 0.0, 2.0 * PI);
        let _ = cr.fill();
    }

    cr.set_source_rgba(0.31, 0.80, 0.77, 1.0);
    cr.set_line_width(2.0);
    for (i, &db) in response.iter().enumerate() {
        let y = db_to_y(db, plot_h);
        if i == 0 { cr.move_to(0.0, y); } else { cr.line_to(i as f64, y); }
    }
    let _ = cr.stroke();

    let _ = cr.restore();
}

impl ParametricEqView {
    pub(crate) fn new() -> Self {
        glib::Object::new()
    }

    /// Must be called before the first `set_state()` — drives the mode
    /// dropdown's option list. Never a hardcoded fixed set: the filter
    /// vocabulary is device-reported and varies by model/firmware.
    pub(crate) fn set_filters(&self, filters: &[PeqBandMode]) {
        *self.imp().filters.borrow_mut() = filters.to_vec();
    }

    fn mode_index(&self, mode: &PeqBandMode) -> u32 {
        self.imp().filters.borrow().iter().position(|m| m == mode).unwrap_or(0) as u32
    }

    pub(crate) fn set_state(&self, state: &EqState) {
        let EqState::Parametric { bands, active_preset } = state else {
            debug_assert!(false, "ParametricEqView::set_state() handed a non-Parametric EqState");
            return;
        };
        let flat: Vec<ParametricBand> = match bands {
            ChannelBands::Stereo(v) => v.clone(),
            ChannelBands::LeftRight { left, .. } => {
                debug_assert!(false, "ParametricEqView never sees LeftRight directly — \
                    the host panel must flatten to one channel first");
                left.clone()
            }
        };

        let imp = self.imp();
        imp.updating.set(true);
        *imp.active_preset.borrow_mut() = active_preset.clone();

        let needs_rebuild = imp.bands.borrow().len() != flat.len()
            || imp.mode_dds.borrow().is_empty() && !flat.is_empty();
        if needs_rebuild {
            self.rebuild_rows(&flat);
        } else {
            let mode_dds    = imp.mode_dds.borrow();
            let freq_spins  = imp.freq_spins.borrow();
            let q_spins     = imp.q_spins.borrow();
            let gain_scales = imp.gain_scales.borrow();
            for (i, band) in flat.iter().enumerate() {
                mode_dds[i].set_selected(self.mode_index(&band.mode));
                freq_spins[i].set_value(band.freq_hz);
                q_spins[i].set_value(band.q);
                gain_scales[i].set_value(band.gain_db);
            }
        }

        *imp.bands.borrow_mut() = flat;
        imp.updating.set(false);
        if let Some(curve) = imp.curve.get() { curve.queue_draw(); }
    }

    pub(crate) fn state(&self) -> EqState {
        let imp = self.imp();
        EqState::Parametric {
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

    /// One row per band, laid out as `gtk::Grid` columns (badge/mode/
    /// freq/Q/gain) so every row's widgets line up under the header row
    /// built in `constructed()` — replaces the old per-row `GtkBox`,
    /// whose column widths tracked each row's own widest content instead
    /// of a shared width across rows.
    fn rebuild_rows(&self, bands: &[ParametricBand]) {
        let imp = self.imp();
        let band_list = imp.band_list.get().expect("built in constructed()");
        while let Some(child) = band_list.first_child() {
            band_list.remove(&child);
        }
        let mut mode_dds = imp.mode_dds.borrow_mut();
        let mut freq_spins = imp.freq_spins.borrow_mut();
        let mut q_spins = imp.q_spins.borrow_mut();
        let mut gain_scales = imp.gain_scales.borrow_mut();
        mode_dds.clear();
        freq_spins.clear();
        q_spins.clear();
        gain_scales.clear();

        let filter_labels: Vec<String> = imp.filters.borrow().iter().map(mode_label).collect();
        let label_refs: Vec<&str> = filter_labels.iter().map(String::as_str).collect();

        let col_groups = imp.col_groups.get().expect("built in constructed()");
        for (i, band) in bands.iter().enumerate() {
            let row = i as i32;
            let letter = (b'a' + (i as u8 % 26)) as char;
            let (r, g, b) = BAND_COLORS[i % BAND_COLORS.len()];
            let badge = band_badge((r, g, b), letter);
            band_list.attach(&badge, 0, row, 1, 1);
            col_groups[0].add_widget(&badge);

            let mode_dd = DropDown::new(Some(StringList::new(&label_refs)), gtk::Expression::NONE);
            mode_dd.set_selected(self.mode_index(&band.mode));
            band_list.attach(&mode_dd, 1, row, 1, 1);
            col_groups[1].add_widget(&mode_dd);

            let freq_spin = SpinButton::with_range(20.0, 20000.0, 1.0);
            freq_spin.set_value(band.freq_hz);
            freq_spin.set_width_chars(6);
            band_list.attach(&freq_spin, 2, row, 1, 1);
            col_groups[2].add_widget(&freq_spin);

            let q_spin = SpinButton::with_range(0.1, 24.0, 0.01);
            q_spin.set_digits(2);
            q_spin.set_value(band.q);
            q_spin.set_width_chars(5);
            band_list.attach(&q_spin, 3, row, 1, 1);
            col_groups[3].add_widget(&q_spin);

            let gain = Scale::with_range(Orientation::Horizontal, -12.0, 12.0, 0.5);
            gain.set_draw_value(true);
            gain.set_value_pos(gtk::PositionType::Right);
            gain.set_hexpand(true);
            gain.set_size_request(160, -1);
            gain.set_value(band.gain_db);
            band_list.attach(&gain, 4, row, 1, 1);
            col_groups[4].add_widget(&gain);

            let idx = i as u32;

            mode_dd.connect_selected_notify({
                let weak = self.downgrade();
                move |dd| {
                    let Some(this) = weak.upgrade() else { return };
                    let imp = this.imp();
                    if imp.updating.get() { return; }
                    let sel = dd.selected() as usize;
                    if let Some(mode) = imp.filters.borrow().get(sel).cloned() {
                        if let Some(band) = imp.bands.borrow_mut().get_mut(idx as usize) {
                            band.mode = mode;
                        }
                    }
                    if let Some(curve) = imp.curve.get() { curve.queue_draw(); }
                    this.emit_by_name::<()>("band-changed", &[&idx]);
                }
            });
            freq_spin.connect_value_changed({
                let weak = self.downgrade();
                move |s| {
                    let Some(this) = weak.upgrade() else { return };
                    let imp = this.imp();
                    if imp.updating.get() { return; }
                    if let Some(band) = imp.bands.borrow_mut().get_mut(idx as usize) { band.freq_hz = s.value(); }
                    if let Some(curve) = imp.curve.get() { curve.queue_draw(); }
                    this.emit_by_name::<()>("band-changed", &[&idx]);
                }
            });
            q_spin.connect_value_changed({
                let weak = self.downgrade();
                move |s| {
                    let Some(this) = weak.upgrade() else { return };
                    let imp = this.imp();
                    if imp.updating.get() { return; }
                    if let Some(band) = imp.bands.borrow_mut().get_mut(idx as usize) { band.q = s.value(); }
                    if let Some(curve) = imp.curve.get() { curve.queue_draw(); }
                    this.emit_by_name::<()>("band-changed", &[&idx]);
                }
            });
            gain.connect_value_changed({
                let weak = self.downgrade();
                move |s| {
                    let Some(this) = weak.upgrade() else { return };
                    let imp = this.imp();
                    if imp.updating.get() { return; }
                    if let Some(band) = imp.bands.borrow_mut().get_mut(idx as usize) { band.gain_db = s.value(); }
                    if let Some(curve) = imp.curve.get() { curve.queue_draw(); }
                    this.emit_by_name::<()>("band-changed", &[&idx]);
                }
            });

            mode_dds.push(mode_dd);
            freq_spins.push(freq_spin);
            q_spins.push(q_spin);
            gain_scales.push(gain);
        }
    }
}

/// Curve drag interaction + draw func — wired exactly once, from
/// `constructed()`, not from `rebuild_rows()` (which runs again on every
/// band-count change): registering a fresh `GestureDrag`/`set_draw_func`
/// on each rebuild silently piled up duplicate drag controllers.
fn wire_curve(view: &ParametricEqView) {
    let curve = view.imp().curve.get().expect("built in constructed()").clone();
    let drag = gtk::GestureDrag::new();
    {
        let weak = view.downgrade();
        let curve_for_begin = curve.clone();
        drag.connect_drag_begin(move |_, x, y| {
            let Some(this) = weak.upgrade() else { return };
            let imp = this.imp();
            // `x`/`y` are absolute widget coordinates; band handle
            // positions are computed in *plot*-relative coordinates (see
            // `draw_peq_curve()`'s own `LEFT_MARGIN` translate), so the
            // handle's own x needs the same offset added back before
            // comparing against the raw press position.
            let plot_w = (curve_for_begin.width() as f64 - LEFT_MARGIN).max(1.0);
            let plot_h = (curve_for_begin.height() as f64 - BOTTOM_MARGIN).max(1.0);
            let bands = imp.bands.borrow();
            let mut best: Option<(usize, f64)> = None;
            for (i, band) in bands.iter().enumerate() {
                if matches!(band.mode, PeqBandMode::Off) { continue; }
                let hx = freq_to_x(band.freq_hz, plot_w) + LEFT_MARGIN;
                let hy = db_to_y(band.gain_db, plot_h);
                let dist = ((x - hx).powi(2) + (y - hy).powi(2)).sqrt();
                if dist < 20.0 && best.is_none_or(|(_, d)| dist < d) {
                    best = Some((i, dist));
                }
            }
            if let Some((i, _)) = best {
                imp.drag_band.replace(Some(i));
                imp.drag_start_freq.set(bands[i].freq_hz);
                imp.drag_start_gain.set(bands[i].gain_db);
            }
        });
    }
    {
        let weak = view.downgrade();
        let curve_for_update = curve.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            let Some(this) = weak.upgrade() else { return };
            let imp = this.imp();
            let Some(bi) = *imp.drag_band.borrow() else { return };
            // `dx`/`dy` are deltas (not absolute), so the margin offset
            // itself cancels out — only `plot_w`/`plot_h` (not the raw
            // widget size) need to match what `draw_peq_curve()` actually
            // plots against, same reasoning as `drag_begin` above.
            let plot_w = (curve_for_update.width() as f64 - LEFT_MARGIN).max(1.0);
            let plot_h = (curve_for_update.height() as f64 - BOTTOM_MARGIN).max(1.0);
            let start_x = freq_to_x(imp.drag_start_freq.get(), plot_w);
            let new_freq = x_to_freq((start_x + dx).clamp(0.0, plot_w), plot_w).clamp(FREQ_MIN, FREQ_MAX);
            let gain_per_px = 24.0 / (plot_h - 20.0).max(1.0);
            let new_gain = (imp.drag_start_gain.get() + (-dy) * gain_per_px).clamp(-12.0, 12.0);
            let freq_spin = imp.freq_spins.borrow().get(bi).cloned();
            let gain_scale = imp.gain_scales.borrow().get(bi).cloned();
            if let (Some(freq_spin), Some(gain_scale)) = (freq_spin, gain_scale) {
                freq_spin.set_value(new_freq);
                gain_scale.set_value(new_gain);
            }
        });
    }
    {
        let weak = view.downgrade();
        drag.connect_drag_end(move |_, _, _| {
            let Some(this) = weak.upgrade() else { return };
            this.imp().drag_band.replace(None);
        });
    }
    curve.add_controller(drag);

    let bands_for_draw = view.downgrade();
    curve.set_draw_func(move |_da, cr, width, height| {
        if let Some(this) = bands_for_draw.upgrade() {
            draw_peq_curve(cr, width as f64, height as f64, &this.imp().bands.borrow());
        }
    });
}
