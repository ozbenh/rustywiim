# TODO / known issues

Folded in from `ANALYSIS.md` (now retired) — only what's still actually open;
resolved/deliberate/dismissed items were dropped. Git history has the full
investigation detail if any of these need re-digging into later.

## Bugs

* **Switching to HDMI input may send the wrong wire value.**
  `switch_input()` (`state.rs:1159-1164` → `api.rs:746-748`) sends the
  canonical input ID verbatim as `setPlayerCmd:switchmode:{id}` — no
  translation layer. Every canonical ID is lowercase (`"wifi"`,
  `"bluetooth"`, `"line-in"`, `"optical"`, `"phono"`, `"hdmi"`), but the
  authoritative `wiim` SDK's own `InputMode` enum (`consts.py:225-231`) has
  `HDMI = (16, "TV", "HDMI")` — every other entry's wire command name is
  lowercase, HDMI's specifically isn't. The OpenAPI catalog's
  `setPlayerCmd:switchmode:{mode}` enum agrees:
  `['line-in', 'bluetooth', 'optical', 'udisk', 'wifi', 'HDMI']`. If the
  firmware does exact string matching (plausible — nothing else in this API
  is case-insensitive), switching to HDMI from our dropdown may never have
  actually worked. Found while investigating the duplicate-HDMI-entry bug
  below; unrelated to and not fixed by that change. Unverified against real
  hardware — needs testing before deciding whether/how to special-case it
  (a case-preserving mapping just for the switchmode wire value, keeping
  the canonical ID lowercase everywhere else for consistency/icons).

* **`getAudioOutputStatus` legacy fallback not probed.** `api.rs`'s
  `get_audio_output()` only calls `getNewAudioOutputHardwareMode`. pywiim
  falls back to the older `getAudioOutputStatus` when that fails (per
  `mjcumming/wiim#144`); no currently-supported device is known to actually
  need it, so this is defensive-completeness, not a confirmed-broken fix.
  Same "probe once at connect, remember the outcome" shape as the
  outputs/inputs probing already in `capabilities::detect_capabilities()`.

* **Discovery keeps re-probing devices confirmed non-WiiM** (e.g. a Samsung
  TV, a Chromecast). Two of the three planned layers are now **implemented**
  in `discovery.rs`, both session-only (in-memory `Inner` state, never
  persisted to `config.json` — a DHCP reassignment or app restart always
  gets a clean retry):
  * **Done**: `is_likely_non_linkplay()` matches the SSDP `SERVER`/`ST`/`NT`
    headers (now captured by `parse_ssdp_packet()`) against a conservative,
    certain-negatives-only denylist (ported from pywiim's
    `NON_LINKPLAY_SERVER_PATTERNS`/`NON_LINKPLAY_ST_PATTERNS` —
    `"sonos"`/`"chromecast"`/`"samsung"`/`"smartthings"`/etc., and
    `ZonePlayer`/`roku`/`dial`/Samsung service-type URNs), applied *before*
    any HTTP probe — zero extra network round-trips. Devices with a generic
    `SERVER: Linux` header (common on Arylic/Audio Pro) don't match and fall
    through to a real probe, same as always. Needs testing against a real
    Samsung TV/Chromecast to confirm their actual SSDP headers hit the list.
  * **Done**: a consecutive-failure counter (`NON_API_FAIL_THRESHOLD = 3`)
    for whatever slips through the header filter — after 3 full
    `identify_device()` failures (each already trying every `PROBE_MODES`
    entry + the description.xml fallback) an IP is treated as confirmed
    non-API and skipped on further SSDP re-announcements for the rest of
    the run.
  * **Still open**: the `description.xml` `<manufacturer>`/`<modelName>`
    check (what the authoritative `wiim` SDK actually gates on) as an
    *earlier* check than the failure counter, for devices the header
    denylist doesn't catch — it's already fetched today, but only as a
    last-resort fallback *after* every `getStatusEx` probe has failed.
    Costs one extra HTTP GET per candidate IP, unlike the header filter.
    Lower priority now that the two cheaper layers exist.

## Tech debt / clarity

* UTF-8 safety reasoning in `config.rs:252-284`'s `strip_trailing_commas` is
  non-obvious (safe in practice, since none of the byte-matched characters
  can appear as a non-first byte of a multi-byte UTF-8 sequence) — deserves
  a comment saying so.
* `ui/mod.rs:196`'s `queue_draw_recursive` walks every widget in every
  window on every theme switch (twice — immediate + idle callback). Probably
  unnecessary given GTK4's own invalidation; expensive for complex trees.
* `#![allow(deprecated)]` for the old `glib::clone!` `@strong`/`@weak`
  syntax, now spread across most of `src/ui/` (9 files) — should move to the
  newer syntax or explicit `Rc::downgrade` (already the pattern used in
  `ui/mod.rs`'s signal handlers).
* Health checks re-derive `DeviceCapabilities` from scratch every 30s
  (`devlist.rs`'s `trigger_health_check_for()`) just to get a model-name
  string. Bigger fix than a
  quick cache: give every *discovered* device a real `DeviceState` (not
  just ones with an open window) and let its normal poll cadence double as
  the health check, instead of devlist.rs's independent ping path. Its own
  design pass, not a quick patch.

## Capabilities / `EndpointConfig`

* 8 remaining `EndpointConfig` fields (`supports_player_status_ex`,
  `supports_get_meta_info`, `supports_eq`, `supports_eq_set`,
  `supports_alarms`, `supports_sleep_timer`, `status_endpoint`,
  `reboot_command`) are declared per-family but never consulted anywhere
  except a debug print. Cross-checked against pywiim: 7 of the 8 have a
  real feature behind them there (status-endpoint selection, EQ/metadata
  gating, a real `reboot()`, alarm/sleep-timer capability properties) that
  our port never finished wiring up; only `supports_eq_set` is dead in
  pywiim too. **Not decided yet** — next step is reconciling against
  Wiim-Dashboard/wiim-now-playing/linkplay-cli before choosing to wire
  these up for real or remove them (as already done for the unrelated,
  fully-invented `supports_preset_info`, which had no pywiim equivalent at
  all and is gone).

## UI polish

* **Maybe**: add a `border-radius` clip to the main album-art widget
  (`FlipCover`) to match how the WiiM device/app hide baked-in white
  corners on some cover art (confirmed via a real capture: the corners are
  literally opaque white pixels in the JPEG, not transparency — WiiM's own
  UI unconditionally clips artwork to a rounded rect, Tidal's app doesn't).
  Only `.preset-art`'s small thumbnails get this today; the big artwork
  display doesn't. Deliberately **not decided** — rounding every cover
  would also round ones that are already square/full-bleed, which cuts
  against the "vinyl record" look intended for the main view. Experiment
  with it before committing either way.
* Verify `SwipeText`'s (`widgets.rs:52-92`) plain `gtk::Stack` +
  `StackTransitionType::SlideLeft` actually eases the same way as the three
  other animations (`flip_cover.rs`, `art_background.rs`, `playback.rs`'s
  `animate_panel_to()`), which all explicitly use
  `adw::Easing::EaseInOutCubic`/`EaseInOutQuad` via `adw::TimedAnimation`.
  `gtk::Stack`'s built-in transition easing isn't something our code sets
  explicitly — unclear without checking GTK4's source or observing it
  directly whether it's linear or already eased.

* Add a hover tooltip on the WiFi icon showing the signal level in dBm.
  `DeviceInfo.rssi` (`api.rs:344-345`, `getStatusEx`'s `RSSI` field) is
  already parsed and used to pick the icon variant in
  `update_network_icon()` (`ui/playback.rs:235-246`) — just needs a
  `set_tooltip_text()` alongside the existing `set_icon_name()` call.

## Roadmap / wishlist (pre-existing)

* Continue improving handling of device and API quirks
  * Look at various status reply sanitization code, also check WiiMDashboard
  * Add UPnP support for devices that require it
  * Add more caps detection around sub, EQ, etc...
  * Find testers :-)
* Move most of the device handling and API wrappers to a crate separate from
  the UI for other use cases ? That or do a daemon so we can also have a
  gnome-shell UI or similar...
* Add EQ (PEQ and GEQ) editor (I have code, just not quite publishable)
* Add sub config
* Add gnome notifications on song changes ? (TBD)
* Add all sorts of device settings to the Settings dialog — today's "Device"
  section only has "Advanced" (`PlaybackAccessConfig` overrides) and "About"
  (a static info snapshot); nothing exposes actual device configuration
  (alarms, sleep timer, LED brightness/touch-control lock, network info
  beyond the About page, etc.). Scope not decided yet — needs a pass over
  what the WiiM app itself exposes and what our own `EndpointConfig`
  reconciliation (see "Capabilities / `EndpointConfig`" above) turns up as
  actually wired/available first.
* Implement the UPnP transport for real. `device/upnp.rs` is currently just
  a skeleton (wire-shaped, not canonical, response types + an `UpnpClient`
  whose methods are all `unimplemented`-style stubs), and
  `AccessMethod::UpnpPolled` is already accepted end-to-end by
  `PlaybackAccessConfig`/config/Settings' "Advanced" per-device panel — it
  just silently falls back to the HTTP default with a `--debug=state`
  warning today since there's no real fetch path behind it. Real UPnP
  eventing (GENA subscriptions) vs. plain polled `AVTransport`/
  `RenderingControl` SOAP calls is an open design question — see the
  authoritative `wiim` SDK's subscription-renewal approach researched
  earlier (`docs/wiim-capability-research.md`) for one real-world reference
  if going the eventing route. Supersedes the vaguer pre-existing "Add UPnP
  support for devices that require it" bullet above.
