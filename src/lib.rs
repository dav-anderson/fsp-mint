//! fsp-mint: Fast Service Points, issued by one service in exchange for ESP
//! named for it, and spent at that service with a swap.
//!
//! - [`config`]: the settings.
//! - [`api`]: the request and reply types (the wallet uses them too).
//! - [`keysets`]: the FSP signing keys, their rotation, and the key file.
//! - [`store`]: atomic JSON files.
//! - [`issue`]: the issue call (kind 11), ESP in (transferred at the Issuer and kept), FSP out.
//! - [`spend`]: the spend call (kind 12), the fast path, with the spent lists and their log.
//! - [`esp_client`]: the calls to the ESP mint (the Issuer) and what they return.
//! - [`esp_air`]: those calls over AIR, the only way this mint reaches the ESP mint.
//! - [`export`]: `export-esp`, the kept ESP written as `sp_wire::esp::handover::Handover` files,
//!   by the running `serve` (or by the command when none runs).
//! - [`lock`]: the data folder's writer lock: one fsp-mint process writes it at a time.
//! - [`server`]: what the mint answers (an outcome function per call) and the
//!   HTTP routes left: `GET /audit`, `GET /health` and `POST /export-esp`.
//! - [`stream`]: the key set stream, the live keysets published on AIR so that every wallet sees the same keys.
//! - [`air_listener`]: this mint's own AIR listener, the only way wallets reach it.

pub mod air_listener;
pub mod api;
pub mod audit;
pub mod config;
pub mod esp_air;
pub mod esp_client;
pub mod export;
pub mod issue;
pub mod keysets;
pub mod lock;
pub mod server;
pub mod spend;
pub mod store;
pub mod stream;

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether `units` is a power of two (FSP denominations).
pub fn is_denomination(units: u64) -> bool {
    units.is_power_of_two()
}
