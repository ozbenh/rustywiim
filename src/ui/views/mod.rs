//! Self-contained, `DeviceState`-bound view objects.
//!
//! A "view" is a widget cluster (a GObject subclass of `adw::Bin`) that
//! owns its own widget tree, connects the `DeviceState` signals it needs
//! itself, and doesn't care who hosts it or how many instances exist —
//! a device window, a device-list row, or a popover can all embed one.
//! Views communicate *up* to their host via GObject signals (requests
//! like "open input configuration"), never by knowing what the host is;
//! device-directed actions (play/pause, volume, input switch, …) go
//! straight to the bound `DeviceState`.
//!
//! Shared lifecycle contract:
//! - Bound to one `DeviceState` at construction, never rebound.
//! - `set_active(bool)`: signal handlers early-return while inactive;
//!   flipping inactive → active runs a full refresh (including the
//!   offline/disconnected rendering, so a view activated while the
//!   device is gone never shows stale content).
//! - `dispose()` disconnects the stored `DeviceState` handler ids and
//!   unparents any `set_parent()`-attached popovers.

pub(crate) mod common;
pub(crate) mod io;
pub(crate) mod playback_mini;
pub(crate) mod presets;
pub(crate) mod volume;
