//! The Issuer over AIR: the three calls to the ESP mint, sent to its DID through
//! an AIR server (the only way fsp-mint reaches the ESP mint).
//!
//! - the network check at `serve`: a mint-info request (`KIND_MINT_INFO`),
//! - the current keyset: a keysets request (`KIND_KEYSETS`),
//! - the ESP transfer of an issue: a transfer request (`KIND_TRANSFER`).
//!
//! The flow of one request is `sp_wire::air_request::ask::ask`, shared with the
//! wallet: the reply cell is read first, the request is sealed to the ESP mint's
//! DID and written to the first free mailbox slot, and the reply (signed by the
//! ESP mint, sealed to the request's one-off key `r`) is waited for.
//!
//! **A transfer is kept while it is unanswered.** The request id, `r` and the
//! slots go to `issues.json` (in the pending group of the transfer, through the
//! `save` callback of [`Issuer::transfer_kept`]) before the request is written
//! and after each write. A retry of the same transfer is handed that request: it
//! is resumed only if it was made for the same ESP mint DID and the same
//! transfer (the SHA-256 of the transfer JSON is kept with it). `ask` reads its
//! reply cell first, so a reply that arrived after the last try is found and no
//! second request is written; otherwise the same request is sent again in the
//! next free slot.
//!
//! A request ends when its reply cell is filled: the cell is write-once, so a
//! reply that is "cannot answer now" or fails a check, or that is a `409` or
//! `5xx` answer, can never be replaced. The request is dropped then, the outcome
//! is `Unknown`, and the retry is a new request with the same body: the ESP mint
//! answers a repeated transfer from its stored answer, so it is applied once.
//!
//! How the ESP mint's replies read:
//!
//! - answered: `Done` (the body is the `TransferResponse`),
//! - a signed refusal with the `400` body (no `http_status` field, or `400`):
//!   `Refused { code, why }`,
//! - any other refusal (`http_status` 409 and the like), "cannot answer now"
//!   (`5xx`), and no reply after the tries: `Unknown`.

use sha2::{Digest, Sha256};
use sp_wire::ServiceDid;
use sp_wire::air_request::addr;
use sp_wire::air_request::ask::{AskConfig, AskError, InFlight, Verified, ask};
use sp_wire::air_request::client::{self, ServerRef, block_on_new_runtime};
use sp_wire::air_request::envelope::{
    KIND_KEYSETS, KIND_MINT_INFO, KIND_TRANSFER, MintInfo, STATUS_UNAVAILABLE, VerifyError,
};
use sp_wire::esp::transfer::{TransferRequest, TransferResponse};

use crate::config::Config;
use crate::esp_client::{AirRequest, Issuer, Save, TransferOutcome};

/// The Issuer: the ESP mint, reached over AIR.
pub struct AirIssuer {
    cfg: AskConfig,
}

/// What `ask` is handed to keep a read-only request: nothing (a read is asked
/// again from scratch).
fn keep_nothing(_: &InFlight) -> Result<(), String> {
    Ok(())
}

impl AirIssuer {
    /// From the `air.*` settings, all three required ones of which must be set.
    pub fn new(config: &Config) -> Result<Self, String> {
        config.require_air()?;
        let mint = config
            .air_esp_did
            .ok_or_else(|| "air.esp_did is not set".to_string())?;
        Ok(AirIssuer {
            cfg: AskConfig {
                server: ServerRef {
                    name: config.air_server.clone(),
                    host: config.air_host.clone(),
                },
                mint,
                wire_version: sp_wire::WIRE_VERSION,
                network: config.network.clone(),
                reply_wait_secs: config.air_reply_wait_secs,
                attempts: u32::try_from(config.air_attempts).unwrap_or(u32::MAX),
                max_skew_secs: config.air_max_skew_secs,
                timeout_secs: config.air_timeout_secs,
            },
        })
    }

    /// The ESP mint's DID.
    pub fn mint(&self) -> &ServiceDid {
        &self.cfg.mint
    }

    /// One run of `ask` on a runtime of its own (this mint's issue runs on a
    /// blocking thread, outside any runtime of the AIR client's).
    fn run(
        &self,
        flight: &mut InFlight,
        save: &mut (dyn FnMut(&InFlight) -> Result<(), String> + Send),
    ) -> Result<Verified, AskError> {
        let cfg = &self.cfg;
        match block_on_new_runtime(move || async move {
            let client = client::connect(&cfg.server).await?;
            ask(&client, cfg, flight, save).await
        }) {
            Ok(result) => result,
            Err(e) => Err(AskError::Other(e.0)),
        }
    }

    /// A read-only question: asked afresh, nothing kept, the signed body returned.
    fn read(&self, kind: u8, what: &str) -> Result<Vec<u8>, String> {
        let mut flight = InFlight::of(kind);
        let v = self
            .run(&mut flight, &mut keep_nothing)
            .map_err(|e| format!("the ESP mint's {what} over AIR ({}): {e}", self.describe()))?;
        if v.refused() {
            return Err(format!(
                "the ESP mint refused to give its {what} over AIR ({}): {}",
                self.describe(),
                String::from_utf8_lossy(v.body())
            ));
        }
        Ok(v.body().to_vec())
    }

    /// One try at the transfer with the request `kept` (or a new one when it is
    /// `None`): the result of `ask` and the request as it stands afterwards.
    fn try_transfer(
        &self,
        body: &[u8],
        hash: &str,
        kept: Option<&AirRequest>,
        save: Save,
    ) -> (Result<Verified, AskError>, Option<AirRequest>) {
        let mint = self.cfg.mint.to_hex();
        let (mut flight, created_at) =
            match kept.and_then(|k| flight_of(k, body).map(|f| (f, k.created_at))) {
                Some(resumed) => resumed,
                None => (
                    InFlight::with_body(KIND_TRANSFER, body.to_vec()),
                    addr::now_secs(),
                ),
            };
        let result = {
            let (mint, hash) = (&mint, hash);
            let mut keep = |f: &InFlight| save(&air_request_of(f, mint, hash, created_at));
            self.run(&mut flight, &mut keep)
        };
        (
            result,
            Some(air_request_of(&flight, &mint, hash, created_at)),
        )
    }
}

impl Issuer for AirIssuer {
    fn transfer(&self, request: &TransferRequest) -> TransferOutcome {
        self.transfer_kept(request, None, &mut |_: &AirRequest| Ok(()))
            .0
    }

    fn transfer_kept(
        &self,
        request: &TransferRequest,
        kept: Option<AirRequest>,
        save: Save,
    ) -> (TransferOutcome, Option<AirRequest>) {
        let body = match serde_json::to_vec(request) {
            Ok(b) => b,
            Err(e) => {
                return (
                    TransferOutcome::Unknown(format!("the transfer could not be written: {e}")),
                    kept,
                );
            }
        };
        let hash = body_hash(&body);
        let mint = self.cfg.mint.to_hex();
        // A request made for another ESP mint DID (the setting changed) or for
        // another transfer is not this one's: it is dropped, not resumed.
        let mut kept = kept.filter(|k| k.mint == mint && k.body_hash == hash);
        let mut stale_once = false;
        loop {
            let resumed = kept.is_some();
            let (result, now) = self.try_transfer(&body, &hash, kept.as_ref(), &mut *save);
            // Still in flight afterwards only if it was written (or was kept).
            let in_flight = now.filter(|n| resumed || !n.slots.is_empty());
            match result {
                Ok(v) => return (outcome_of(&v), None),
                // The reply cell of a request kept from long ago holds a reply
                // that is too old: that request is dead, send a new one (once).
                Err(AskError::Verify(VerifyError::Stale { .. })) if resumed && !stale_once => {
                    stale_once = true;
                    kept = None;
                }
                Err(AskError::Verify(VerifyError::Refused(STATUS_UNAVAILABLE))) => {
                    return (
                        TransferOutcome::Unknown(
                            "the ESP mint said it cannot answer the transfer now (a signed reply)"
                                .into(),
                        ),
                        None,
                    );
                }
                Err(AskError::Verify(e)) => {
                    return (
                        TransferOutcome::Unknown(format!(
                            "the ESP mint's reply to the transfer was rejected: {e}"
                        )),
                        None,
                    );
                }
                Err(AskError::NotOpenable(e)) => {
                    return (
                        TransferOutcome::Unknown(format!(
                            "the ESP mint's reply cell does not open with the request key: {e}"
                        )),
                        None,
                    );
                }
                Err(e @ AskError::Unreachable { .. }) => {
                    return (
                        TransferOutcome::Unknown(format!(
                            "the ESP mint did not answer the transfer over AIR: {e}"
                        )),
                        in_flight,
                    );
                }
                Err(AskError::Other(e)) => {
                    return (
                        TransferOutcome::Unknown(format!("the transfer over AIR: {e}")),
                        in_flight,
                    );
                }
            }
        }
    }

    fn current_keyset(&self) -> Result<String, String> {
        let body = self.read(KIND_KEYSETS, "keysets")?;
        let v: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| format!("the ESP mint's keysets over AIR: unreadable reply: {e}"))?;
        v.get("current")
            .and_then(|c| c.as_str())
            .map(str::to_string)
            .ok_or_else(|| "the ESP mint's keysets over AIR: no current keyset in the reply".into())
    }

    fn network(&self) -> Result<Option<String>, String> {
        let body = self.read(KIND_MINT_INFO, "mint info")?;
        let info: MintInfo = serde_json::from_slice(&body)
            .map_err(|e| format!("the ESP mint's mint info over AIR: unreadable reply: {e}"))?;
        Ok(Some(info.network).filter(|n| !n.is_empty()))
    }

    fn describe(&self) -> String {
        format!(
            "DID {} over AIR via {}",
            self.cfg.mint.to_hex(),
            self.cfg.server.host
        )
    }
}

/// What the ESP mint answered to a transfer, as the Issuer's outcome.
fn outcome_of(v: &Verified) -> TransferOutcome {
    if v.refused() {
        let value = serde_json::from_slice::<serde_json::Value>(v.body()).ok();
        let text = |k: &str| {
            value
                .as_ref()
                .and_then(|v| v.get(k))
                .and_then(|e| e.as_str())
                .map(str::to_string)
        };
        // The `400` body is unchanged; any other 4xx carries its number.
        let status = value
            .as_ref()
            .and_then(|v| v.get("http_status"))
            .and_then(|s| s.as_u64());
        return match status {
            None | Some(400) => TransferOutcome::Refused {
                code: text("code").unwrap_or_default(),
                why: text("error").unwrap_or_else(|| String::from_utf8_lossy(v.body()).into()),
            },
            Some(n) => TransferOutcome::Unknown(format!(
                "the ESP mint refused the transfer with {n}: {}",
                String::from_utf8_lossy(v.body())
            )),
        };
    }
    match v.json::<TransferResponse>() {
        Ok(r) => TransferOutcome::Done(r),
        Err(e) => TransferOutcome::Unknown(format!("unreadable reply to the transfer: {e}")),
    }
}

/// The SHA-256 of a request body, hex.
fn body_hash(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

fn air_request_of(flight: &InFlight, mint: &str, hash: &str, created_at: u64) -> AirRequest {
    AirRequest {
        request_id: hex::encode(flight.request_id),
        mint: mint.to_string(),
        r: addr::secret_to_hex(&flight.r),
        slots: flight.slots.clone(),
        created_at,
        body_hash: hash.to_string(),
    }
}

/// The request in flight a kept request stands for, with the body of the
/// transfer being sent again. `None` if what was kept does not read (it is then
/// not resumed; a new request is made).
fn flight_of(kept: &AirRequest, body: &[u8]) -> Option<InFlight> {
    let request_id: [u8; 32] = hex::decode(&kept.request_id).ok()?.try_into().ok()?;
    let r = addr::secret_from_hex(&kept.r).ok()?;
    Some(InFlight {
        kind: KIND_TRANSFER,
        request_id,
        r,
        slots: kept.slots.clone(),
        body: body.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kept_request_round_trips_through_text() {
        let flight = InFlight::with_body(KIND_TRANSFER, b"{}".to_vec());
        let mut with_slots = flight.clone();
        with_slots.slots = vec![0, 3];
        let kept = air_request_of(&with_slots, "02ab", &body_hash(b"{}"), 7);
        let back = flight_of(&kept, b"{}").expect("reads");
        assert_eq!(back.request_id, flight.request_id);
        assert_eq!(back.slots, vec![0, 3]);
        assert_eq!(back.kind, KIND_TRANSFER);
        assert_eq!(addr::secret_to_hex(&back.r), addr::secret_to_hex(&flight.r));
        // The same request as text again.
        assert_eq!(air_request_of(&back, "02ab", &body_hash(b"{}"), 7), kept);
    }

    #[test]
    fn a_kept_request_that_does_not_read_is_not_resumed() {
        let kept = AirRequest {
            request_id: "zz".into(),
            mint: "02ab".into(),
            r: "00".into(),
            slots: vec![],
            created_at: 0,
            body_hash: body_hash(b"{}"),
        };
        assert!(flight_of(&kept, b"{}").is_none());
    }

    #[test]
    fn the_body_hash_is_the_sha256_in_hex() {
        assert_eq!(
            body_hash(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
