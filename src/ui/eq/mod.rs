//! EQ editor UI — the reusable band-editor widgets (`GraphicEqView`/
//! `ParametricEqView`), the host panel (`panel`), and the small chrome
//! widgets (`chrome`: `EqMechanismToggle`/`EqSourcePicker`/
//! `EqChannelToggle`/`EqChannelPicker`/`EqPresetPicker`) that assemble
//! them into a working editor for a device's per-source EQ.
//!
//! **Deliberately a sibling of `ui/views/`, not inside it**: `views/`'s
//! own module doc comment defines "a view" as bound to one `DeviceState`
//! for its whole lifetime, with an `active` flag and device signal
//! subscriptions — none of that applies here. `GraphicEqView`/
//! `ParametricEqView` are pure data-in/data-out widgets (`set_state()`/
//! `state()`/a `band-changed` signal, nothing else), on purpose: the same
//! widget instance has to work identically for a per-source slot, the
//! room-correction layer, or a future per-output layer, and none of those
//! map to "one `DeviceState`, bound at construction." All device I/O for
//! this feature goes through `device::eq::EqSession` instead, owned by
//! the host panel, not by these widgets.

pub(crate) mod chrome;
pub(crate) mod graphic;
pub(crate) mod panel;
pub(crate) mod parametric;

/// Dim-grey shared by both band editors' hand-drawn (Cairo, not styled
/// child widgets) grid lines/axis labels — deliberately not a themed color
/// pulled from CSS: a fixed mid-grey reads reasonably on both light and
/// dark themes without needing real theme integration, out of scope for
/// this first-pass visual treatment.
pub(super) const GRID_RGBA: (f64, f64, f64, f64) = (0.5, 0.5, 0.5, 0.35);
pub(super) const LABEL_RGBA: (f64, f64, f64, f64) = (0.5, 0.5, 0.5, 0.9);
