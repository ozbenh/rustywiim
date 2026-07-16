pub mod api;
pub mod capabilities;
pub mod discovery;
pub mod discovery_manager;
pub mod gena;
pub mod manager;
pub mod playback;
pub mod state;
pub mod tcpuart;
pub mod upnp;

/// Wall-clock timestamp prefix for `--debug=*` log lines (`HH:MM:SS.mmm`,
/// local time) — every per-module `dbg()`/`debug()` function in this crate
/// prefixes its output with this, so interleaved multi-device/multi-module
/// debug logs stay chronologically readable. `config.rs`/`ui/mod.rs` (in
/// the separate main binary crate, outside this library) keep their own
/// tiny duplicate of this one-liner rather than depending on it directly —
/// not worth exposing as public API surface just for a logging cosmetic.
pub(crate) fn timestamp() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}
