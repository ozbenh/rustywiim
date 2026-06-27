# RustyWiiM

A simple Linux GTK4 front-end for WiiM media players written in Rust.

Copyright (c) 2026 Benjamin Herrenschmidt

Licensed under the [MIT License](LICENSE).

This was almost entirely written with the help of an AI tool and using quite a lot of "inspiration" from https://github.com/mjcumming/pywiim (and in some case translating the python code almost verbatim). My own contribution is mostly to direct the AI and some additional API reverse engineering beyond what I could find in pywiim.

This was mostly an exercise for me in using AI to program in a language I am not (yet) familiar with (Rust). The AI is so good I barely learned any Rust at this point but since the end result might be useful to some, here it is.

## Build instructions ##

### Install dependencies ###

#### Ubuntu / Debian ####
`sudo apt-get install cargo rustc libgtk-4-dev libadwaita-1-dev libssl-dev`

#### Fedora ####
`sudo dnf install cargo rust gtk4-devel libadwaita-devel openssl-devel`

### Build ###
`cargo build`

### Run ###
`target/debug/rustywiim`

There is no installer or package yet and you can of course build a release build rather than a debug build etc... but since it's all pretty wet behind the ears, those simple instructions will do.

## Options ##
For now just those two:

| Option          | Description                                           |
|:----------------|:------------------------------------------------------|
| `--debug-api`   | Dump in the console a log of API calls and rsesponses |
| `--debug-state` | Dump in the console detected device state changes     |

## Events ##


  * 0.1.0 - 2026-06-24
    * Initial release 0.1.0

  * 0.2.0 - 2026-06-25
    * Sorry, had to rebase ! Initial commit had to be fixed up.
    * Significant internal refactoring, code is a lot cleaner now, smaller
	  functions, better abstractions, better detection of device capabilities,
	  inputs and outputs etc... Should work better with other devices.

  * 0.3.0 - 2026-06-27
    * New mini-window mode
	* Various GUI cleanups, fixes and improvements
	* Support using system themes or our custom dark theme via a (primitive) settings dialog
	* Rate limit some API calls and add retries on request failures caused by disconnections
    * Additional implementation cleanups, still plenty of AI slop but slowly getting better

## Screenshots ##

![Screenshot](screenshot1.png)
![Screenshot](screenshot2.png)
![Screenshot](screenshot3.png)
