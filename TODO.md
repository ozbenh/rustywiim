# Incomplete TODO list

* Continue improving handling of device and API quirks
  * Look at pywiim profiles.py and fold some of that into our own profiles
  * Look at various status reply sanitization code, also check WiiMDashboard
  * Add UPnP support for devices that require it
  * Add more caps detection around sub, EQ, etc...
  * Find testers :-)

* Move most of the device handling and API wrappers to a crate separate from
  the UI for other use cases ? That or do a daemon so we can also have a
  gnome-shell UI or similar...

* Check/fix handling of HTML in strings in metadata

* Add EQ (PEQ and GEQ) editor (I have code, just not quite publishable)

* Add sub config

* Add gnome notifications on song changes ? (TBD)
