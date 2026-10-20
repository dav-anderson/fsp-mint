//! Settings: `fsp-mint.toml` in the data directory (or `FSP_CONFIG`), then
//! `FSP_...` environment variables, then checks. The same shape as esp-mint and
//! psp-mint: every setting is in `SPECS`, an unknown key, a wrong type or a value
//! out of range stops the mint with every problem listed, and the values in use
//! are logged at startup with where each came from. Read once at startup; no
//! reload yet.
//!

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sp_wire::ServiceDid;
use sp_wire::storage::StoragePolicy;

pub const FILE_NAME: &str = "fsp-mint.toml";
pub const FILE_ENV: &str = "FSP_CONFIG";

/// The default for `issue.max_items` and `spend.max_items`. It is only a
/// default: the limits in force come from the settings (and are served on
/// `GET /fees`), never from this constant.
pub const DEFAULT_MAX_ITEMS: usize = 1000;

/// Settings that were taken out, with the environment variable each had and what
/// replaces it. A file or an environment that still names one is refused with this
/// text, not with "unknown setting".
pub const REMOVED: &[(&str, &str, &str)] = &[
    (
        "esp.url",
        "FSP_ESP_URL",
        "`esp.url` is gone: fsp-mint reaches the ESP mint only over AIR now, not over HTTP. Set air.esp_did (the ESP mint's DID; `esp-mint pubkey` prints it), air.esp_server and air.esp_host (FSP_AIR_ESP_DID, FSP_AIR_ESP_SERVER, FSP_AIR_ESP_HOST) and remove esp.url",
    ),
    (
        "esp.timeout_secs",
        "FSP_ESP_TIMEOUT_SECS",
        "`esp.timeout_secs` is gone: the calls to the ESP mint are over AIR, and the waits are air.reply_wait_secs, air.attempts and air.timeout_secs (FSP_AIR_REPLY_WAIT_SECS, FSP_AIR_ATTEMPTS, FSP_AIR_TIMEOUT_SECS). Remove esp.timeout_secs",
    ),
];

pub struct Spec {
    pub key: &'static str,
    pub env: &'static str,
    pub default: &'static str,
    pub units: &'static str,
    pub what: &'static str,
}

pub const SPECS: &[Spec] = &[
    Spec {
        key: "server.network",
        env: "FSP_NETWORK",
        default: "main",
        units: "network",
        what: "The Bitcoin network of the ESP mint this FSP mint takes ESP from: main, regtest, signet or testnet. Checked at startup against the ESP mint's network, which it reports in its mint info (over AIR); a mismatch stops the mint.",
    },
    Spec {
        key: "server.bind",
        env: "FSP_BIND",
        default: "127.0.0.1:3200",
        units: "host:port",
        what: "The address of the HTTP API that is left: the operator's `GET /audit`. Must be a loopback IP:port: wallets reach this mint over AIR only, never over HTTP.",
    },
    Spec {
        key: "server.max_request_bytes",
        env: "FSP_MAX_REQUEST_BYTES",
        default: "1048576",
        units: "bytes",
        what: "Ceiling on a request body (an issue or a spend, as the AIR listener reads it), as on the other mints. Over it: a signed refusal with the 413 body, before anything is parsed. 4096 to 1 GiB.",
    },
    Spec {
        key: "service.did",
        env: "FSP_SERVICE_DID",
        default: "",
        units: "DID (66 hex)",
        what: "The service's ID: its AIR DID, a 33-byte compressed public key (66 hex characters, starting 02 or 03; the old 32-byte x-only form is refused). Only ESP named for it is accepted. The secret half is service_key.hex in the data folder (fsp-mint service-key makes it) and must match. Required by serve.",
    },
    Spec {
        key: "fees.issue_fee_per_byte",
        env: "FSP_ISSUE_FEE_PER_BYTE",
        default: "0",
        units: "units/byte",
        what: "The ESP to FSP fee: FSP units per byte of each group's canonical size (16 + 106 a note + 41 an output), taken from the group's ESP. 0 to 10^12.",
    },
    Spec {
        key: "rotation.activation_hours",
        env: "FSP_ROTATION_ACTIVATION_HOURS",
        default: "8760",
        units: "hours",
        what: "Hours after the mint's first FSP issuance (recorded once in first_issue.json) before the first rollover. 0: rotation off, one keyset and one spent list for ever. 0 to 876000.",
    },
    Spec {
        key: "rotation.period_hours",
        env: "FSP_ROTATION_PERIOD_HOURS",
        default: "8760",
        units: "hours",
        what: "How long a keyset is current. Its notes stay spendable for one more period as previous, then are deleted with its spent list. 1 to 876000.",
    },
    Spec {
        key: "issue.max_items",
        env: "FSP_ISSUE_MAX_ITEMS",
        default: "1000",
        units: "notes",
        what: "The most groups in one issue request, and the most ESP notes and the most FSP outputs in one group. More is refused 400 too_many_items before anything is spent. The wallet reads it from the fees call. 1 to 100000.",
    },
    Spec {
        key: "spend.max_items",
        env: "FSP_SPEND_MAX_ITEMS",
        default: "1000",
        units: "notes",
        what: "The most notes in, and the most outputs, in one spend. More is refused 400 too_many_items. The wallet reads it from the fees call. 1 to 100000.",
    },
    Spec {
        key: "spend.max_memo_bytes",
        env: "FSP_SPEND_MAX_MEMO_BYTES",
        default: "256",
        units: "bytes",
        what: "The longest memo on a spend. A longer one is refused 400 memo_too_long before anything is spent. The wallet reads it from the fees call. 0 to 65536.",
    },
    Spec {
        key: "spend.max_batch",
        env: "FSP_SPEND_MAX_BATCH",
        default: "1024",
        units: "spends",
        what: "The most spends written and synced to disk together. A spend is answered only after its batch is synced. 1 to 65536.",
    },
    Spec {
        key: "esp.keyset_ttl_secs",
        env: "FSP_ESP_KEYSET_TTL_SECS",
        default: "3600",
        units: "seconds",
        what: "How long the ESP mint's current keyset, which new issue transfers name for their outputs, is used before it is read again (its keysets over AIR). It is also read again at once when the ESP mint answers keyset_retired (the cached set was deleted; a set that has only become the previous one is still signed with). If a read fails, the cached keyset is used; only with none cached is an issue refused 502 issuer_unavailable. 1 to 604800.",
    },
    Spec {
        key: "air.esp_did",
        env: "FSP_AIR_ESP_DID",
        default: "",
        units: "service DID (66 hex characters)",
        what: "The ESP mint's DID: the 33-byte compressed key of esp-mint's mint_did_key.json (`esp-mint pubkey` prints it), in hex. Where the requests to the ESP mint are sealed, and the key its replies must be signed by. Required by serve, with air.server and air.host: AIR is the only way fsp-mint reaches the ESP mint, and it has no built-in DID.",
    },
    Spec {
        key: "air.esp_server",
        env: "FSP_AIR_ESP_SERVER",
        default: "",
        units: "AIR server name (66 hex characters)",
        what: "The name of the AIR server that carries the requests to the ESP mint (the SPI's): its public key, the compressed hex AIR prints (66 characters, starting 02 or 03). fsp-mint checks the server's signed challenge against it. To be replaced by DID resolution: resolving a mint's DID to its AIR server is not built (ROADMAP upstream item 4), so it is configuration for now. With air.esp_host and air.esp_did, it is how the three calls to the ESP mint (mint info, keysets, transfer) are made: all three are required by serve. This is not the server this mint's own listener is on (air.server).",
    },
    Spec {
        key: "air.esp_host",
        env: "FSP_AIR_ESP_HOST",
        default: "",
        units: "host name or IP, no port",
        what: "Where that AIR server is: a host name or an IP with no scheme, no path and no port (the AIR client always dials port 5702 for ws). 127.0.0.1 for a server on this machine. To be replaced by DID resolution, like air.esp_server. Required by serve, with air.esp_server and air.esp_did.",
    },
    Spec {
        key: "air.server",
        env: "FSP_AIR_SERVER",
        default: "",
        units: "AIR server name (66 hex characters)",
        what: "The AIR server this mint's own listener holds reads on and writes replies to: its identity, a public key in AIR's compressed hex form (66 characters). The listener checks the server's signed challenge against it. Required by serve: the AIR listener always runs, because AIR is the only way wallets reach this mint. It is the AIR server of the service (it may be the same server as air.esp_server, or another: the DIDs differ, so the mailboxes do). To be replaced by DID resolution: a wallet is given this name beside the service's DID (its per-service `air.fsp` setting) until the AIR resolves a DID to a server (ROADMAP upstream item 4).",
    },
    Spec {
        key: "air.host",
        env: "FSP_AIR_HOST",
        default: "",
        units: "host name or IP, no port",
        what: "Where that AIR server is: a host name or an IP (127.0.0.1 for a server on this machine). No port: the AIR client always uses 5702 (ws). Required by serve, with air.server. To be replaced by DID resolution, like air.server.",
    },
    Spec {
        key: "air.held_reads",
        env: "FSP_AIR_HELD_READS",
        default: "16",
        units: "reads",
        what: "How many mailbox slots the listener holds a read on, ahead of the next unfinished one. More lets more wallets be answered at once without waiting for a slot to be reached; each held read costs the AIR server about 2 KB. The AIR server closes the whole session at its 257th held read, so the most accepted is 200. The requests themselves are answered one at a time, in slot order. 1 to 200.",
    },
    Spec {
        key: "air.reply_wait_secs",
        env: "FSP_AIR_REPLY_WAIT_SECS",
        default: "30",
        units: "seconds (1 to 600)",
        what: "How long to wait for the ESP mint's reply to one try of a request. After that the same request is written again, in the next free mailbox slot.",
    },
    Spec {
        key: "air.attempts",
        env: "FSP_AIR_ATTEMPTS",
        default: "3",
        units: "tries (1 to 10)",
        what: "How many tries one call makes before it gives up. A transfer that gets no reply leaves the issue pending (502 issuer_unavailable): its request is kept in issues.json, and sending the same issue request again carries on with it and picks up a late reply, without writing a second request.",
    },
    Spec {
        key: "air.max_skew_secs",
        env: "FSP_AIR_MAX_SKEW_SECS",
        default: "300",
        units: "seconds (1 to 86400)",
        what: "How far the reply's issued_at (the ESP mint's clock) may be from this mint's clock before the reply is refused as stale. A kept request answered later than this is dropped and the same transfer is sent in a new request (the ESP mint answers it from its stored answer).",
    },
    Spec {
        key: "air.timeout_secs",
        env: "FSP_AIR_TIMEOUT_SECS",
        default: "10",
        units: "seconds (1 to 600)",
        what: "How long the AIR server may take to answer one Create or Read before that step fails (the calls to the ESP mint and this mint's own listener alike).",
    },
    Spec {
        key: "storage.mint_state",
        env: "FSP_STORAGE_MINT_STATE",
        default: "",
        units: "service DIDs (66 hex characters)",
        what: "Which AIR services keep this mint's state (keys, spent lists, records), as an ordered list of service IDs: index 0 is the primary and the rest are secondaries, in the order they would be promoted. A service ID is the service's AIR DID, a 33-byte compressed key in hex (66 characters, starting 02 or 03); the old 32-byte x-only form is refused. In the environment the IDs are separated by commas; in the file it is an array of strings or one such string. One entry: that service holds the state and the mint keeps a local backup copy. Empty (the default): local only. At most 8, no duplicates. Setting only for now: nothing reads or writes the AIR yet (see DESIGN.md in sandbox-mint, \"AIR storage requirements\").",
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub network: String,
    pub bind: String,
    pub max_request_bytes: u64,
    pub esp_keyset_ttl_secs: u64,
    /// The ESP mint's DID, for the calls over AIR.
    pub air_esp_did: Option<ServiceDid>,
    /// The AIR server that carries the calls to the ESP mint (its name, a
    /// public key), or empty. To be replaced by DID resolution.
    pub air_esp_server: String,
    /// That server's host or IP, or empty.
    pub air_esp_host: String,
    /// This mint's own AIR server (the listener's), or empty. To be replaced by
    /// DID resolution.
    pub air_server: String,
    /// That server's host or IP, or empty.
    pub air_host: String,
    /// Mailbox slots the listener holds a read on.
    pub air_held_reads: u32,
    pub air_reply_wait_secs: u64,
    pub air_attempts: u64,
    pub air_max_skew_secs: u64,
    pub air_timeout_secs: u64,
    pub service_did: String,
    pub issue_fee_per_byte: u64,
    pub rotation_activation_hours: u64,
    pub rotation_period_hours: u64,
    pub spend_max_batch: u64,
    pub issue_max_items: u64,
    pub spend_max_items: u64,
    pub spend_max_memo_bytes: u64,
    /// Which AIR services keep this mint's state (setting only for now).
    pub storage_mint_state: StoragePolicy,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            network: "main".into(),
            bind: "127.0.0.1:3200".into(),
            max_request_bytes: 1024 * 1024,
            esp_keyset_ttl_secs: 3600,
            air_esp_did: None,
            air_esp_server: String::new(),
            air_esp_host: String::new(),
            air_server: String::new(),
            air_host: String::new(),
            air_held_reads: 16,
            air_reply_wait_secs: 30,
            air_attempts: 3,
            air_max_skew_secs: 300,
            air_timeout_secs: 10,
            service_did: String::new(),
            issue_fee_per_byte: 0,
            rotation_activation_hours: 8760,
            rotation_period_hours: 8760,
            spend_max_batch: 1024,
            issue_max_items: DEFAULT_MAX_ITEMS as u64,
            spend_max_items: DEFAULT_MAX_ITEMS as u64,
            spend_max_memo_bytes: sp_wire::fsp::MAX_MEMO_BYTES as u64,
            storage_mint_state: StoragePolicy::default(),
        }
    }
}

pub fn network_name(value: &str) -> Result<&'static str, String> {
    match value.trim() {
        "main" | "mainnet" => Ok("main"),
        "regtest" => Ok("regtest"),
        "signet" => Ok("signet"),
        "testnet" => Ok("testnet"),
        other => Err(format!(
            "`{other}` is not a network: use main, regtest, signet or testnet"
        )),
    }
}

fn whole(key: &str, value: &str) -> Result<u64, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("{key} = `{value}` is not a whole number (no sign, no decimals)"))
}

fn in_range(key: &str, value: &str, min: u64, max: u64) -> Result<u64, String> {
    match value.trim().parse::<u64>() {
        Ok(n) if (min..=max).contains(&n) => Ok(n),
        _ => Err(format!(
            "{key} = `{value}` must be a whole number from {min} to {max}"
        )),
    }
}

impl Config {
    pub fn get(&self, key: &str) -> Option<String> {
        Some(match key {
            "server.network" => self.network.clone(),
            "server.bind" => self.bind.clone(),
            "server.max_request_bytes" => self.max_request_bytes.to_string(),
            "esp.keyset_ttl_secs" => self.esp_keyset_ttl_secs.to_string(),
            "air.esp_did" => self.air_esp_did.map(|d| d.to_hex()).unwrap_or_default(),
            "air.esp_server" => self.air_esp_server.clone(),
            "air.esp_host" => self.air_esp_host.clone(),
            "air.server" => self.air_server.clone(),
            "air.host" => self.air_host.clone(),
            "air.held_reads" => self.air_held_reads.to_string(),
            "air.reply_wait_secs" => self.air_reply_wait_secs.to_string(),
            "air.attempts" => self.air_attempts.to_string(),
            "air.max_skew_secs" => self.air_max_skew_secs.to_string(),
            "air.timeout_secs" => self.air_timeout_secs.to_string(),
            "service.did" => self.service_did.clone(),
            "fees.issue_fee_per_byte" => self.issue_fee_per_byte.to_string(),
            "rotation.activation_hours" => self.rotation_activation_hours.to_string(),
            "rotation.period_hours" => self.rotation_period_hours.to_string(),
            "spend.max_batch" => self.spend_max_batch.to_string(),
            "issue.max_items" => self.issue_max_items.to_string(),
            "spend.max_items" => self.spend_max_items.to_string(),
            "spend.max_memo_bytes" => self.spend_max_memo_bytes.to_string(),
            "storage.mint_state" => self.storage_mint_state.to_config_string(),
            _ => return None,
        })
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "server.network" => self.network = network_name(value)?.to_string(),
            "server.bind" => self.bind = value.trim().to_string(),
            "server.max_request_bytes" => self.max_request_bytes = whole(key, value)?,
            "esp.keyset_ttl_secs" => self.esp_keyset_ttl_secs = whole(key, value)?,
            "air.esp_did" => {
                let text = value.trim();
                self.air_esp_did = if text.is_empty() {
                    None
                } else {
                    Some(ServiceDid::parse(text).map_err(|e| format!("{key} = `{value}`: {e}"))?)
                };
            }
            "air.esp_server" | "air.server" => {
                let text = value.trim();
                if !text.is_empty() {
                    sp_wire::seal::parse_public_key(text).map_err(|e| {
                        format!(
                            "{key} = `{value}` is not an AIR server name (a public key, 66 hex characters): {e}"
                        )
                    })?;
                }
                if key == "air.server" {
                    self.air_server = text.to_string();
                } else {
                    self.air_esp_server = text.to_string();
                }
            }
            "air.esp_host" | "air.host" => {
                let text = value.trim();
                if let Some(why) = sp_wire::air_request::host_problem(text) {
                    return Err(format!("{key} = `{value}` {why}"));
                }
                if key == "air.host" {
                    self.air_host = text.to_string();
                } else {
                    self.air_esp_host = text.to_string();
                }
            }
            "air.held_reads" => self.air_held_reads = in_range(key, value, 1, 200)? as u32,
            "air.reply_wait_secs" => self.air_reply_wait_secs = in_range(key, value, 1, 600)?,
            "air.attempts" => self.air_attempts = in_range(key, value, 1, 10)?,
            "air.max_skew_secs" => self.air_max_skew_secs = in_range(key, value, 1, 86_400)?,
            "air.timeout_secs" => self.air_timeout_secs = in_range(key, value, 1, 600)?,
            "service.did" => self.service_did = value.trim().to_lowercase(),
            "fees.issue_fee_per_byte" => self.issue_fee_per_byte = whole(key, value)?,
            "rotation.activation_hours" => self.rotation_activation_hours = whole(key, value)?,
            "rotation.period_hours" => self.rotation_period_hours = whole(key, value)?,
            "spend.max_batch" => self.spend_max_batch = whole(key, value)?,
            "issue.max_items" => self.issue_max_items = whole(key, value)?,
            "spend.max_items" => self.spend_max_items = whole(key, value)?,
            "spend.max_memo_bytes" => self.spend_max_memo_bytes = whole(key, value)?,
            "storage.mint_state" => {
                self.storage_mint_state =
                    StoragePolicy::parse(value).map_err(|e| format!("{key} = `{value}`: {e}"))?
            }
            _ => return Err(format!("unknown setting `{key}`")),
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut p = Vec::new();
        match self.bind.parse::<std::net::SocketAddr>() {
            Err(_) => p.push(format!("server.bind = `{}` must be an IP:port", self.bind)),
            Ok(addr) if !addr.ip().is_loopback() => p.push(format!(
                "server.bind = `{}` is not a loopback address. Wallets reach this mint over AIR only; what is left on this address (/audit) is for the operator on this machine",
                self.bind
            )),
            Ok(_) => {}
        }
        if !(4096..=1 << 30).contains(&self.max_request_bytes) {
            p.push(format!(
                "server.max_request_bytes = {} must be 4096 to 1073741824 bytes",
                self.max_request_bytes
            ));
        }
        if !(1..=604_800).contains(&self.esp_keyset_ttl_secs) {
            p.push(format!(
                "esp.keyset_ttl_secs = {} must be 1 to 604800 seconds",
                self.esp_keyset_ttl_secs
            ));
        }
        if !self.service_did.is_empty() {
            if let Err(e) = ServiceDid::parse(&self.service_did) {
                p.push(format!("service.did: {e}"));
            }
        }
        if self.issue_fee_per_byte > 1_000_000_000_000 {
            p.push(format!(
                "fees.issue_fee_per_byte = {} must be 0 to 10^12",
                self.issue_fee_per_byte
            ));
        }
        if self.rotation_activation_hours > 876_000 {
            p.push(format!(
                "rotation.activation_hours = {} must be 0 to 876000",
                self.rotation_activation_hours
            ));
        }
        if !(1..=876_000).contains(&self.rotation_period_hours) {
            p.push(format!(
                "rotation.period_hours = {} must be 1 to 876000",
                self.rotation_period_hours
            ));
        }
        if !(1..=65536).contains(&self.spend_max_batch) {
            p.push(format!(
                "spend.max_batch = {} must be 1 to 65536",
                self.spend_max_batch
            ));
        }
        if !(1..=100_000).contains(&self.issue_max_items) {
            p.push(format!(
                "issue.max_items = {} must be 1 to 100000",
                self.issue_max_items
            ));
        }
        if !(1..=100_000).contains(&self.spend_max_items) {
            p.push(format!(
                "spend.max_items = {} must be 1 to 100000",
                self.spend_max_items
            ));
        }
        if self.spend_max_memo_bytes > 65_536 {
            p.push(format!(
                "spend.max_memo_bytes = {} must be 0 to 65536",
                self.spend_max_memo_bytes
            ));
        }
        if p.is_empty() { Ok(()) } else { Err(p) }
    }

    /// What `serve` needs: all five AIR settings. The ESP mint is reached over
    /// AIR only (`air.esp_did`, `air.esp_server`, `air.esp_host`), and the
    /// listener, which always runs because AIR is the only way wallets reach this
    /// mint, needs its own server (`air.server`, `air.host`). The error names every
    /// one that is not set. (The other commands do not talk to the AIR and do not
    /// need them.)
    pub fn require_air(&self) -> Result<(), String> {
        let mut missing = Vec::new();
        if self.air_esp_did.is_none() {
            missing.push("air.esp_did (FSP_AIR_ESP_DID)");
        }
        if self.air_esp_server.is_empty() {
            missing.push("air.esp_server (FSP_AIR_ESP_SERVER)");
        }
        if self.air_esp_host.is_empty() {
            missing.push("air.esp_host (FSP_AIR_ESP_HOST)");
        }
        if self.air_server.is_empty() {
            missing.push("air.server (FSP_AIR_SERVER)");
        }
        if self.air_host.is_empty() {
            missing.push("air.host (FSP_AIR_HOST)");
        }
        if missing.is_empty() {
            return Ok(());
        }
        Err(format!(
            "serve needs the AIR settings, because AIR is the only way fsp-mint reaches the ESP mint and the only way wallets reach this mint, and {} {} not set. air.esp_did is the ESP mint's DID (`esp-mint pubkey` prints it), air.esp_server and air.esp_host the AIR server that carries the calls to it, and air.server and air.host the AIR server this mint's own listener is on (each a public key of 66 hex characters, and a host name or IP with no port)",
            missing.join(", "),
            if missing.len() == 1 { "is" } else { "are" }
        ))
    }

    /// The listener's settings (call [`Config::require_air`] first). `state_dir`
    /// is the network's data folder.
    pub fn air_listener(&self, state_dir: &Path) -> crate::air_listener::Settings {
        crate::air_listener::Settings {
            server: sp_wire::air_request::client::ServerRef {
                name: self.air_server.clone(),
                host: self.air_host.clone(),
            },
            held_reads: self.air_held_reads,
            timeout_secs: self.air_timeout_secs,
            state_dir: state_dir.to_path_buf(),
        }
    }

    /// The service's DID, if set and valid.
    pub fn service(&self) -> Option<ServiceDid> {
        ServiceDid::parse(&self.service_did).ok()
    }

    /// The rotation settings. The first issue comes from the data folder
    /// (`Spender::open` fills it in).
    pub fn rotation(&self) -> crate::keysets::Rotation {
        crate::keysets::Rotation {
            first_issue: None,
            activation_secs: self.rotation_activation_hours * 3600,
            period_secs: self.rotation_period_hours * 3600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Default,
    File,
    Env(&'static str),
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Default => write!(f, "default"),
            Source::File => write!(f, "file"),
            Source::Env(n) => write!(f, "env {n}"),
        }
    }
}

#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub sources: BTreeMap<&'static str, Source>,
    pub file: Option<PathBuf>,
}

impl Loaded {
    pub fn describe(&self) -> Vec<String> {
        SPECS
            .iter()
            .map(|s| {
                let src = self.sources.get(s.key).cloned().unwrap_or(Source::Default);
                format!(
                    "  {} = {} {} ({src})",
                    s.key,
                    self.config.get(s.key).unwrap_or_default(),
                    s.units
                )
            })
            .collect()
    }
}

fn parse_file(text: &str) -> Result<Vec<(String, String)>, Vec<String>> {
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| vec![format!("not valid TOML: {}", e.message())])?;
    let (mut pairs, mut problems) = (Vec::new(), Vec::new());
    for (section, body) in &table {
        let Some(body) = body.as_table() else {
            problems.push(format!(
                "`{section}` must be a [section] of settings, not a single value"
            ));
            continue;
        };
        for (name, value) in body {
            let key = format!("{section}.{name}");
            if let Some((_, _, why)) = REMOVED.iter().find(|(k, _, _)| *k == key) {
                problems.push((*why).to_string());
                continue;
            }
            if !SPECS.iter().any(|s| s.key == key) {
                problems.push(format!("unknown setting `{key}`"));
                continue;
            }
            match value {
                toml::Value::Integer(n) => pairs.push((key, n.to_string())),
                toml::Value::String(s) => pairs.push((key, s.clone())),
                toml::Value::Array(items) if key.starts_with("storage.") => {
                    let ids: Vec<&str> = items.iter().filter_map(|v| v.as_str()).collect();
                    if ids.len() == items.len() {
                        pairs.push((key, ids.join(",")));
                    } else {
                        problems.push(format!(
                            "{key} must be a list of strings (service IDs), not {items:?}"
                        ));
                    }
                }
                _ => problems.push(format!(
                    "{key} must be a whole number or a string, not {}",
                    value.type_str()
                )),
            }
        }
    }
    if problems.is_empty() {
        Ok(pairs)
    } else {
        Err(problems)
    }
}

pub fn load(data_dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Loaded, Vec<String>> {
    let (path, must_exist) = match env(FILE_ENV) {
        Some(p) if !p.is_empty() => (PathBuf::from(p), true),
        _ => (data_dir.join(FILE_NAME), false),
    };
    let mut config = Config::default();
    let mut sources = BTreeMap::new();
    let mut problems = Vec::new();
    let mut file = None;
    match fs::read_to_string(&path) {
        Ok(text) => {
            file = Some(path.clone());
            match parse_file(&text) {
                Ok(pairs) => {
                    for (key, value) in pairs {
                        match config.set(&key, &value) {
                            Ok(()) => {
                                if let Some(s) = SPECS.iter().find(|s| s.key == key) {
                                    sources.insert(s.key, Source::File);
                                }
                            }
                            Err(e) => problems.push(format!("{}: {e}", path.display())),
                        }
                    }
                }
                Err(list) => {
                    problems.extend(list.into_iter().map(|p| format!("{}: {p}", path.display())))
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound && !must_exist => {}
        Err(e) => problems.push(format!("cannot read config file {}: {e}", path.display())),
    }
    for (_, name, why) in REMOVED {
        if env(name).is_some_and(|v| !v.is_empty()) {
            problems.push(format!("environment variable {name}: {why}"));
        }
    }
    for spec in SPECS {
        if let Some(value) = env(spec.env).filter(|v| !v.is_empty()) {
            match config.set(spec.key, &value) {
                Ok(()) => {
                    sources.insert(spec.key, Source::Env(spec.env));
                }
                Err(e) => problems.push(format!("environment variable {}: {e}", spec.env)),
            }
        }
    }
    if problems.is_empty() {
        if let Err(l) = config.validate() {
            problems.extend(l);
        }
    }
    if problems.is_empty() {
        Ok(Loaded {
            config,
            sources,
            file,
        })
    } else {
        Err(problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fsp-config-{name}-{}", {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            format!(
                "{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        }));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn the_storage_policy_comes_from_the_file_and_the_environment() {
        const A: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        const B: &str = "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5";
        let dir = dir("storage");
        let first = load(&dir, &no_env).unwrap();
        assert!(first.config.storage_mint_state.is_local_only());
        assert_eq!(first.config.get("storage.mint_state").as_deref(), Some(""));

        fs::write(
            dir.join(FILE_NAME),
            format!("[storage]\nmint_state = [\"{A}\", \"{B}\"]\n"),
        )
        .unwrap();
        let l = load(&dir, &no_env).unwrap();
        let p = &l.config.storage_mint_state;
        assert_eq!(p.primary().unwrap().to_hex(), A);
        assert_eq!(p.secondaries().len(), 1);
        assert_eq!(l.sources["storage.mint_state"], Source::File);
        assert_eq!(
            l.config.get("storage.mint_state").unwrap(),
            format!("{A},{B}")
        );

        // The environment wins over the file. One entry is valid.
        let l = load(&dir, &env_of(&[("FSP_STORAGE_MINT_STATE", A)])).unwrap();
        assert_eq!(l.config.storage_mint_state.to_config_string(), A);
        assert!(l.config.storage_mint_state.secondaries().is_empty());
        assert_eq!(
            l.sources["storage.mint_state"],
            Source::Env("FSP_STORAGE_MINT_STATE")
        );

        // One string in the file works too, and so does an empty list.
        fs::write(
            dir.join(FILE_NAME),
            format!("[storage]\nmint_state = \"{B}, {A}\"\n"),
        )
        .unwrap();
        let l = load(&dir, &no_env).unwrap();
        assert_eq!(l.config.storage_mint_state.primary().unwrap().to_hex(), B);
        fs::write(dir.join(FILE_NAME), "[storage]\nmint_state = []\n").unwrap();
        let l = load(&dir, &no_env).unwrap();
        assert!(l.config.storage_mint_state.is_local_only());

        // Every kind of mistake is refused, naming the variable or the setting.
        let twice = format!("{A},{A}");
        let p = load(&dir, &env_of(&[("FSP_STORAGE_MINT_STATE", twice.as_str())])).unwrap_err();
        assert!(
            p.iter()
                .any(|x| x.contains("FSP_STORAGE_MINT_STATE") && x.contains("listed twice")),
            "{p:?}"
        );
        let gap = format!("{A},,{B}");
        let p = load(&dir, &env_of(&[("FSP_STORAGE_MINT_STATE", gap.as_str())])).unwrap_err();
        assert!(p.iter().any(|x| x.contains("entry 2")), "{p:?}");
        let p = load(&dir, &env_of(&[("FSP_STORAGE_MINT_STATE", "not-a-key")])).unwrap_err();
        assert!(p.iter().any(|x| x.contains("must be hex")), "{p:?}");
        fs::write(dir.join(FILE_NAME), "[storage]\nmint_state = [1]\n").unwrap();
        let p = load(&dir, &no_env).unwrap_err();
        assert!(p.iter().any(|x| x.contains("list of strings")), "{p:?}");
        fs::write(dir.join(FILE_NAME), "[storage]\nwallet = \"x\"\n").unwrap();
        let p = load(&dir, &no_env).unwrap_err();
        assert!(
            p.iter()
                .any(|x| x.contains("unknown setting `storage.wallet`")),
            "{p:?}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn defaults_are_valid_and_every_spec_round_trips() {
        let c = Config::default();
        c.validate().unwrap();
        for s in SPECS {
            assert_eq!(c.get(s.key).as_deref(), Some(s.default), "{}", s.key);
            let mut d = Config::default();
            d.set(s.key, s.default).unwrap();
            assert_eq!(d, c);
        }
    }

    #[test]
    fn service_rotation_and_batch_are_checked() {
        let mut c = Config::default();
        c.set(
            "service.did",
            "0279BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798",
        )
        .unwrap();
        c.validate().unwrap();
        assert!(c.service().is_some());
        // The 32-byte x-only form of wire version 2 is refused, and says why.
        let mut old = Config::default();
        old.set(
            "service.did",
            "79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798",
        )
        .unwrap();
        let errors = old.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("x-only")), "{errors:?}");
        assert!(old.service().is_none());
        assert_eq!(c.rotation().activation_secs, 8760 * 3600);
        assert_eq!(c.rotation().period_secs, 8760 * 3600);
        c.set("rotation.activation_hours", "0").unwrap();
        c.validate().unwrap();
        assert_eq!(c.rotation().activation(), None, "0 is off");
        c.set("rotation.activation_hours", "876001").unwrap();
        c.set("rotation.period_hours", "0").unwrap();
        c.set("spend.max_batch", "0").unwrap();
        c.set("service.did", "abcd").unwrap();
        assert_eq!(c.validate().unwrap_err().len(), 4);
    }

    #[test]
    fn the_note_limits_come_from_the_settings() {
        let d = dir("items");
        fs::write(d.join(FILE_NAME), "[issue]\nmax_items = 50\n").unwrap();
        let env = |n: &str| (n == "FSP_SPEND_MAX_ITEMS").then(|| "20".to_string());
        let l = load(&d, &env).unwrap();
        assert_eq!(l.config.issue_max_items, 50);
        assert_eq!(l.config.spend_max_items, 20);
        assert_eq!(Config::default().spend_max_memo_bytes, 256);
        let memo = |n: &str| (n == "FSP_SPEND_MAX_MEMO_BYTES").then(|| "1024".to_string());
        assert_eq!(load(&d, &memo).unwrap().config.spend_max_memo_bytes, 1024);
        assert_eq!(l.sources["issue.max_items"], Source::File);
        assert_eq!(
            l.sources["spend.max_items"],
            Source::Env("FSP_SPEND_MAX_ITEMS")
        );
        assert_eq!(
            Config::default().issue_max_items,
            DEFAULT_MAX_ITEMS as u64,
            "the default is only a default"
        );
        let mut c = Config::default();
        c.set("issue.max_items", "0").unwrap();
        c.set("spend.max_items", "100001").unwrap();
        c.set("spend.max_memo_bytes", "65537").unwrap();
        assert_eq!(c.validate().unwrap_err().len(), 3);
        fs::remove_dir_all(&d).unwrap();
    }

    /// A DID that parses: the curve's generator, even.
    const ESP_DID: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    #[test]
    fn file_then_env_with_sources() {
        let d = dir("order");
        fs::write(
            d.join(FILE_NAME),
            "[air]\nhost = \"10.0.0.1\"\nattempts = 5\n",
        )
        .unwrap();
        let env = |n: &str| (n == "FSP_AIR_ATTEMPTS").then(|| "7".to_string());
        let l = load(&d, &env).unwrap();
        assert_eq!(l.config.air_host, "10.0.0.1");
        assert_eq!(l.config.air_attempts, 7);
        assert_eq!(l.sources["air.host"], Source::File);
        assert_eq!(l.sources["air.attempts"], Source::Env("FSP_AIR_ATTEMPTS"));
        assert!(
            l.describe()
                .iter()
                .any(|x| x == "  air.attempts = 7 tries (1 to 10) (env FSP_AIR_ATTEMPTS)")
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_air_settings_have_defaults_and_are_checked() {
        let c = Config::default();
        assert_eq!(
            (
                c.air_reply_wait_secs,
                c.air_attempts,
                c.air_max_skew_secs,
                c.air_timeout_secs
            ),
            (30, 3, 300, 10)
        );
        assert!(
            c.air_esp_did.is_none() && c.air_esp_server.is_empty() && c.air_esp_host.is_empty()
        );
        assert!(c.air_server.is_empty() && c.air_host.is_empty());
        assert_eq!(c.air_held_reads, 16);
        let mut c = Config::default();
        c.set("air.esp_did", ESP_DID).unwrap();
        assert_eq!(c.get("air.esp_did").unwrap(), ESP_DID);
        for (key, value) in [
            ("air.esp_did", "zz"),
            ("air.esp_did", &ESP_DID[2..]),
            ("air.esp_server", "zz"),
            ("air.esp_host", "ws://127.0.0.1"),
            ("air.server", "zz"),
            ("air.host", "ws://127.0.0.1"),
            ("air.host", "127.0.0.1:5702"),
            ("air.host", "a b"),
            ("air.held_reads", "0"),
            ("air.held_reads", "201"),
            ("air.attempts", "0"),
            ("air.attempts", "11"),
            ("air.reply_wait_secs", "0"),
            ("air.reply_wait_secs", "soon"),
            ("air.max_skew_secs", "86401"),
            ("air.timeout_secs", "601"),
        ] {
            let e = Config::default().set(key, value).unwrap_err();
            assert!(e.contains(key), "{key} = {value}: {e}");
        }
    }

    #[test]
    fn serve_needs_all_five_air_settings_and_names_each_missing_one() {
        let c = Config::default();
        let e = c.require_air().unwrap_err();
        for name in [
            "air.esp_did",
            "air.esp_server",
            "air.esp_host",
            "air.server",
            "air.host",
        ] {
            assert!(e.contains(name), "{e}");
        }
        let mut c = Config::default();
        c.set("air.esp_did", ESP_DID).unwrap();
        c.set("air.esp_host", "127.0.0.1").unwrap();
        c.set("air.server", ESP_DID).unwrap();
        c.set("air.host", "127.0.0.1").unwrap();
        let e = c.require_air().unwrap_err();
        assert!(e.contains("FSP_AIR_ESP_SERVER"), "{e}");
        assert!(
            !e.contains("FSP_AIR_ESP_HOST")
                && !e.contains("FSP_AIR_ESP_DID")
                && !e.contains("(FSP_AIR_SERVER)")
                && !e.contains("(FSP_AIR_HOST)"),
            "{e}"
        );
        assert!(e.contains("is not set"), "{e}");
        c.set("air.esp_server", ESP_DID).unwrap();
        assert!(c.require_air().is_ok());
        // The listener's own server and host are the ones the listener uses.
        let l = c.air_listener(std::path::Path::new("/x"));
        assert_eq!((l.server.host.as_str(), l.held_reads), ("127.0.0.1", 16));
        c.set("air.esp_host", "10.0.0.9").unwrap();
        assert_eq!(
            c.air_listener(std::path::Path::new("/x")).server.host,
            "127.0.0.1"
        );
    }

    #[test]
    fn server_bind_must_be_a_loopback_address() {
        let mut c = Config::default();
        c.set("server.bind", "0.0.0.0:3200").unwrap();
        let p = c.validate().unwrap_err();
        assert!(
            p.iter()
                .any(|x| x.contains("server.bind") && x.contains("not a loopback address")),
            "{p:?}"
        );
        c.set("server.bind", "[::1]:3200").unwrap();
        assert!(c.validate().is_ok());
        c.set("server.bind", "127.0.0.1:3200").unwrap();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn the_removed_esp_settings_get_a_clear_error() {
        let d = dir("removed");
        fs::write(
            d.join(FILE_NAME),
            "[esp]\nurl = \"http://127.0.0.1:3100\"\ntimeout_secs = 5\nkeyset_ttl_secs = 60\n",
        )
        .unwrap();
        let p = load(&d, &no_env).unwrap_err();
        assert!(
            p.iter().any(|x| x.contains("`esp.url` is gone")
                && x.contains("air.esp_did")
                && x.contains("air.esp_server")),
            "{p:?}"
        );
        assert!(
            p.iter()
                .any(|x| x.contains("`esp.timeout_secs` is gone") && x.contains("air.attempts")),
            "{p:?}"
        );
        assert!(
            !p.iter().any(|x| x.contains("unknown setting")),
            "they are not reported as unknown: {p:?}"
        );
        // esp.keyset_ttl_secs still means something (the cache of the ESP mint's keyset).
        assert!(!p.iter().any(|x| x.contains("keyset_ttl_secs")), "{p:?}");
        fs::remove_dir_all(&d).unwrap();
        // The same from the environment.
        let env =
            |n: &str| matches!(n, "FSP_ESP_URL" | "FSP_ESP_TIMEOUT_SECS").then(|| "x".to_string());
        let p = load(&dir("removed-env"), &env).unwrap_err();
        assert!(
            p.iter()
                .any(|x| x.contains("environment variable FSP_ESP_URL") && x.contains("is gone")),
            "{p:?}"
        );
        assert!(
            p.iter()
                .any(|x| x.contains("environment variable FSP_ESP_TIMEOUT_SECS")),
            "{p:?}"
        );
    }

    #[test]
    fn every_problem_is_listed() {
        let d = dir("bad");
        fs::write(
            d.join(FILE_NAME),
            "[server]\nbind = \"nowhere\"\nnope = 1\n",
        )
        .unwrap();
        let env = |n: &str| (n == "FSP_AIR_ATTEMPTS").then(|| "x".to_string());
        let p = load(&d, &env).unwrap_err();
        assert!(
            p.iter()
                .any(|x| x.contains("unknown setting `server.nope`")),
            "{p:?}"
        );
        let env = |n: &str| (n == "FSP_NETWORK").then(|| "moon".to_string());
        assert!(load(&dir("net"), &env).unwrap_err()[0].contains("not a network"));
        assert!(p.iter().any(|x| x.contains("FSP_AIR_ATTEMPTS")), "{p:?}");
        let env = |n: &str| (n == "FSP_MAX_REQUEST_BYTES").then(|| "10".to_string());
        assert!(load(&dir("range"), &env).unwrap_err()[0].contains("4096"));
        let env = |n: &str| (n == FILE_ENV).then(|| "/nonexistent/fsp.toml".to_string());
        assert!(load(&d, &env).is_err());
        fs::remove_dir_all(&d).unwrap();
    }
}
