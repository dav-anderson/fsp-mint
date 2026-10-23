//! The calls to the ESP mint (the Issuer): three, all over AIR to the ESP mint's
//! DID (`esp_air::AirIssuer`, the only implementation; there is no HTTP path to
//! the ESP mint):
//!
//! - the network check at `serve` (the ESP mint's mint info),
//! - the current keyset (its keysets),
//! - the ESP transfer of an issue (its transfer).
//!
//! This module holds what the rest of the mint sees: the [`Issuer`] trait, what
//! the Issuer said to a transfer ([`TransferOutcome`]), the request that is kept
//! while a transfer is unanswered ([`AirRequest`]) and the network check.

use serde::{Deserialize, Serialize};
use sp_wire::esp::transfer::{TransferRequest, TransferResponse};

/// What the Issuer said to a transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferOutcome {
    /// Done (or done before: the Issuer stores answers by request hash).
    Done(TransferResponse),
    /// A signed refusal with the `400` body: refused, nothing burned. The code
    /// and text are the Issuer's.
    Refused { code: String, why: String },
    /// No reply after the tries, a signed "cannot answer now", a `5xx` or a
    /// `409` (the Issuer is still working on it), or a reply that failed a check:
    /// the outcome is unknown and the same transfer must be sent again.
    Unknown(String),
}

/// A request over AIR that has been written and not yet answered, kept with the
/// transfer it carries (in `issues.json`, in the pending group), so that a retry
/// reads the reply cell of the same request first and writes no second one. It is
/// what `sp_wire::air_request::ask::InFlight` is, as text: the request id and the
/// one-off key `r` are all that is needed to read the reply, and the body is the
/// transfer itself (rebuilt from the pending group, and found again by its hash).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AirRequest {
    /// 32 random bytes, hex.
    pub request_id: String,
    /// The ESP mint's DID the request was written for, hex. A request kept for
    /// another DID (the setting changed) is not resumed.
    pub mint: String,
    /// The one-off secret key `r`, hex. Used for this request only.
    pub r: String,
    /// The mailbox slots the request was written to, in order.
    pub slots: Vec<u32>,
    /// When the request was made, Unix seconds.
    pub created_at: u64,
    /// The SHA-256 of the request body (the transfer as JSON), hex. A kept
    /// request is resumed only by the transfer it was made for.
    pub body_hash: String,
}

/// What `save` is: called with the request as it stands, before the first write
/// and after each one. If it fails the call fails, because a request that could
/// not be kept cannot safely be sent.
pub type Save<'a> = &'a mut (dyn FnMut(&AirRequest) -> Result<(), String> + Send);

pub trait Issuer: Send + Sync {
    /// The Issuer's current keyset, which a new transfer names for its
    /// outputs. An error means the Issuer could not be asked.
    fn current_keyset(&self) -> Result<String, String>;

    /// Send a transfer. `kept` is the request in flight from an earlier try
    /// (from the pending group), `save` is how a new state of it is kept, and
    /// the second result is the request still in flight afterwards (`None` when
    /// it ended: answered, refused, or its reply failed a check).
    fn transfer_kept(
        &self,
        request: &TransferRequest,
        kept: Option<AirRequest>,
        save: Save,
    ) -> (TransferOutcome, Option<AirRequest>);

    /// The network the Issuer reports in its mint info, if it says. An error
    /// means it could not be asked.
    fn network(&self) -> Result<Option<String>, String>;

    /// Where the Issuer is, for messages: its DID and AIR server.
    fn describe(&self) -> String {
        "the Issuer".into()
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
            "the ESP mint's mint info does not say which network it is on, so it cannot be checked against {expected}. Refusing to start"
        )),
    }
}

/// The line `serve` prints once the check above has passed, the same as
/// esp-mint prints for the PSP. `at` says where the ESP mint is.
pub fn same_network_line(network: &str, at: &str) -> String {
    format!("fsp-mint on {network}: the ESP mint at {at} is on the same network")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_network_passes_and_serve_says_so() {
        assert!(check_network("regtest", Some("regtest")).is_ok());
        assert_eq!(
            same_network_line("regtest", "DID 02ab over AIR"),
            "fsp-mint on regtest: the ESP mint at DID 02ab over AIR is on the same network"
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
