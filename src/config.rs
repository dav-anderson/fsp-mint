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

pub const FILE_NAME: &str = "fsp-mint.toml";
pub const FILE_ENV: &str = "FSP_CONFIG";

/// The default for `issue.max_items` and `spend.max_items`. It is only a
/// default: the limits in force come from the settings (and are served on
/// `GET /fees`), never from this constant.
pub const DEFAULT_MAX_ITEMS: usize = 1000;

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
        what: "The Bitcoin network of the ESP mint this FSP mint takes ESP from: main, regtest, signet or testnet. To be checked against the ESP mint's /health.",
    },
    Spec {
        key: "server.bind",
        env: "FSP_BIND",
        default: "127.0.0.1:3200",
        units: "host:port",
        what: "The HTTP API's address. Must be an IP:port.",
    },
    Spec {
        key: "server.max_request_bytes",
        env: "FSP_MAX_REQUEST_BYTES",
        default: "1048576",
        units: "bytes",
        what: "Ceiling on a request body, as on the other mints. 4096 to 1 GiB.",
    },
    Spec {
        key: "service.did",
        env: "FSP_SERVICE_DID",
        default: "",
        units: "x-only key",
        what: "The service's DID public key (x-only, 64 hex). Only ESP named for it is accepted. The secret half is service_key.hex in the data folder (fsp-mint service-key makes it) and must match. Required by serve.",
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
        what: "The most groups in one issue request, and the most ESP notes and the most FSP outputs in one group. More is refused 400 too_many_items before anything is spent. The wallet reads it from GET /fees. 1 to 100000.",
    },
    Spec {
        key: "spend.max_items",
        env: "FSP_SPEND_MAX_ITEMS",
        default: "1000",
        units: "notes",
        what: "The most notes in, and the most outputs, in one spend. More is refused 400 too_many_items. The wallet reads it from GET /fees. 1 to 100000.",
    },
    Spec {
        key: "spend.max_memo_bytes",
        env: "FSP_SPEND_MAX_MEMO_BYTES",
        default: "256",
        units: "bytes",
        what: "The longest memo on a spend. A longer one is refused 400 memo_too_long before anything is spent. The wallet reads it from GET /fees. 0 to 65536.",
    },
    Spec {
        key: "spend.max_batch",
        env: "FSP_SPEND_MAX_BATCH",
        default: "1024",
        units: "spends",
        what: "The most spends written and synced to disk together. A spend is answered only after its batch is synced. 1 to 65536.",
    },
    Spec {
        key: "esp.url",
        env: "FSP_ESP_URL",
        default: "http://127.0.0.1:3100",
        units: "URL",
        what: "The ESP mint (esp-mint serve) where accepted ESP is transferred to notes the service controls.",
    },
    Spec {
        key: "esp.timeout_secs",
        env: "FSP_ESP_TIMEOUT_SECS",
        default: "30",
        units: "seconds",
        what: "How long one call to the ESP mint may take. 1 to 600.",
    },
    Spec {
        key: "esp.keyset_ttl_secs",
        env: "FSP_ESP_KEYSET_TTL_SECS",
        default: "3600",
        units: "seconds",
        what: "How long the ESP mint's current keyset, which new issue transfers name for their outputs, is used before it is read again (GET /keysets). It is also read again at once when the ESP mint answers keyset_retired (the cached set was deleted; a set that has only become the previous one is still signed with). If a read fails, the cached keyset is used; only with none cached is an issue refused 502 issuer_unavailable. 1 to 604800.",
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub network: String,
    pub bind: String,
    pub max_request_bytes: u64,
    pub esp_url: String,
    pub esp_timeout_secs: u64,
    pub esp_keyset_ttl_secs: u64,
    pub service_did: String,
    pub issue_fee_per_byte: u64,
    pub rotation_activation_hours: u64,
    pub rotation_period_hours: u64,
    pub spend_max_batch: u64,
    pub issue_max_items: u64,
    pub spend_max_items: u64,
    pub spend_max_memo_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            network: "main".into(),
            bind: "127.0.0.1:3200".into(),
            max_request_bytes: 1024 * 1024,
            esp_url: "http://127.0.0.1:3100".into(),
            esp_timeout_secs: 30,
            esp_keyset_ttl_secs: 3600,
            service_did: String::new(),
            issue_fee_per_byte: 0,
            rotation_activation_hours: 8760,
            rotation_period_hours: 8760,
            spend_max_batch: 1024,
            issue_max_items: DEFAULT_MAX_ITEMS as u64,
            spend_max_items: DEFAULT_MAX_ITEMS as u64,
            spend_max_memo_bytes: sp_wire::fsp::MAX_MEMO_BYTES as u64,
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

impl Config {
    pub fn get(&self, key: &str) -> Option<String> {
        Some(match key {
            "server.network" => self.network.clone(),
            "server.bind" => self.bind.clone(),
            "server.max_request_bytes" => self.max_request_bytes.to_string(),
            "esp.url" => self.esp_url.clone(),
            "esp.timeout_secs" => self.esp_timeout_secs.to_string(),
            "esp.keyset_ttl_secs" => self.esp_keyset_ttl_secs.to_string(),
            "service.did" => self.service_did.clone(),
            "fees.issue_fee_per_byte" => self.issue_fee_per_byte.to_string(),
            "rotation.activation_hours" => self.rotation_activation_hours.to_string(),
            "rotation.period_hours" => self.rotation_period_hours.to_string(),
            "spend.max_batch" => self.spend_max_batch.to_string(),
            "issue.max_items" => self.issue_max_items.to_string(),
            "spend.max_items" => self.spend_max_items.to_string(),
            "spend.max_memo_bytes" => self.spend_max_memo_bytes.to_string(),
            _ => return None,
        })
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "server.network" => self.network = network_name(value)?.to_string(),
            "server.bind" => self.bind = value.trim().to_string(),
            "server.max_request_bytes" => self.max_request_bytes = whole(key, value)?,
            "esp.url" => self.esp_url = value.trim().to_string(),
            "esp.timeout_secs" => self.esp_timeout_secs = whole(key, value)?,
            "esp.keyset_ttl_secs" => self.esp_keyset_ttl_secs = whole(key, value)?,
            "service.did" => self.service_did = value.trim().to_lowercase(),
            "fees.issue_fee_per_byte" => self.issue_fee_per_byte = whole(key, value)?,
            "rotation.activation_hours" => self.rotation_activation_hours = whole(key, value)?,
            "rotation.period_hours" => self.rotation_period_hours = whole(key, value)?,
            "spend.max_batch" => self.spend_max_batch = whole(key, value)?,
            "issue.max_items" => self.issue_max_items = whole(key, value)?,
            "spend.max_items" => self.spend_max_items = whole(key, value)?,
            "spend.max_memo_bytes" => self.spend_max_memo_bytes = whole(key, value)?,
            _ => return Err(format!("unknown setting `{key}`")),
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut p = Vec::new();
        if self.bind.parse::<std::net::SocketAddr>().is_err() {
            p.push(format!("server.bind = `{}` must be an IP:port", self.bind));
        }
        if !(4096..=1 << 30).contains(&self.max_request_bytes) {
            p.push(format!(
                "server.max_request_bytes = {} must be 4096 to 1073741824 bytes",
                self.max_request_bytes
            ));
        }
        if !(self.esp_url.starts_with("http://") || self.esp_url.starts_with("https://")) {
            p.push(format!(
                "esp.url = `{}` must start with http:// or https://",
                self.esp_url
            ));
        }
        if !(1..=600).contains(&self.esp_timeout_secs) {
            p.push(format!(
                "esp.timeout_secs = {} must be 1 to 600 seconds",
                self.esp_timeout_secs
            ));
        }
        if !(1..=604_800).contains(&self.esp_keyset_ttl_secs) {
            p.push(format!(
                "esp.keyset_ttl_secs = {} must be 1 to 604800 seconds",
                self.esp_keyset_ttl_secs
            ));
        }
        if !self.service_did.is_empty() && self.service().is_none() {
            p.push(format!(
                "service.did = `{}` must be a 32-byte x-only key in hex",
                self.service_did
            ));
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

    /// The service's DID key, if set and valid.
    pub fn service(&self) -> Option<secp256k1::XOnlyPublicKey> {
        let bytes = hex::decode(&self.service_did).ok()?;
        secp256k1::XOnlyPublicKey::from_slice(&bytes).ok()
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
            if !SPECS.iter().any(|s| s.key == key) {
                problems.push(format!("unknown setting `{key}`"));
                continue;
            }
            match value {
                toml::Value::Integer(n) => pairs.push((key, n.to_string())),
                toml::Value::String(s) => pairs.push((key, s.clone())),
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
            "79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798",
        )
        .unwrap();
        c.validate().unwrap();
        assert!(c.service().is_some());
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

    #[test]
    fn file_then_env_with_sources() {
        let d = dir("order");
        fs::write(
            d.join(FILE_NAME),
            "[esp]\nurl = \"http://10.0.0.1:3100\"\ntimeout_secs = 5\n",
        )
        .unwrap();
        let env = |n: &str| (n == "FSP_ESP_TIMEOUT_SECS").then(|| "7".to_string());
        let l = load(&d, &env).unwrap();
        assert_eq!(l.config.esp_url, "http://10.0.0.1:3100");
        assert_eq!(l.config.esp_timeout_secs, 7);
        assert_eq!(l.sources["esp.url"], Source::File);
        assert_eq!(
            l.sources["esp.timeout_secs"],
            Source::Env("FSP_ESP_TIMEOUT_SECS")
        );
        assert!(
            l.describe()
                .iter()
                .any(|x| x == "  esp.timeout_secs = 7 seconds (env FSP_ESP_TIMEOUT_SECS)")
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn every_problem_is_listed() {
        let d = dir("bad");
        fs::write(
            d.join(FILE_NAME),
            "[server]\nbind = \"nowhere\"\nnope = 1\n",
        )
        .unwrap();
        let env = |n: &str| (n == "FSP_ESP_TIMEOUT_SECS").then(|| "x".to_string());
        let p = load(&d, &env).unwrap_err();
        assert!(
            p.iter()
                .any(|x| x.contains("unknown setting `server.nope`")),
            "{p:?}"
        );
        let env = |n: &str| (n == "FSP_NETWORK").then(|| "moon".to_string());
        assert!(load(&dir("net"), &env).unwrap_err()[0].contains("not a network"));
        assert!(
            p.iter().any(|x| x.contains("FSP_ESP_TIMEOUT_SECS")),
            "{p:?}"
        );
        let env = |n: &str| (n == "FSP_MAX_REQUEST_BYTES").then(|| "10".to_string());
        assert!(load(&dir("range"), &env).unwrap_err()[0].contains("4096"));
        let env = |n: &str| (n == FILE_ENV).then(|| "/nonexistent/fsp.toml".to_string());
        assert!(load(&d, &env).is_err());
        fs::remove_dir_all(&d).unwrap();
    }
}
