# TODO / known issues

Folded in from `ANALYSIS.md` (now retired) — only what's still actually open;
resolved/deliberate/dismissed items were dropped. Git history has the full
investigation detail if any of these need re-digging into later.

## Bugs

* **`getAudioOutputStatus` legacy fallback not probed.** `api.rs`'s
  `get_audio_output()` only calls `getNewAudioOutputHardwareMode`. pywiim
  falls back to the older `getAudioOutputStatus` when that fails (per
  `mjcumming/wiim#144`); no currently-supported device is known to actually
  need it, so this is defensive-completeness, not a confirmed-broken fix.
  Same "probe once at connect, remember the outcome" shape as the
  outputs/inputs probing already in `capabilities::detect_capabilities()`.

* **Discovery keeps re-probing devices confirmed non-WiiM** (e.g. a Samsung
  TV answering SSDP). `discovery.rs`'s `identify_device()`/`probe_api()`
  re-runs the full `getStatusEx` probe across all 3 `PROBE_MODES` every time
  a non-WiiM IP reappears (every 60s M-SEARCH cycle, every `ssdp:alive`),
  forever. Two complementary fixes, checked against how other projects
  handle this:
  * **Cheaper, do this first**: pywiim filters out *confirmed*-non-LinkPlay
    devices using only the **raw SSDP response headers it already has in
    hand** — `is_likely_non_linkplay()` matches the `SERVER`/`ST` headers
    against an explicit denylist (`"Sonos"`, `"Chromecast"`, `"Samsung"`,
    `"SmartThings"`, `ZonePlayer`/`roku`/`dial` service-type URNs, etc.),
    applied *before* any HTTP probe at all — zero extra network round-trips,
    since the SSDP packet is already received. Deliberately conservative
    (only catches devices that self-identify unambiguously; Arylic/Audio
    Pro often send a generic `"Linux"` `SERVER` header and fall through to
    a full probe anyway, same as today) but would catch exactly the
    reported Samsung TV case for free. Our own `parse_ssdp_packet()`
    (`discovery.rs:347`) doesn't capture `SERVER`/`ST` yet — needed first.
  * **Fallback for the rest**: `description.xml`'s `<manufacturer>`/
    `<modelName>` (what the authoritative `wiim` SDK actually gates on —
    `_is_supported_wiim_device()` checks `device.manufacturer` before ever
    attempting the HTTP API) is already fetched by us today, but only as a
    last-resort fallback *after* every `getStatusEx` probe already failed —
    check it early instead, for devices the SSDP-header filter above didn't
    catch. Costs one extra HTTP GET per candidate IP, unlike the header
    filter. `linkplay-cli`, for reference, does neither — it has no
    pre-filter at all and relies purely on `getStatusEx` success/failure,
    same weakness we have now.
  * Plus, independently, a simple consecutive-failure counter as a
    backstop regardless of which signal is used.
  * Must live in `DiscoveryService`'s in-memory state only, never
    persisted to `config.json` — a DHCP reassignment or app restart should
    always get a clean retry.

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
