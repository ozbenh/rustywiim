# RustyWiiM

[![GitHub release](https://img.shields.io/github/v/release/ozbenh/rustywiim)](https://github.com/ozbenh/rustywiim/releases)

A Linux GTK4 front-end for WiiM media players written in Rust. It should also work with other LinkPlay based players, tested with iEAST AudioCast and an AudioPro C5 for now, see below how to send me data to help support other devices if you own them.

Copyright (c) 2026 Benjamin Herrenschmidt

Licensed under the [MIT License](LICENSE).

This started as an exercise in using AI to program in Rust which I am not familiar with, so trying to both build experience with driving AI and learn a bit of Rust...

The former is a hit, the latter, less so, at least initially as the AI did too well :-)

Now, though, as the project slowly evolves (matures ?), I'm getting more involved with the code, and while a lot is still written by AI, it's under much more precise directions, ie the amount of "slop" is hopefully decreasing. As a result I am slowly learning Rust, ah !

## Pre-built packages ##

See [Releases page](https://github.com/ozbenh/rustywiim/releases)

## Build instructions ##

### Install dependencies ###

#### Ubuntu / Debian ####
`sudo apt-get install cargo rustc libgtk-4-dev libadwaita-1-dev libssl-dev libglib2.0-dev-bin`

#### Fedora ####
`sudo dnf install cargo rust gtk4-devel libadwaita-devel openssl-devel glib2-devel`

### Build ###

* Basic build:

`cargo build`

or

`make`

* Package build (.deb or .rpm depending on your distro):

`make package`

### Run ###
`target/debug/rustywiim`

## Options ##
For now just this one:

| Option              | Description                                                                                                                                                              |
|:--------------------|:-------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `--debug=<options>`   | Comma-separated list of debug/tracing options: `api` (dump API calls), `state` (state change messages), `device` (device capabilities detection), `discovery` (the discovery machinery), `upnp` (the UPnP protocol layer), `ui` (parts of the GUI code), `config` (config file management), `all` (all of the above) |
| `--tls=<mode>`        | Override TLS mode: `wiim` (default), `audio-pro`, `any`, `http`                                                                                                         |
| `--connect=<url>`     | Connect directly to `scheme://ip[:port]` (e.g. `http://127.0.0.1:8080` for `wiim-simulator`), opening a device window for it immediately instead of discovery |
| `--no-config`         | Don't load or save the config file — every run behaves like a fresh install |
| `--config-file=<path>`| Use an alternate config file path instead of the default (for testing) |

## Helping with your device ##

Since I can only really test here with a WiiM Ultra and the implementation of the API seems to vary fairly wildly from device to device (or FW version to FW version), I have added a little tool that gets built in `target/debug/wiim-capture`.

You call it by passing the IP address of your device as an argugment, for example:

`wiim-capture 192.168.1.38`

It will send a number of non-destructive commands to the device (basically all "get" type commands), and generate a large JSON file called "<model>_<date>_<time>.json", for example "WiiM_Ultra_20260704_104058.json". Unless I missed some, all the MAC addresses, IP addresses, SSIDs, UUIDs etc... (identifying information) should be sanitized out.

You can pretty-print this file using `target/debug/wiim-capdump`. I would appreciate capture files sent to me (benh@kernel.crashing.org) so I can keep a collection. For now any device that isn't a WiiM Ultra, I will update this once I have enough of them with more precise asks. Please also let me know if you are ok with me shipping the file in a future version since I plan to build some testing infrastructure using those capture files. Thanks !

## Known issues ##

* There's an occasional row of stale pixels at the top of the scrolling song title in the miniaturized window. This happens with older gtk versions such as the one in Ubuntu 24.04 and is related to bugs in the gtk4 renderer. I have tried various workarounds but so far without great success. I'll investigate replacing some of this code with direct cairo rendering, see if that helps.

## Changelog ##

  * 0.9.0 - 2026-07-15
    * Add AudioPro C5 support (old and new firmwares)
	* Major internal rework of device management to clean up the overall
	  code structure, and get rid of the "split" responsibility of device
	  polling between discovery and device state management. This simplifies
	  things and will avoid interesting classes of bugs and enables more
	  UI elements to be client of the device state. The device state now has
	  a simple and a full mode, depending on whether minimal info is requested
	  (polled every 5s) or full details (every 1s). This also moves more of
	  the discovery code to the non-UI part which will eventually becomes
	  a separate re-usable crate (and maybe a shared lib too).
    * The device list now uses the Device State in simple mode to display
	  artwork and current song for active devices in the list. It also gets
	  a volume control for quick access to devices volumes.
    * Fix issues with mute setting not syncing properly
	* Improve display quality of icons under some circumstances and add new
	  custom icons for RCA and Jack plugs (improve detection of the plug type
	  on some devices as well)
    * Fix incorrect inputs list on some WiiM devices (such as bogus Coax
	  input on the Ultra).
    * Add capture support for the old "TCP UART" protocol still used by some
	  3rd party linkplay-powered devices. We don't use it in rustywiim yet
	  but it will be eventually needed for things like bass/treble control
	  on AudioPro C5 (and more).
    * The Mini window is now the same window as the main window, it just
	  gets resized. This fixes/simplifies a lot of internal logic and makes
	  the switch faster. It also avoid the window popping in random places
	  on the screen when switching. The one drawback is a visual glitch
	  when maximizing (double click on normal window title bar), then
	  switching to mini mode, back to normal mode, and un-maximizing. I think
	  we can live with that.
    * Major internal rework of the UI components. The various widget "clusters"
	  (called views) are now in separate modules (presets, input/outputs,
	  standard player, mini player, volume control) for better re-usability.
	  No visible effect (hopefully) other than code cleanliness, but this will
	  make it easier to implement different visual layouts, such as a Kiosk
	  mode in the future where some of these things are "pop overs" over the
	  main window for example. This hasn't yet extended to the entries in the
	  device list.
  
  * 0.8.2 - 2026-07-10
    * Fix volume button & scale disabled

  * 0.8.1 - 2026-07-10
    * Fix input pop-ver flip/flopping when switching inputs
	* Add support for setting Mute via UPnP and make it the default.
	  Also add fallback to querying via a separate UPnP command when
	  GetInfoEx doesn't return it (AudioCast).
	  This matches the WiiM App behaviour as far as I can tell and
	  fixes mute handling on AudioCast devices.
    * Add better support for Bluetooth sink (bluetooth as input). The
	  connection state is displayed and the UI properly cleared when
	  disconnected. A button "Restart pairing" appears when BT is the
	  current input and not currently in pairing state. Matches the
	  behaviour of the WiiM App.
    * Add retries on UPnP and generally improve error handling
	* A pile of fix around devices being or going offline/online

  * 0.8.0 - 2026-07-08
    * Add support for iEAST AudioCast (not yet Pro, AMP, etc... just
	  the base one, though the others might partially work, please send
	  captures !)
    * Add UPnP support for retrieving player status. For now switch all
	  devices to UPnP by default, but an "Advanced" Settings tab can be
	  used to switch back to HTTP if that doesn't work for you (please
	  open a github issue and ideally send a capture too). This provides
	  richer information (such as the Tidal quality label) and means a
	  single API call per 1s poll. We still just poll, GENA subscription
	  will come later.
    * Add UPnP preset retrieval for use when HTTP getPresetInfo is not
	  supported (enables preset to work with AudioCast, and there are
	  indications that might also help Arylic devices).
    * wiim-captures captures more things
	* Fix speaker out icon on WiiM amp in Outputs menu
	* Minor cosmetic improvements (some things are a bit more readable)
	* More --debug options and diagnostic output
	* Preset artworks are now fetched concurrently and asynchronously,
	  so your preset list will show up more quickly, potentially with
	  generic icons, which will get updated as the artworks are fetched.
    * Prev/Next buttons, seek bar, and loop control buttons are now
	  disabled when sources don't support them (the Spotify case is a
	  bit finnicky ... free accounts *seem* to support "Next" but not
	  "Prev" though the WiiM app supports neither in that case).
    * Fix input detection on WiiM Mini

  * 0.7.0 - 2026-07-06
    * Add cargo & Makefile rules to build packages
    * Add binary package releases on github
	* Fixes around handling of HDMI input
	* Improvement in device discovery, don't hammer unrelated devices
	* Add bluetooth remote info and Wifi signal strength

  * 0.6.4 - 2026-07-06
    * Add basic wiim-simulator (work in progress) for testing purposes
    * Major cleanup of the handling of the player state to better abstract the
      backend from the UI, some prep work towards being able to use UPnP for
      player status which seems to be what the WiiM official app does.
    * Fix WiiM Amp Ultra detection and outputs handling
    * Fix name and icon for "Speaker" output for other "Amps" models

  * 0.6.3 - 2026-07-05
    * Remove remaining target_ip field from capture files

  * 0.6.2 - 2026-07-04
    * Make modern theme the default
    * Add wiim-capture and wiim-capdump for creating/viewing command capture files

  * 0.6.1 - 2026-07-03
    * Rework mini-window resize to avoid compositor maximization (side effect: it
      can only be resized from the right hand edge, not the left hand one).
    * Add key shortcuts (left & right for prev & next, space for play/pause, up & down
      for volume and M for minimize/maximize).
    * When closing the last window, don't save it as closed. The app will quit and
      will be re-launched with that window opened instead of the device-list now.

  * 0.6.0 - 2026-07-02
    * Add animations (song transitions and side panel open/close)
    * Add a new "modern" theme with blurry art background and transparency
    * A few cosmetic tweaks here or there
    * Hammer the WiiM a bit less on poll
    * Mini window is horizontally resizable

  * 0.5.0 - 2026-07-02
    * Small cosmetic improvements (volume button, rendering glitches, slightly
      bigger fonts and less dim text).
    * Should properly fix stale artwork when switching to a song with no artwork
    * A whole lot of internal implementation cleanups, optimisations and fixes.

  * 0.4.3 - 2026-06-30
    * Fix name/model display in device list for non pinned devices

  * 0.4.2 - 2026-06-30
    * Really fix the refresh of all windows and widgets on theme switch ! So far it does
      seem to work even when starting the app with the custom dark theme.
    * Various small cosmetic and UI behaviour adjustments
    * Fix auto-reopening on windows for non-pinned devices

  * 0.4.1 - 2026-06-30
    * Fix (again, maybe for real now ?) refresh of all windows when changing theme
      [EDIT: FAIL ! It didn't fix it]

  * 0.4.0 - 2026-06-30
    * A whole lot of internal shuffling and cleaning up, various bug fixes, etc...
    * There is now a "Devices list" window. It will be displayed on launch in absence of
      existing opened window in the config, and can be opened via the menu otherwise. It
      replaces the old device selection popover. As a result it is now possible to open
      multiple device windows. Each device entry has a "pin" button (currently a star but
      that might change). This forces the device to remain listed even if it is not
      responding on the network. There is a + button to add devices via manual IP entry
      (they will be pinned by default).
    * Song title, album & artist fields are now scrollable. When they are too big to fit
      the window they will slowly scroll.
    * Note: There have been significant changes to the config file format, it's unlikely
      that previous settings will be preserved.

  * 0.3.0 - 2026-06-27
    * New mini-window mode
    * Various GUI cleanups, fixes and improvements
    * Support using system themes or our custom dark theme via a (primitive) settings dialog
    * Rate limit some API calls and add retries on request failures caused by disconnections
    * Additional implementation cleanups, still plenty of AI slop but slowly getting better

  * 0.2.0 - 2026-06-25
    * Sorry, had to rebase ! Initial commit had to be fixed up.
    * Significant internal refactoring, code is a lot cleaner now, smaller
      functions, better abstractions, better detection of device capabilities,
      inputs and outputs etc... Should work better with other devices.

  * 0.1.0 - 2026-06-24
    * Initial release 0.1.0



## Screenshots ##

![Screenshot](pics/screenshot1.png)
![Screenshot](pics/screenshot2.png)
![Screenshot](pics/screenshot3.png)
![Screenshot](pics/screenshot4.png)
![Screenshot](pics/screenshot5.png)
![Screenshot](pics/screenshot6.png)
