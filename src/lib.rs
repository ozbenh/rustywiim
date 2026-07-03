//! Library target exposing `device` for the standalone CLI tools under
//! `src/bin/` (e.g. `wiim-capture`). The GUI app (`src/main.rs`) does not use
//! this crate as a dependency — it still declares its own `mod device;` — this
//! is purely so `src/bin/*.rs` binaries, which are separate crates from
//! `main.rs`, can reach `rustywiim::device::...`.

pub mod capture;
pub mod device;
