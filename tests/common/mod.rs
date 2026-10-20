//! The AIR plumbing the integration tests share: a real `air_server` in this
//! process (`sp_wire::air_request::local_server`), the AIR listeners of the ESP
//! mint (the Issuer, as `esp-mint serve` runs it) and of fsp-mint, and a
//! wallet-side `call` that sends one request the way the wallet does
//! (`sp_wire::air_request::ask`) and hands back the status and JSON the mint's
//! outcome has.
//!
//! Wallets reach fsp-mint over AIR only, so the tests that used to call its HTTP
//! routes call its AIR listener instead. A reply is read as the HTTP route's
//! answer used to be: an answered request is `200` and its JSON; a signed refusal
//! is its `http_status` (`400` when it has none) and its JSON, without the
//! `http_status` field; a signed "cannot answer now" is `503` and no JSON.
//!
//! The AIR client always dials port 5702 (upstream item 12), so these tests need
//! that port free; `local_server::start` says so when it is not. One server
//! serves every test of a binary, and the tests take turns on it (it has a single
//! storage worker, so tests running in parallel starve each other past their
//! timeouts). Each test has its own mint keys, so no two tests touch the same cell.

#![allow(dead_code)]

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use serde_json::Value;
use sp_wire::ServiceDid;
use sp_wire::WIRE_VERSION;
use sp_wire::air_request::ask::{AskConfig, AskError, InFlight, Verified, ask};
use sp_wire::air_request::client::{self, ServerRef, block_on_new_runtime};
use sp_wire::air_request::{
    KIND_FEES, KIND_FSP_ISSUE, KIND_FSP_SPEND, KIND_KEYS, KIND_MINT_INFO, KIND_ROTATION,
    STATUS_UNAVAILABLE, VerifyError, local_server,
};
use sp_wire::seal::SecretKey as DidKey;

/// One test at a time on the shared AIR server.
static AIR_TURN: Mutex<()> = Mutex::new(());

/// The shared server plus this test's turn on it; the turn ends when this drops,
/// whether the test passed, failed or panicked.
pub struct Server {
    server: ServerRef,
    _turn: std::sync::MutexGuard<'static, ()>,
}

impl std::ops::Deref for Server {
    type Target = ServerRef;

    fn deref(&self) -> &ServerRef {
        &self.server
    }
}

pub fn server() -> Server {
    let turn = AIR_TURN.lock().unwrap_or_else(|e| e.into_inner());
    let server = local_server::start().expect("a local AIR server (is port 5702 free?)");
    Server {
        server,
        _turn: turn,
    }
}

/// A listener on a thread of its own, as `serve` runs it. Dropping it stops it.
pub struct Listener {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

type Stop = Pin<Box<dyn Future<Output = ()> + Send>>;

fn spawn_listener(
    run: impl FnOnce(Stop, &tokio::runtime::Runtime) -> Result<(), String> + Send + 'static,
) -> Listener {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let thread = thread::Builder::new()
        .name("test-air-listener".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            run(
                Box::pin(async move {
                    let _ = stopped.await;
                }),
                &runtime,
            )
        })
        .unwrap();
    Listener {
        stop: Some(stop),
        thread: Some(thread),
    }
}

/// The ESP mint's listener (the Issuer), on `server`, answering with `serve`.
pub fn start_esp_listening(
    server: &ServerRef,
    key: &DidKey,
    dir: &Path,
    serve: Arc<esp_mint::air_listener::Serve>,
) -> Listener {
    let settings = esp_mint::air_listener::Settings {
        server: server.clone(),
        held_reads: 16,
        timeout_secs: 10,
        state_dir: dir.to_path_buf(),
    };
    let key = key.clone();
    spawn_listener(move |stop, runtime| {
        runtime.block_on(esp_mint::air_listener::run(&settings, &key, &serve, stop))
    })
}

/// fsp-mint's own listener, on `server`, answering with `serve`.
pub fn start_listening(
    server: &ServerRef,
    key: &DidKey,
    dir: &Path,
    serve: Arc<fsp_mint::air_listener::Serve>,
) -> Listener {
    let settings = fsp_mint::air_listener::Settings {
        server: server.clone(),
        held_reads: 16,
        timeout_secs: 10,
        state_dir: dir.to_path_buf(),
    };
    let key = key.clone();
    spawn_listener(move |stop, runtime| {
        runtime.block_on(fsp_mint::air_listener::run(&settings, &key, &serve, stop))
    })
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The kind of request a route of the old HTTP API stands for.
pub fn kind_of(route: &str) -> u8 {
    match route {
        "/health" => KIND_MINT_INFO,
        "/keys" => KIND_KEYS,
        "/fees" => KIND_FEES,
        "/rotation" => KIND_ROTATION,
        "/issue" => KIND_FSP_ISSUE,
        "/spend" => KIND_FSP_SPEND,
        other => panic!("{other} is not a call fsp-mint answers over AIR"),
    }
}

/// A refusal's status and JSON: its `http_status` (`400` when it has none), and
/// the body without that field.
pub fn refusal(body: &[u8]) -> (u16, Value) {
    let mut value = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
    let status = value
        .get("http_status")
        .and_then(Value::as_u64)
        .unwrap_or(400) as u16;
    if let Some(object) = value.as_object_mut() {
        object.remove("http_status");
    }
    (status, value)
}

/// The wallet side of one service: asks fsp-mint's DID on its AIR server, the way
/// the wallet does.
pub struct Air {
    pub did: ServiceDid,
    cfg: AskConfig,
}

impl Air {
    pub fn new(server: &ServerRef, did: ServiceDid, network: &str) -> Air {
        Air {
            did,
            cfg: AskConfig {
                server: server.clone(),
                mint: did,
                wire_version: WIRE_VERSION,
                network: network.to_string(),
                reply_wait_secs: 60,
                attempts: 2,
                max_skew_secs: 300,
                timeout_secs: 10,
            },
        }
    }

    /// Send one request of `kind` with `body` and return the verified reply.
    pub fn ask(&self, kind: u8, body: &[u8]) -> Result<Verified, AskError> {
        async fn go(cfg: &AskConfig, flight: &mut InFlight) -> Result<Verified, AskError> {
            let client = client::connect(&cfg.server).await?;
            ask(&client, cfg, flight, &mut |_| Ok(())).await
        }
        let cfg = &self.cfg;
        let body = body.to_vec();
        block_on_new_runtime(move || async move {
            let mut flight = InFlight::with_body(kind, body);
            go(cfg, &mut flight).await
        })
        .map_err(AskError::from)
        .and_then(|r| r)
    }

    /// The route's call over AIR: the status and JSON of the outcome (see the
    /// module docs). A reply that fails a check, or no reply, fails the test.
    pub fn call(&self, route: &str, body: &[u8]) -> (u16, Value) {
        match self.ask(kind_of(route), body) {
            Ok(v) => {
                if v.refused() {
                    refusal(v.body())
                } else {
                    (200, serde_json::from_slice(v.body()).unwrap_or(Value::Null))
                }
            }
            // The mint said it cannot answer now: what a 5xx is over AIR.
            Err(AskError::Verify(VerifyError::Refused(STATUS_UNAVAILABLE))) => (503, Value::Null),
            Err(e) => panic!("{route} over AIR: {e}"),
        }
    }

    pub fn get_json(&self, route: &str) -> (u16, Value) {
        self.call(route, &[])
    }

    pub fn post_json(&self, route: &str, body: &impl serde::Serialize) -> (u16, Value) {
        self.call(route, &serde_json::to_vec(body).unwrap())
    }
}
