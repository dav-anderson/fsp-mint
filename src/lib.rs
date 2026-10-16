//! fsp-mint: Fast Service Points, issued by one service in exchange for ESP
//! named for it, and spent at that service with a swap.
//!
//! - [`config`]: the settings.
//! - [`api`]: the request and reply types (the wallet uses them too).
//! - [`keysets`]: the FSP signing keys, their rotation, and the key file.
//! - [`store`]: atomic JSON files.
//! - [`issue`]: `POST /issue`, ESP in (transferred at the Issuer and kept), FSP out.
//! - [`spend`]: `POST /spend`, the fast path, with the spent lists and their log.
//! - [`esp_client`]: the calls to the ESP mint.
//! - [`server`]: the HTTP routes.

pub mod api;
pub mod config;
pub mod esp_client;
pub mod issue;
pub mod keysets;
pub mod server;
pub mod spend;
pub mod store;

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
