//! What the mint answers. Wallets reach this mint over AIR only: each wallet-facing
//! call is an outcome function here (a status and the route's JSON, as the HTTP
//! route returned them) and [`air_served`] turns the outcome into the signed
//! reply the AIR listener writes. Every request body is read under
//! `server.max_request_bytes` first (a `413` with the same JSON body as the other
//! mints, nothing parsed), then parsed, then checked cheapest first. The HTTP
//! routes left are the operator's `GET /audit` and the health monitor's `GET /health`,
//! on `server.bind` (loopback only).

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use secp256k1::Keypair;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sp_wire::air_request::client::block_on_new_runtime;
use sp_wire::air_request::{
    KIND_FEES, KIND_FSP_ISSUE, KIND_FSP_SPEND, KIND_KEYS, KIND_MINT_INFO, KIND_ROTATION, Served,
};

use crate::api::{FeesResponse, IssueRequest, KeysResponse, SpendRequest};
use crate::config::Config;
use crate::esp_client::Issuer;
use crate::issue::{self, IssueContext, Issues};
use crate::spend::Spender;

pub struct FspState {
    pub config: Config,
    pub spender: Spender,
    pub issues: Mutex<Issues>,
    pub issuer: Box<dyn Issuer>,
    /// The service's DID key pair. `None` refuses every issue.
    pub service_keypair: Option<Keypair>,
    /// The clock, so tests can move it (rotation). Unix seconds.
    pub clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

/// What a call comes to: the status the HTTP route used to return and its JSON.
/// Over AIR the status says how the reply is signed (see [`served_of`]).
pub type Outcome = (StatusCode, Value);

fn error(status: u16, code: &str, why: &str) -> Outcome {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        json!({"error": why, "code": code}),
    )
}

/// [`error`] for a refusal that names notes: `note_spent_elsewhere` carries
/// `spent`, the positions of every input that is spent, which the wallet reads
/// to stop offering those notes.
fn error_naming(status: u16, code: &str, why: &str, spent: &[usize]) -> Outcome {
    let (status, mut body) = error(status, code, why);
    if !spent.is_empty() {
        body["spent"] = json!(spent);
    }
    (status, body)
}

/// The `413` body, the same JSON as the other mints'.
fn too_large_body(size: u64, limit: usize) -> Value {
    json!({
        "error": format!("the request is {size} bytes, and this route accepts at most {limit} bytes. Split it into several requests. Nothing was parsed or spent"),
        "code": sp_wire::codes::REQUEST_TOO_LARGE,
        "size_bytes": size,
        "limit_bytes": limit as u64,
    })
}

/// `GET /health`: the mint info. `ok` while the mint is sound, a `503` with the
/// reason when the spend writer has failed (a signed "cannot answer now" over AIR).
pub fn health_outcome(s: &FspState) -> Outcome {
    let service = s.config.service().map(|k| k.to_string());
    match s.spender.fault() {
        None => (
            StatusCode::OK,
            json!({
                "status": "ok",
                "network": s.config.network,
                "service": service,
                "wire_version": sp_wire::WIRE_VERSION,
            }),
        ),
        Some(why) => (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({
                "status": "broken",
                "reason": why,
                "network": s.config.network,
                "service": service,
                "wire_version": sp_wire::WIRE_VERSION,
            }),
        ),
    }
}

/// When the current keyset is replaced, if rotation is on.
pub fn next_rotation(s: &FspState, now: u64) -> Option<u64> {
    let r = s.spender.rotation();
    r.start_of(r.epoch_at(now) + 1)
}

/// `GET /keys`.
pub fn keys_outcome(s: &FspState) -> Outcome {
    let now = (s.clock)();
    match s.spender.keysets(now) {
        Ok(live) => {
            let body = KeysResponse {
                service: s.config.service(),
                current: live.last().map(|k| k.id.clone()).unwrap_or_default(),
                previous: (live.len() >= 2).then(|| live[live.len() - 2].id.clone()),
                keysets: live
                    .iter()
                    .map(|k| (k.id.clone(), k.public.clone()))
                    .collect(),
                next_rotation: next_rotation(&s, now),
            };
            (
                StatusCode::OK,
                serde_json::to_value(body).unwrap_or_default(),
            )
        }
        Err(e) => error(500, sp_wire::codes::INTERNAL, &e),
    }
}

/// The rotation schedule (`GET /rotation`).
pub fn rotation_window(s: &FspState, now: u64) -> Result<crate::api::RotationResponse, String> {
    use crate::api::{KeysetWindow, RotationResponse};
    let live = s.spender.keysets(now)?;
    let r = s.spender.rotation();
    let window = |k: &crate::keysets::Keyset| KeysetWindow {
        id: k.id.clone(),
        epoch: k.epoch,
        starts: r.start_of(k.epoch),
        // Epoch 0 ends at activation (the start of epoch 1).
        current_until: r.start_of(k.epoch + 1),
        retires: r.start_of(k.epoch + 2),
    };
    let current = window(live.last().expect("a keyset"));
    let previous = (live.len() >= 2).then(|| window(&live[live.len() - 2]));
    Ok(RotationResponse {
        active: r.epoch_at(now) >= 1,
        first_issue: r.first_issue,
        rotation_activation_hours: r.activation_secs / 3600,
        activation: r.activation(),
        rotation_period_hours: r.period_secs / 3600,
        next_rotation: current.current_until,
        current,
        previous,
        now,
    })
}

/// `GET /rotation`.
pub fn rotation_outcome(s: &FspState) -> Outcome {
    match rotation_window(s, (s.clock)()) {
        Ok(body) => (
            StatusCode::OK,
            serde_json::to_value(body).unwrap_or_default(),
        ),
        Err(e) => error(500, sp_wire::codes::INTERNAL, &e),
    }
}

/// `GET /fees`.
pub fn fees_outcome(s: &FspState) -> Outcome {
    let body = FeesResponse {
        issue_fee_per_byte: s.config.issue_fee_per_byte,
        issue_group_fixed_bytes: 16,
        issue_note_bytes: 106,
        issue_output_bytes: 41,
        max_request_bytes: s.config.max_request_bytes,
        issue_max_items: Some(s.config.issue_max_items),
        spend_max_items: Some(s.config.spend_max_items),
        spend_max_memo_bytes: Some(s.config.spend_max_memo_bytes),
    };
    (
        StatusCode::OK,
        serde_json::to_value(body).unwrap_or_default(),
    )
}

/// Run one issue on this thread (it calls the Issuer and writes files).
pub fn run_issue(s: &FspState, request: &IssueRequest) -> Outcome {
    let Some(service) = s.config.service() else {
        return error(503, "not_configured", "service.did is not set");
    };
    let Some(keypair) = s.service_keypair.as_ref() else {
        return error(503, "not_configured", "the service key is not loaded");
    };
    let live = match s.spender.keysets((s.clock)()) {
        Ok(l) => l,
        Err(e) => return error(500, sp_wire::codes::INTERNAL, &e),
    };
    let current = Arc::clone(live.last().expect("a keyset"));
    let previous = (live.len() >= 2).then(|| Arc::clone(&live[live.len() - 2]));
    let ctx = IssueContext {
        service,
        service_keypair: keypair,
        fee_per_byte: s.config.issue_fee_per_byte,
        current: &current,
        previous: previous.as_deref(),
        now: (s.clock)(),
        esp_keyset_ttl_secs: s.config.esp_keyset_ttl_secs,
        max_items: s.config.issue_max_items as usize,
    };
    let Ok(mut issues) = s.issues.lock() else {
        return error(500, sp_wire::codes::INTERNAL, "the issue lock is poisoned");
    };
    match issue::issue(&mut issues, s.issuer.as_ref(), request, &ctx) {
        Ok(r) => {
            // The first FSP issued starts the rotation clock (recorded once).
            if r.groups
                .iter()
                .any(|g| matches!(g, crate::api::GroupResult::Issued { .. }))
            {
                if let Err(e) = s.spender.note_first_issue((s.clock)()) {
                    return error(
                        500,
                        sp_wire::codes::INTERNAL,
                        &format!(
                            "could not record the first issue: {e}. Send the same request again"
                        ),
                    );
                }
            }
            (StatusCode::OK, serde_json::to_value(r).unwrap_or_default())
        }
        Err(e) => error_naming(e.status, e.code, &e.why, &e.spent),
    }
}

/// Run one spend on this thread. `Spender::spend` is async (it waits for its
/// batch to be synced); it is driven here on a runtime of its own, so the caller
/// is a plain blocking thread (the listener's).
pub fn run_spend(s: &FspState, request: &SpendRequest) -> Outcome {
    let now = (s.clock)();
    match block_on_new_runtime(|| s.spender.spend(request, now)) {
        Ok(Ok(r)) => (StatusCode::OK, serde_json::to_value(r).unwrap_or_default()),
        Ok(Err(e)) => error_naming(e.status, e.code, &e.why, &e.spent),
        Err(e) => error(
            500,
            sp_wire::codes::INTERNAL,
            &format!("spend task failed: {}", e.0),
        ),
    }
}

// ---- over AIR ---------------------------------------------------------------

/// The signed refusal of a 4xx. A `400` is the route's body as it is. Any other
/// 4xx (the `413` of a body over its ceiling) has `http_status` added to the body,
/// so that the wallet reads it as a mint error where the outcome may be unknown
/// and not as a refusal of the notes: only a `400` means "refused, nothing was
/// spent".
fn refused(status: StatusCode, mut body: Value) -> Served {
    if status != StatusCode::BAD_REQUEST {
        if let Some(object) = body.as_object_mut() {
            object.insert("http_status".into(), json!(status.as_u16()));
        }
    }
    Served::Refused(serde_json::to_vec(&body).expect("a JSON value encodes"))
}

/// What a call's outcome is over AIR: a success is the body, byte for byte
/// (`serde_json`, the same as axum's `Json`); a 4xx is a signed refusal
/// ([`refused`]); a 5xx (the `502` of an ESP mint that is not answering
/// included), or a body that cannot be encoded, is "cannot answer now": nothing
/// of the mint's state is told to the wallet, and a retry is safe, because an
/// issue is idempotent by request hash and a spend by its notes.
fn served_of(what: &str, (status, body): Outcome) -> Served {
    if status.is_success() {
        return match serde_json::to_vec(&body) {
            Ok(bytes) => Served::Body(bytes),
            Err(e) => {
                eprintln!("AIR: {what} could not be encoded: {e}");
                Served::Unavailable
            }
        };
    }
    if status.is_client_error() {
        return refused(status, body);
    }
    eprintln!("AIR: {what} cannot be answered now ({status}): {body}");
    Served::Unavailable
}

/// A call with a JSON body: the ceiling first, then the parse, then the call. A
/// body over `limit` bytes is refused (the `413` JSON, before it is parsed), and
/// one that does not parse as `Q` is `malformed_request`.
fn post_served<Q: DeserializeOwned>(
    what: &str,
    limit: usize,
    body: &[u8],
    call: impl FnOnce(Q) -> Outcome,
) -> Served {
    if body.len() > limit {
        return refused(
            StatusCode::PAYLOAD_TOO_LARGE,
            too_large_body(body.len() as u64, limit),
        );
    }
    match serde_json::from_slice::<Q>(body) {
        Ok(request) => served_of(what, call(request)),
        Err(e) => {
            let (status, body) = error(
                400,
                sp_wire::codes::MALFORMED_REQUEST,
                &format!("not a valid request: {e}"),
            );
            refused(status, body)
        }
    }
}

/// What the AIR listener says to one kind of request: the JSON body of the call
/// (mint info 1, keys 2, rotation 4, fees 7, issue 11, spend 12), a signed
/// "cannot answer now" for a 5xx outcome, or a signed refusal for a 4xx one.
/// `None` for a kind this mint does not serve (keysets, and the ESP's kinds),
/// which the listener skips.
///
/// `body` is the request body (empty for the reads). It runs on a blocking thread
/// (an issue calls the ESP mint over AIR, and a spend waits for its batch).
pub fn air_served(state: &FspState, kind: u8, body: &[u8]) -> Option<Served> {
    let limit = state.config.max_request_bytes as usize;
    Some(match kind {
        KIND_MINT_INFO => served_of("mint info", health_outcome(state)),
        KIND_KEYS => served_of("keys", keys_outcome(state)),
        KIND_ROTATION => served_of("rotation", rotation_outcome(state)),
        KIND_FEES => served_of("fees", fees_outcome(state)),
        KIND_FSP_ISSUE => post_served("issue", limit, body, |req: IssueRequest| {
            run_issue(state, &req)
        }),
        KIND_FSP_SPEND => post_served("spend", limit, body, |req: SpendRequest| {
            run_spend(state, &req)
        }),
        _ => return None,
    })
}

/// The audit (`GET /audit`): outstanding FSP per key set and in total, and the
/// ESP held in reserve, matured and unmatured. See [`crate::audit`].
pub fn audit_report(s: &FspState, now: u64) -> Result<crate::audit::Report, String> {
    // The spend side first, then the issue side: a spend only uses FSP that
    // was already issued, so this order cannot show more spent than issued.
    let view = s.spender.audit_view(now)?;
    let issues = s
        .issues
        .lock()
        .map_err(|_| "the issue lock is poisoned".to_string())?;
    Ok(crate::audit::build(now, &view, &issues.data))
}

async fn audit(State(s): State<Arc<FspState>>) -> (StatusCode, Json<Value>) {
    // The issue lock is held across a whole issue (including the calls to the
    // Issuer), so wait for it off the async threads.
    let now = (s.clock)();
    match tokio::task::spawn_blocking(move || audit_report(&s, now)).await {
        Ok(Ok(report)) => (
            StatusCode::OK,
            Json(serde_json::to_value(report).unwrap_or_default()),
        ),
        Ok(Err(e)) => {
            let (status, body) = error(500, sp_wire::codes::INTERNAL, &e);
            (status, Json(body))
        }
        Err(e) => {
            let (status, body) = error(
                500,
                sp_wire::codes::INTERNAL,
                &format!("audit task failed: {e}"),
            );
            (status, Json(body))
        }
    }
}

/// `GET /health` for the operator's health monitor (`sp-health-monitor.sh`,
/// which runs on this machine). Wallets do not use it; they ask for the same
/// document over AIR (mint info).
async fn health(State(s): State<Arc<FspState>>) -> (StatusCode, Json<Value>) {
    let (status, body) = health_outcome(&s);
    (status, Json(body))
}

/// The HTTP API that is left, on `server.bind` (loopback only): the operator's
/// `GET /audit`, and `GET /health`, which the local health monitor calls (SPI-local
/// traffic, like esp-mint's). Wallets do not use either; they use the AIR listener.
pub fn router(state: Arc<FspState>) -> Router {
    Router::new()
        .route("/audit", get(audit))
        .route("/health", get(health))
        .with_state(state)
}
