from gi.repository import Gtk, Gdk

def check_for_touchscreen_early():
    # 1. Fetch the global display manager instance
    manager = Gdk.DisplayManager.get()
    
    # 2. Get the primary connected display (Cage)
    display = manager.get_default_display()
    
    # Handle the edge case where the Wayland display connection isn't ready yet
    if not display:
        print("Error: No display connection found. Is Cage running?")
        return False
        
    # 3. Request the active input seat configuration
    seat = display.get_default_seat()
    if not seat:
        print("Error: Input seats are not initialized yet.")
        return False
        
    # 4. Check the mask capabilities for touch hardware
    caps = seat.get_capabilities()
    
    if caps & Gdk.SeatCapabilities.TOUCH:
        print("Early Check: Touchscreen detected via GdkDisplayManager!")
        return True
    else:
        print("Early Check: No touchscreen detected.")
        return False

# Example usage in your application startup flow:
def on_activate(app):
    # Run the check early before constructing any widgets
    has_touch = check_for_touchscreen_early()
    
    window = Gtk.ApplicationWindow(application=app)
    
    # If a touchscreen is present, automatically hide the mouse pointer
    if has_touch:
        blank_cursor = Gdk.Cursor.new_from_name("none", None)
        window.set_cursor(blank_cursor)
        
    window.present()

app = Gtk.Application(application_id="org.example.kiosk")
app.connect("activate", on_activate)
app.run(None)
check_for_touchscreen

import gi
# Ensure the correct API versions are requested BEFORE importing the modules
gi.require_version('Gtk', '4.0')
gi.require_version('Gdk', '4.0')


