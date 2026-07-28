//! Touch-input detection — shared by Kiosk mode's cursor-hiding
//! (`kiosk_hide_cursor_on_touch`) and the on-screen keyboard's `Auto` mode
//! (`ui::osk`), both of which want the same "does this seat have a touch
//! screen" answer.

/// Whether the default seat reports touch capability — the same check
/// `src/experiments/check_touch.py` prototyped
/// (`GdkDisplayManager`→default display→default seat→`SeatCapabilities`),
/// ported to gtk4-rs.
pub(crate) fn has_touchscreen() -> bool {
    use gtk::prelude::*;
    let Some(display) = gtk::gdk::DisplayManager::get().default_display() else { return false };
    let Some(seat) = display.default_seat() else { return false };
    seat.capabilities().contains(gtk::gdk::SeatCapabilities::TOUCH)
}
