//! The HTTP routes. Every request body is read under `server.max_request_bytes`
//! first (a `413` with the same JSON body as the other mints, nothing parsed),
//! then parsed, then checked cheapest first.

use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use secp256k1::Keypair;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

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

type Reply = (StatusCode, Json<Value>);

fn error(status: u16, code: &str, why: &str) -> Reply {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(json!({"error": why, "code": code})),
    )
}

async fn read_json<T: DeserializeOwned>(
    headers: &HeaderMap,
    body: Body,
    limit: usize,
) -> Result<T, Reply> {
    let too_large = |size: Option<u64>| {
        let said = match size {
            Some(s) => format!("the request is {s} bytes"),
            None => "the request is larger than the ceiling".into(),
        };
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "error": format!("{said}, and this route accepts at most {limit} bytes. Split it into several requests. Nothing was parsed or spent"),
                "code": "request_too_large",
                "size_bytes": size,
                "limit_bytes": limit as u64,
            })),
        )
    };
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > limit as u64) {
        return Err(too_large(declared));
    }
    let bytes: Bytes = match axum::body::to_bytes(body, limit).await {
        Ok(b) => b,
        Err(e) if e.to_string().contains("length limit") => return Err(too_large(None)),
        Err(e) => {
            return Err(error(
                400,
                "malformed_request",
                &format!("the body could not be read: {e}"),
            ));
        }
    };
    serde_json::from_slice(&bytes).map_err(|e| {
        error(
            400,
            "malformed_request",
            &format!("not a valid request: {e}"),
        )
    })
}

async fn health(State(s): State<Arc<FspState>>) -> Reply {
    let service = s.config.service().map(|k| k.to_string());
    match s.spender.fault() {
        None => (
            StatusCode::OK,
            Json(json!({"status": "ok", "network": s.config.network, "service": service})),
        ),
        Some(why) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({"status": "broken", "reason": why, "network": s.config.network, "service": service}),
            ),
        ),
    }
}

/// When the current keyset is replaced, if rotation is on.
pub fn next_rotation(s: &FspState, now: u64) -> Option<u64> {
    let r = s.spender.rotation();
    r.start_of(r.epoch_at(now) + 1)
}

async fn keys(State(s): State<Arc<FspState>>) -> Reply {
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
                Json(serde_json::to_value(body).unwrap_or_default()),
            )
        }
        Err(e) => error(500, "internal", &e),
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

async fn rotation(State(s): State<Arc<FspState>>) -> Reply {
    match rotation_window(&s, (s.clock)()) {
        Ok(body) => (
            StatusCode::OK,
            Json(serde_json::to_value(body).unwrap_or_default()),
        ),
        Err(e) => error(500, "internal", &e),
    }
}

async fn fees(State(s): State<Arc<FspState>>) -> Reply {
    let body = FeesResponse {
        issue_fee_per_byte: s.config.issue_fee_per_byte,
        issue_group_fixed_bytes: 16,
        issue_note_bytes: 106,
        issue_output_bytes: 41,
        max_request_bytes: s.config.max_request_bytes,
        issue_max_items: Some(s.config.issue_max_items),
        spend_max_items: Some(s.config.spend_max_items),
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(body).unwrap_or_default()),
    )
}

/// Run one issue on this thread (it calls the Issuer and writes files).
pub fn run_issue(s: &FspState, request: &IssueRequest) -> Reply {
    let Some(service) = s.config.service() else {
        return error(503, "not_configured", "service.did is not set");
    };
    let Some(keypair) = s.service_keypair.as_ref() else {
        return error(503, "not_configured", "the service key is not loaded");
    };
    let live = match s.spender.keysets((s.clock)()) {
        Ok(l) => l,
        Err(e) => return error(500, "internal", &e),
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
        return error(500, "internal", "the issue lock is poisoned");
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
                        "internal",
                        &format!(
                            "could not record the first issue: {e}. Send the same request again"
                        ),
                    );
                }
            }
            (
                StatusCode::OK,
                Json(serde_json::to_value(r).unwrap_or_default()),
            )
        }
        Err(e) => error(e.status, e.code, &e.why),
    }
}

async fn issue_handler(State(s): State<Arc<FspState>>, headers: HeaderMap, body: Body) -> Reply {
    let request: IssueRequest =
        match read_json(&headers, body, s.config.max_request_bytes as usize).await {
            Ok(r) => r,
            Err(r) => return r,
        };
    match tokio::task::spawn_blocking(move || run_issue(&s, &request)).await {
        Ok(r) => r,
        Err(e) => error(500, "internal", &format!("issue task failed: {e}")),
    }
}

async fn spend_handler(State(s): State<Arc<FspState>>, headers: HeaderMap, body: Body) -> Reply {
    let request: SpendRequest =
        match read_json(&headers, body, s.config.max_request_bytes as usize).await {
            Ok(r) => r,
            Err(r) => return r,
        };
    match s.spender.spend(&request, (s.clock)()).await {
        Ok(r) => (
            StatusCode::OK,
            Json(serde_json::to_value(r).unwrap_or_default()),
        ),
        Err(e) => error(e.status, e.code, &e.why),
    }
}

pub fn router(state: Arc<FspState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/keys", get(keys))
        .route("/fees", get(fees))
        .route("/rotation", get(rotation))
        .route("/issue", post(issue_handler))
        .route("/spend", post(spend_handler))
        .with_state(state)
}
