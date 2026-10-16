//! The calls to the ESP mint (the Issuer).

use std::time::Duration;

use esp_mint::transfer::{TransferRequest, TransferResponse};

/// What the Issuer said to a transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferOutcome {
    /// Done (or done before: the Issuer stores answers by request hash).
    Done(TransferResponse),
    /// A `400`: refused, nothing burned. The code and text are the Issuer's.
    Refused { code: String, why: String },
    /// Unreachable, a timeout or a `5xx`: the outcome is unknown and the same
    /// request must be sent again.
    Unknown(String),
}

pub trait Issuer: Send + Sync {
    fn transfer(&self, request: &TransferRequest) -> TransferOutcome;
    /// The Issuer's current keyset (`GET /keysets`), which a new transfer
    /// names for its outputs. An error means the Issuer could not be asked.
    fn current_keyset(&self) -> Result<String, String>;
}

pub struct HttpIssuer {
    base: String,
    client: reqwest::blocking::Client,
}

impl HttpIssuer {
    pub fn new(base: &str, timeout_secs: u64) -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| format!("could not make an HTTP client: {e}"))?;
        Ok(HttpIssuer {
            base: base.trim_end_matches('/').to_string(),
            client,
        })
    }

    /// The network the ESP mint reports in `GET /health`.
    pub fn network(&self) -> Result<Option<String>, String> {
        let r = self
            .client
            .get(format!("{}/health", self.base))
            .send()
            .map_err(|e| format!("esp /health: {e}"))?;
        let body: serde_json::Value = r.json().map_err(|e| format!("esp /health: {e}"))?;
        Ok(body
            .get("network")
            .and_then(|n| n.as_str())
            .map(str::to_string))
    }
}

impl Issuer for HttpIssuer {
    fn current_keyset(&self) -> Result<String, String> {
        let url = format!("{}/keysets", self.base);
        let r = self
            .client
            .get(&url)
            .send()
            .map_err(|e| format!("{url}: {e}"))?;
        if !r.status().is_success() {
            return Err(format!("{url} returned {}", r.status()));
        }
        let body: serde_json::Value = r.json().map_err(|e| format!("{url}: {e}"))?;
        body.get("current")
            .and_then(|c| c.as_str())
            .map(str::to_string)
            .ok_or_else(|| format!("{url}: no current keyset in the reply"))
    }

    fn transfer(&self, request: &TransferRequest) -> TransferOutcome {
        let url = format!("{}/transfer", self.base);
        let response = match self.client.post(&url).json(request).send() {
            Ok(r) => r,
            Err(e) => return TransferOutcome::Unknown(format!("{url}: {e}")),
        };
        let status = response.status();
        let text = response.text().unwrap_or_default();
        if status.is_success() {
            match serde_json::from_str(&text) {
                Ok(r) => TransferOutcome::Done(r),
                Err(e) => TransferOutcome::Unknown(format!("{url}: unreadable reply: {e}")),
            }
        } else if status.as_u16() == 400 {
            let v = serde_json::from_str::<serde_json::Value>(&text).ok();
            let field = |k: &str| {
                v.as_ref()
                    .and_then(|v| v.get(k).and_then(|e| e.as_str()).map(str::to_string))
            };
            TransferOutcome::Refused {
                code: field("code").unwrap_or_default(),
                why: field("error").unwrap_or(text),
            }
        } else {
            TransferOutcome::Unknown(format!("{url} returned {status}: {text}"))
        }
    }
}

/// Check the ESP mint's network against ours.
pub fn check_network(expected: &str, reported: Option<&str>) -> Result<(), String> {
    match reported {
        Some(n) if n == expected => Ok(()),
        Some(n) => Err(format!(
            "the ESP mint is on {n}, but this FSP mint is set to {expected} (server.network, FSP_NETWORK). Refusing to start"
        )),
        None => Err(format!(
            "the ESP mint's /health does not say which network it is on, so it cannot be checked against {expected}. Refusing to start"
        )),
    }
}

/// The line `serve` prints once the check above has passed, the same as
/// esp-mint prints for the PSP.
pub fn same_network_line(network: &str, esp_url: &str) -> String {
    format!("fsp-mint on {network}: the ESP mint at {esp_url} is on the same network")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_network_passes_and_serve_says_so() {
        assert!(check_network("regtest", Some("regtest")).is_ok());
        assert_eq!(
            same_network_line("regtest", "http://127.0.0.1:3100"),
            "fsp-mint on regtest: the ESP mint at http://127.0.0.1:3100 is on the same network"
        );
    }

    #[test]
    fn a_different_or_unreported_network_stops_the_mint() {
        assert!(
            check_network("main", Some("regtest"))
                .unwrap_err()
                .contains("Refusing to start")
        );
        assert!(
            check_network("main", None)
                .unwrap_err()
                .contains("does not say which network")
        );
    }
}
