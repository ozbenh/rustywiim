//! The per-device window: `DeviceWindow`/`DeviceWindowInner` and its
//! chrome, display, and geometry code. The playback/preset/input-output
//! content it hosts lives in `ui/views/` — this module is the *hosting*
//! side: window construction and lifecycle, full/mini mode switching,
//! geometry bookkeeping and persistence, and the window-level chrome
//! (header, bottom bar, mini top bar/resize).

pub(in crate::ui) mod chrome;
pub(in crate::ui) mod display;
pub(in crate::ui) mod geometry;
