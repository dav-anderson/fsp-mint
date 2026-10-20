//! fsp-mint end to end: a real ESP mint (the Issuer) in-process, fsp-mint in
//! front of it, and the app side built the way the wallet builds it.
//!
//! fsp-mint reaches the ESP mint over AIR only, so every test here runs the ESP
//! mint's own AIR listener against a real `air_server` started in this process
//! (`sp_wire::air_request::local_server`), and fsp-mint's Issuer is the real
//! `AirIssuer`. The AIR client always dials port 5702 (upstream item 12), so
//! these tests need that port free, and they take turns on the one shared server
//! (it has a single storage worker, so tests running in parallel starve each
//! other past their timeouts). The ESP mint's HTTP routes are still served, but
//! only so that the tests can read its `/audit`; fsp-mint does not call them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use axum::Router;
use esp_mint::air_listener::{self, Serve, Settings};
use esp_mint::exchange::{Burn, Psp};
use esp_mint::responses::Responses;
use esp_mint::server::EspState;
use esp_mint::terms::Terms;
use fsp_mint::api::*;
use fsp_mint::audit::{self, Report};
use fsp_mint::config::Config;
use fsp_mint::esp_air::AirIssuer;
use fsp_mint::esp_client::Issuer;
use fsp_mint::issue::{Entry, IssueData, Issues};
use fsp_mint::keysets::Keyset;
use fsp_mint::server::{self, FspState};
use fsp_mint::spend::Spender;
use mint_types::{Amount, BlindNonce, MintInput, MintOutput, Nonce, Note};
use rand::rngs::OsRng;
use secp256k1::{Keypair, PublicKey, SECP256K1, SecretKey};
use sp_wire::ServiceDid;
use sp_wire::air_request::client::{self, ServerRef, block_on_new_runtime};
use sp_wire::air_request::{addr, local_server};
use sp_wire::seal::SecretKey as DidKey;
use tokio::runtime::Runtime;

const NOW: u64 = 1_791_571_184;

/// A folder of its own for every call, even when two tests pick the same name
/// (they did: "partial" twice, so one test deleted the other's folder while
/// both ran in parallel). The counter makes a clash impossible.
fn scratch(name: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("fsp-flow-{}-{n}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn service_keypair() -> Keypair {
    let mut b = [0u8; 32];
    b[31] = 1;
    Keypair::from_secret_key(SECP256K1, &SecretKey::from_slice(&b).unwrap())
}

fn service() -> ServiceDid {
    ServiceDid::from(service_keypair().public_key())
}

struct NoPsp;
impl Psp for NoPsp {
    fn melt(&self, _: &[MintInput], _: &str) -> Result<Amount, String> {
        Err("no psp".into())
    }
    fn burns(&self, _: &[Nonce]) -> Result<Vec<Burn>, String> {
        Err("no psp".into())
    }
}

fn serve(rt: &Runtime, app: Router) -> String {
    let l = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let a = l.local_addr().unwrap();
    rt.spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{a}")
}

/// One test at a time on the shared AIR server.
static AIR_TURN: Mutex<()> = Mutex::new(());

/// The shared server plus this test's turn on it; the turn ends when this drops,
/// whether the test passed, failed or panicked.
struct Server {
    server: ServerRef,
    _turn: std::sync::MutexGuard<'static, ()>,
}

impl std::ops::Deref for Server {
    type Target = ServerRef;

    fn deref(&self) -> &ServerRef {
        &self.server
    }
}

fn server() -> Server {
    let turn = AIR_TURN.lock().unwrap_or_else(|e| e.into_inner());
    let server = local_server::start().expect("a local AIR server (is port 5702 free?)");
    Server {
        server,
        _turn: turn,
    }
}

/// The ESP mint's AIR listener on a thread of its own, as `esp-mint serve` runs
/// it. Dropping it stops it.
struct Listener {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

fn start_serving(server: &ServerRef, key: &DidKey, dir: &Path, serve: Arc<Serve>) -> Listener {
    let settings = Settings {
        server: server.clone(),
        held_reads: 16,
        timeout_secs: 10,
        state_dir: dir.to_path_buf(),
    };
    let key = key.clone();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let thread = thread::Builder::new()
        .name("test-air-listener".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(air_listener::run(&settings, &key, &serve, async move {
                let _ = stopped.await;
            }))
        })
        .unwrap();
    Listener {
        stop: Some(stop),
        thread: Some(thread),
    }
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

/// The ESP mint as fsp-mint reaches it: its DID and the AIR server (what the
/// `air.*` settings name), its listener (which a test may stop and start again,
/// as an ESP mint that is down and comes back), and its HTTP base, which only
/// the tests read (`/audit`).
struct EspAt {
    url: String,
    did: ServiceDid,
    key: DidKey,
    state: Arc<EspState>,
    /// The ESP mint's data folder: where the listener keeps `air_mailbox_next`.
    data: PathBuf,
    listener: Mutex<Option<Listener>>,
    /// Last, so the turn on the AIR server ends after the listener has stopped.
    server: Server,
}

impl EspAt {
    /// What `esp-mint serve` hands its listener to answer with.
    fn serve(&self) -> Arc<Serve> {
        let state = Arc::clone(&self.state);
        Arc::new(move |kind: u8, body: &[u8]| esp_mint::server::air_served(&state, kind, body))
    }

    /// Start the listener (the ESP mint is answering).
    fn listen(&self) {
        let listener = start_serving(&self.server, &self.key, &self.data, self.serve());
        *self.listener.lock().unwrap() = Some(listener);
    }

    /// Stop the listener (the ESP mint is not answering).
    fn silence(&self) {
        drop(self.listener.lock().unwrap().take());
    }

    /// Wait until the listener has taken `want` mailbox slots (it has answered
    /// every request written to the slots below).
    fn wait_for_next(&self, want: u32) {
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            let now = air_listener::read_next(&self.data).unwrap();
            if now == want {
                return;
            }
            assert!(
                Instant::now() < until,
                "the counter is {now}, expected {want}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// The `air.*` settings that point fsp-mint at this ESP mint.
    fn settings(&self, wait_secs: &str, attempts: &str) -> Vec<(&'static str, String)> {
        vec![
            ("air.esp_did", self.did.to_hex()),
            ("air.server", self.server.name.clone()),
            ("air.host", self.server.host.clone()),
            ("air.reply_wait_secs", wait_secs.to_string()),
            ("air.attempts", attempts.to_string()),
        ]
    }
}

/// A real ESP mint with its HTTP routes (for `/audit`) and its AIR listener, on
/// the shared AIR server.
fn start_esp(rt: &Runtime, dir: &Path) -> (EspAt, Arc<EspState>) {
    let server = server();
    let data = dir.join("esp");
    let (logic, master) = esp_mint::mint::open(&data).unwrap();
    let responses = Responses::open(&data.join("responses.json")).unwrap();
    let state = Arc::new(EspState {
        logic: Mutex::new(logic),
        master,
        psp: Box::new(NoPsp),
        responses: Mutex::new(responses),
        queue_path: data.join("queue.json"),
        queue_lock: Mutex::new(()),
        in_flight: Default::default(),
        config: esp_mint::config::Handle::default(),
        rotor: None,
        rollovers: None,
    });
    let key = DidKey::new();
    let at = EspAt {
        url: serve(rt, esp_mint::server::router(Arc::clone(&state))),
        did: addr::key_to_did(&key.public_key()),
        key,
        state: Arc::clone(&state),
        data,
        listener: Mutex::new(None),
        server,
    };
    at.listen();
    (at, state)
}

/// An ESP note the Issuer issued for `service` and `day`.
fn esp_note(state: &EspState, units: u64, service: ServiceDid, day: u64) -> HandedEsp {
    let terms = Terms {
        service,
        redeemable_after_day: day,
    };
    let kp = Keypair::new(SECP256K1, &mut OsRng);
    let nonce = Nonce(kp.public_key());
    let (point, r) = bdhke::blind_message(&nonce.as_hash_preimage(), None);
    let amount = Amount::from_units(units);
    let sk = state.master.key_for(amount, &terms);
    let out = state
        .logic
        .lock()
        .unwrap()
        .issue_with_key(
            &sk,
            &MintOutput {
                amount,
                blind_nonce: BlindNonce(point),
            },
        )
        .unwrap();
    HandedEsp {
        amount: units,
        note: Note {
            nonce,
            signature: bdhke::unblind_message(&out.0, &r, &sk.public_key(SECP256K1)),
        },
        spend_secret: kp.secret_key(),
        keyset: esp_mint::rotation::keyset_id(&state.master),
    }
}

/// A blinded FSP output the app keeps the secrets of.
struct Out {
    amount: u64,
    secret: SecretKey,
    r: SecretKey,
    blind: BlindNonce,
}

impl Out {
    fn new(amount: u64) -> Self {
        let kp = Keypair::new(SECP256K1, &mut OsRng);
        let (p, r) = bdhke::blind_message(&Nonce(kp.public_key()).as_hash_preimage(), None);
        Out {
            amount,
            secret: kp.secret_key(),
            r,
            blind: BlindNonce(p),
        }
    }
    fn mint(&self) -> MintOutput {
        MintOutput {
            amount: Amount::from_units(self.amount),
            blind_nonce: self.blind,
        }
    }
    fn note(&self, sig: &PublicKey, key: &PublicKey) -> (Note, SecretKey) {
        (
            Note {
                nonce: Nonce(self.secret.public_key(SECP256K1)),
                signature: bdhke::unblind_message(sig, &self.r, key),
            },
            self.secret,
        )
    }
}

/// An FSP note signed directly with a keyset (the test's shortcut).
fn fsp_note(k: &Keyset, units: u64) -> (Note, SecretKey) {
    let o = Out::new(units);
    let sk = k.key(Amount::from_units(units)).unwrap();
    o.note(
        &bdhke::sign_message(sk, &o.blind.0),
        &sk.public_key(SECP256K1),
    )
}

fn spend_request(
    keyset: &str,
    notes: &[(Note, SecretKey, u64)],
    outputs: &[MintOutput],
    pay: u64,
) -> SpendRequest {
    let dummy = secp256k1::schnorr::Signature::from_slice(&[1u8; 64]).unwrap();
    let mut req = SpendRequest {
        inputs: notes
            .iter()
            .map(|(n, _, a)| FspInput {
                keyset: keyset.into(),
                input: MintInput {
                    amount: Amount::from_units(*a),
                    note: *n,
                    spend_signature: dummy,
                },
            })
            .collect(),
        outputs: outputs.to_vec(),
        pay,
        memo: "order 1".into(),
    };
    let d = req.digest();
    for (i, (_, s, _)) in req.inputs.iter_mut().zip(notes) {
        i.input.spend_signature =
            SECP256K1.sign_schnorr(&d, &Keypair::from_secret_key(SECP256K1, s));
    }
    req
}

struct Fsp {
    url: String,
    state: Arc<FspState>,
    clock: Arc<AtomicU64>,
}

fn start_fsp(rt: &Runtime, dir: &Path, esp: &EspAt, activation: &str) -> Fsp {
    start_fsp_waiting(rt, dir, esp, activation, "20", "2")
}

/// [`start_fsp`] with the time one try waits for the ESP mint's reply and the
/// number of tries (a test with an ESP mint that is down does not wait long).
fn start_fsp_waiting(
    rt: &Runtime,
    dir: &Path,
    esp: &EspAt,
    activation: &str,
    wait_secs: &str,
    attempts: &str,
) -> Fsp {
    let mut cfg = Config::default();
    for (key, value) in esp.settings(wait_secs, attempts) {
        cfg.set(key, &value).unwrap();
    }
    cfg.set("service.did", &service().to_string()).unwrap();
    cfg.set("rotation.activation_hours", activation).unwrap();
    cfg.set("rotation.period_hours", "240").unwrap();
    // The mint first issued at NOW (the record a real first issue writes).
    std::fs::create_dir_all(dir.join("fsp")).unwrap();
    fsp_mint::keysets::record_first_issue(&dir.join("fsp"), NOW).unwrap();
    cfg.validate().unwrap();
    let clock = Arc::new(AtomicU64::new(NOW));
    let spender = Spender::open(&dir.join("fsp"), cfg.rotation(), 1024, NOW).unwrap_or_else(|_| {
        std::fs::create_dir_all(dir.join("fsp")).unwrap();
        Spender::open(&dir.join("fsp"), cfg.rotation(), 1024, NOW).unwrap()
    });
    let issues = Issues::open(&dir.join("fsp")).unwrap();
    let issuer = AirIssuer::new(&cfg).unwrap();
    let c = Arc::clone(&clock);
    let state = Arc::new(FspState {
        config: cfg,
        spender,
        issues: Mutex::new(issues),
        issuer: Box::new(issuer),
        service_keypair: Some(service_keypair()),
        clock: Box::new(move || c.load(Ordering::SeqCst)),
    });
    Fsp {
        url: serve(rt, server::router(Arc::clone(&state))),
        state,
        clock,
    }
}

fn post(url: &str, body: &impl serde::Serialize) -> (u16, serde_json::Value) {
    let r = reqwest::blocking::Client::new()
        .post(url)
        .json(body)
        .send()
        .unwrap();
    let s = r.status().as_u16();
    (s, r.json().unwrap_or_default())
}

fn current(f: &Fsp) -> Arc<Keyset> {
    f.state
        .spender
        .keysets(f.clock.load(Ordering::SeqCst))
        .unwrap()
        .last()
        .cloned()
        .unwrap()
}

#[test]
fn issue_mixed_days_keeps_the_esp_and_never_issues_twice() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("issue");
    let (esp_at, esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_at, "0");
    let day1 = Terms::new_at(service(), NOW).redeemable_after_day;
    let day2 = day1 + 30; // not mature: FSP ignores maturity
    let a = vec![
        esp_note(&esp, 4, service(), day1),
        esp_note(&esp, 1, service(), day1),
    ];
    let b = vec![esp_note(&esp, 8, service(), day2)];
    let outs_a = [Out::new(4), Out::new(1)];
    let outs_b = [Out::new(8)];
    let req = IssueRequest {
        service: service(),
        groups: vec![
            IssueGroup {
                redeemable_after_day: day1,
                notes: a.clone(),
                outputs: outs_a.iter().map(Out::mint).collect(),
            },
            IssueGroup {
                redeemable_after_day: day2,
                notes: b.clone(),
                outputs: outs_b.iter().map(Out::mint).collect(),
            },
        ],
    };
    let (status, body) = post(&format!("{}/issue", f.url), &req);
    assert_eq!(status, 200, "{body}");
    let reply: IssueResponse = serde_json::from_value(body.clone()).unwrap();
    assert!(
        reply
            .groups
            .iter()
            .all(|g| matches!(g, GroupResult::Issued { .. })),
        "{body}"
    );
    // The same request again: the stored answer, nothing issued twice.
    let (status2, body2) = post(&format!("{}/issue", f.url), &req);
    assert_eq!((status2, &body2), (200, &body));
    assert_eq!(
        f.state.issues.lock().unwrap().data.kept.len(),
        3,
        "the ESP is kept, once"
    );
    // At the Issuer the ESP was transferred, not burned: outstanding unchanged.
    let audit: serde_json::Value = reqwest::blocking::get(format!("{}/audit", esp_at.url))
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(
        audit["redeemed"]
            .as_u64()
            .or(audit["redeemed"].as_str().and_then(|s| s.parse().ok())),
        Some(13),
        "{audit}"
    );
    // The same ESP with other outputs: refused, it is gone.
    let mut again = req.clone();
    again.groups[0].outputs = vec![Out::new(4).mint(), Out::new(1).mint()];
    again.groups[1].outputs = vec![Out::new(8).mint()];
    let (s3, b3) = post(&format!("{}/issue", f.url), &again);
    assert_eq!(
        (s3, b3["code"].as_str()),
        (400, Some("already_spent")),
        "{b3}"
    );

    // The FSP spends: pay 9 of 13 with change 4, then the same notes again.
    let GroupResult::Issued {
        keyset,
        signatures,
        keys,
        ..
    } = &reply.groups[0]
    else {
        unreachable!()
    };
    let GroupResult::Issued {
        signatures: s8,
        keys: k8,
        ..
    } = &reply.groups[1]
    else {
        unreachable!()
    };
    let mut notes = Vec::new();
    for (i, o) in outs_a.iter().enumerate() {
        let (n, s) = o.note(&signatures[i], &keys[i]);
        notes.push((n, s, o.amount));
    }
    let (n8, s8k) = outs_b[0].note(&s8[0], &k8[0]);
    notes.push((n8, s8k, 8));
    let change = Out::new(4);
    let spend = spend_request(keyset, &notes, &[change.mint()], 9);
    let (s, body) = post(&format!("{}/spend", f.url), &spend);
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["paid"], 9);
    let (s, body_again) = post(&format!("{}/spend", f.url), &spend);
    assert_eq!(
        (s, &body_again),
        (200, &body),
        "a retry gets the stored answer"
    );
    let other = spend_request(keyset, &notes, &[Out::new(4).mint()], 9);
    let (s, b) = post(&format!("{}/spend", f.url), &other);
    assert_eq!((s, b["code"].as_str()), (400, Some("already_spent")), "{b}");

    // Wrong service and not exact are refused before anything.
    let mut wrong = req.clone();
    wrong.service = ServiceDid::from(Keypair::new(SECP256K1, &mut OsRng).public_key());
    assert_eq!(
        post(&format!("{}/issue", f.url), &wrong).1["code"],
        "wrong_service"
    );
    let mut inexact = req.clone();
    inexact.groups[1].outputs = vec![Out::new(4).mint()];
    assert_eq!(
        post(&format!("{}/issue", f.url), &inexact).1["code"],
        "not_exact"
    );
}

/// The refused transfer over AIR: the ESP mint's signed `400` refusal for one
/// group, which the other group does not share.
#[test]
fn an_esp_note_for_another_service_or_day_is_refused_by_the_issuer_for_its_group_only() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("partial");
    let (esp_at, esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_at, "0");
    let day1 = Terms::new_at(service(), NOW).redeemable_after_day;
    let good = esp_note(&esp, 2, service(), day1);
    let bad = esp_note(&esp, 2, service(), day1 + 1); // claimed as day1 + 2 below
    let req = IssueRequest {
        service: service(),
        groups: vec![
            IssueGroup {
                redeemable_after_day: day1,
                notes: vec![good],
                outputs: vec![Out::new(2).mint()],
            },
            IssueGroup {
                redeemable_after_day: day1 + 2,
                notes: vec![bad],
                outputs: vec![Out::new(2).mint()],
            },
        ],
    };
    let (s, body) = post(&format!("{}/issue", f.url), &req);
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["groups"][0]["result"], "issued");
    assert_eq!(body["groups"][1]["result"], "refused");
    assert_eq!(f.state.issues.lock().unwrap().data.kept.len(), 1);
}

#[test]
fn rotation_keeps_previous_notes_for_one_period_then_they_are_lost() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("rotate");
    let (esp_at, _esp) = start_esp(&rt, &dir);
    // Activation 120 hours after the first issue (NOW), period 240 hours.
    let f = start_fsp(&rt, &dir, &esp_at, "120");
    let genesis = current(&f);
    let before: RotationResponse = reqwest::blocking::get(format!("{}/rotation", f.url))
        .unwrap()
        .json()
        .unwrap();
    assert!(!before.active && before.previous.is_none());
    assert_eq!(before.current.current_until, Some(NOW + 5 * 86_400));
    let (n, s) = fsp_note(&genesis, 16);
    let (n2, s2) = fsp_note(&genesis, 16);
    f.clock.store(NOW + 5 * 86_400, Ordering::SeqCst);
    let keys: KeysResponse = reqwest::blocking::get(format!("{}/keys", f.url))
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(keys.previous.as_deref(), Some(genesis.id.as_str()));
    assert_ne!(keys.current, genesis.id);
    assert_eq!(keys.next_rotation, Some(NOW + 15 * 86_400));
    let w: RotationResponse = reqwest::blocking::get(format!("{}/rotation", f.url))
        .unwrap()
        .json()
        .unwrap();
    assert!(w.active);
    assert_eq!(w.rotation_period_hours, 240);
    assert_eq!(
        (w.first_issue, w.activation, w.rotation_activation_hours),
        (Some(NOW), Some(NOW + 5 * 86_400), 120)
    );
    assert_eq!(w.current.id, keys.current);
    assert_eq!(w.current.starts, Some(NOW + 5 * 86_400));
    assert_eq!(w.current.retires, Some(NOW + 25 * 86_400));
    let prev = w.previous.clone().unwrap();
    assert_eq!(
        (prev.id.as_str(), prev.starts, prev.retires),
        (genesis.id.as_str(), None, Some(NOW + 15 * 86_400))
    );
    assert_eq!(w.next_rotation, Some(NOW + 15 * 86_400));
    // A previous-set note swaps to the current set.
    let o = Out::new(16);
    let (st, body) = post(
        &format!("{}/spend", f.url),
        &spend_request(&genesis.id, &[(n, s, 16)], &[o.mint()], 0),
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["keyset"], keys.current.as_str());
    // A full period later the genesis set is gone with its spent list.
    f.clock.store(NOW + 15 * 86_400, Ordering::SeqCst);
    let (st, body) = post(
        &format!("{}/spend", f.url),
        &spend_request(&genesis.id, &[(n2, s2, 16)], &[Out::new(16).mint()], 0),
    );
    assert_eq!(
        (st, body["code"].as_str()),
        (400, Some("keyset_retired")),
        "{body}"
    );
    assert!(
        !dir.join("fsp")
            .join(fsp_mint::spend::log_name(&genesis.id))
            .exists()
    );
}

#[test]
fn spends_survive_a_restart_and_a_torn_last_line_is_cut() {
    let dir = scratch("restart").join("fsp");
    std::fs::create_dir_all(&dir).unwrap();
    let rt = Runtime::new().unwrap();
    let off = fsp_mint::keysets::Rotation {
        first_issue: None,
        activation_secs: 0,
        period_secs: 1,
    };
    let sp = Spender::open(&dir, off, 64, NOW).unwrap();
    let k = sp.keysets(NOW).unwrap().last().cloned().unwrap();
    let (n, s) = fsp_note(&k, 2);
    let req = spend_request(&k.id, &[(n, s, 2)], &[Out::new(1).mint()], 1);
    let first = rt.block_on(sp.spend(&req, NOW)).unwrap();
    drop(sp);
    // A crash in the middle of the next line.
    use std::io::Write;
    let log = dir.join(fsp_mint::spend::log_name(&k.id));
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(b"{\"h\":\"ab")
        .unwrap();
    let sp = Spender::open(&dir, off, 64, NOW).unwrap();
    assert!(sp.is_spent(&k.id, &n.nonce.0.serialize()));
    assert_eq!(
        rt.block_on(sp.spend(&req, NOW)).unwrap(),
        first,
        "stored answer after restart"
    );
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.ends_with('\n') && text.lines().count() == 1,
        "the torn line is cut: {text:?}"
    );
}

#[test]
fn one_spent_note_among_several_leaves_the_others_unspent() {
    let dir = scratch("partial-spend").join("fsp");
    std::fs::create_dir_all(&dir).unwrap();
    let rt = Runtime::new().unwrap();
    let off = fsp_mint::keysets::Rotation {
        first_issue: None,
        activation_secs: 0,
        period_secs: 1,
    };
    let sp = Spender::open(&dir, off, 64, NOW).unwrap();
    let k = sp.keysets(NOW).unwrap().last().cloned().unwrap();
    let (a, sa) = fsp_note(&k, 2);
    let (b, sb) = fsp_note(&k, 2);
    let (c, sc) = fsp_note(&k, 2);
    // `a` is spent first, alone.
    let first = spend_request(&k.id, &[(a, sa, 2)], &[Out::new(1).mint()], 1);
    rt.block_on(sp.spend(&first, NOW)).unwrap();
    // Then three notes, `a` among them: refused, and `b` and `c` stay unspent.
    let all = spend_request(
        &k.id,
        &[(b, sb, 2), (a, sa, 2), (c, sc, 2)],
        &[Out::new(4).mint()],
        2,
    );
    let e = rt.block_on(sp.spend(&all, NOW)).unwrap_err();
    assert_eq!(e.code, "already_spent");
    assert!(sp.is_spent(&k.id, &a.nonce.0.serialize()));
    assert!(!sp.is_spent(&k.id, &b.nonce.0.serialize()));
    assert!(!sp.is_spent(&k.id, &c.nonce.0.serialize()));
}

#[test]
fn the_spend_limit_is_the_setting_and_is_served_on_fees() {
    let dir = scratch("spenditems").join("fsp");
    std::fs::create_dir_all(&dir).unwrap();
    let rt = Runtime::new().unwrap();
    let off = fsp_mint::keysets::Rotation {
        first_issue: None,
        activation_secs: 0,
        period_secs: 1,
    };
    let sp = Spender::open(&dir, off, 64, NOW).unwrap().with_max_items(2);
    let k = sp.keysets(NOW).unwrap().last().cloned().unwrap();
    let held: Vec<(Note, SecretKey, u64)> = (0..3)
        .map(|_| {
            let (n, s) = fsp_note(&k, 2);
            (n, s, 2)
        })
        .collect();
    let req = spend_request(&k.id, &held, &[Out::new(1).mint()], 5);
    let e = rt.block_on(sp.spend(&req, NOW)).unwrap_err();
    assert_eq!(e.code, "too_many_items");
    assert!(e.why.contains("at most 2 notes"), "{}", e.why);
    // Two notes are within the limit and go through.
    let ok = spend_request(&k.id, &held[..2], &[Out::new(1).mint()], 3);
    assert!(rt.block_on(sp.spend(&ok, NOW)).is_ok());

    // The mint serves both limits, so a wallet need not guess them.
    let rt = Runtime::new().unwrap();
    let dir = scratch("feeitems");
    let (esp_at, _esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_at, "0");
    let fees: FeesResponse = reqwest::blocking::get(format!("{}/fees", f.url))
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(fees.issue_max_items, Some(1000));
    assert_eq!(fees.spend_max_items, Some(1000));
    assert_eq!(fees.spend_max_memo_bytes, Some(256));
}

#[test]
fn the_memo_limit_is_the_setting_and_a_long_memo_is_refused_before_anything_is_spent() {
    let dir = scratch("spendmemo").join("fsp");
    std::fs::create_dir_all(&dir).unwrap();
    let rt = Runtime::new().unwrap();
    let off = fsp_mint::keysets::Rotation {
        first_issue: None,
        activation_secs: 0,
        period_secs: 1,
    };
    let sp = Spender::open(&dir, off, 64, NOW)
        .unwrap()
        .with_max_memo_bytes(4);
    let k = sp.keysets(NOW).unwrap().last().cloned().unwrap();
    let (n, s) = fsp_note(&k, 2);
    let held = vec![(n, s, 2)];
    // the helper's memo, "order 1", is 7 bytes: over this mint's 4
    let req = spend_request(&k.id, &held, &[Out::new(1).mint()], 1);
    let e = rt.block_on(sp.spend(&req, NOW)).unwrap_err();
    assert_eq!(e.code, "memo_too_long");
    assert!(e.why.contains("at most 4 bytes"), "{}", e.why);
    assert!(!sp.is_spent(&k.id, &held[0].0.nonce.0.serialize()));
}

#[test]
fn throughput_of_the_spend_path() {
    let dir = scratch("speed").join("fsp");
    std::fs::create_dir_all(&dir).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .build()
        .unwrap();
    let off = fsp_mint::keysets::Rotation {
        first_issue: None,
        activation_secs: 0,
        period_secs: 1,
    };
    let sp = Arc::new(Spender::open(&dir, off, 1024, NOW).unwrap());
    let k = sp.keysets(NOW).unwrap().last().cloned().unwrap();
    const N: usize = 2000;
    let requests: Vec<SpendRequest> = (0..N)
        .map(|_| {
            let (n, s) = fsp_note(&k, 2);
            spend_request(&k.id, &[(n, s, 2)], &[Out::new(1).mint()], 1)
        })
        .collect();
    let start = Instant::now();
    rt.block_on(async {
        let mut tasks = Vec::new();
        for r in requests {
            let sp = Arc::clone(&sp);
            tasks.push(tokio::spawn(
                async move { sp.spend(&r, NOW).await.map(|_| ()) },
            ));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }
    });
    let secs = start.elapsed().as_secs_f64();
    let rate = N as f64 / secs;
    println!("{N} spends (one note in, one change output) in {secs:.2} s: {rate:.0} a second");
    let lines = std::fs::read_to_string(dir.join(fsp_mint::spend::log_name(&k.id)))
        .unwrap()
        .lines()
        .count();
    assert_eq!(lines, N);
    assert!(rate > 300.0, "{rate:.0} spends a second");
}

#[test]
fn a_body_over_the_ceiling_is_refused_before_parsing() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("limit");
    let (esp_at, _esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_at, "0");
    let big = vec![b' '; 2 * 1024 * 1024];
    for route in ["issue", "spend"] {
        // A raw request: the mint answers from the header alone and may close
        // before the body is sent, so a write error is not a failure here.
        use std::io::{Read, Write};
        let addr = f.url.trim_start_matches("http://");
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        let head = format!(
            "POST /{route} HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            big.len()
        );
        c.write_all(head.as_bytes()).unwrap();
        let _ = c.write_all(&big);
        let mut reply = String::new();
        let _ = c.read_to_string(&mut reply);
        assert!(reply.starts_with("HTTP/1.1 413"), "{reply}");
        assert!(reply.contains("request_too_large"), "{reply}");
    }
    let w: RotationResponse = reqwest::blocking::get(format!("{}/rotation", f.url))
        .unwrap()
        .json()
        .unwrap();
    assert!(
        !w.active
            && w.activation.is_none()
            && w.rotation_activation_hours == 0
            && w.next_rotation.is_none()
            && w.current.retires.is_none()
    );
    assert_eq!(w.rotation_period_hours, 240);
    let h: serde_json::Value = reqwest::blocking::get(format!("{}/health", f.url))
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(h["network"], "main");
    // The wire format this mint speaks; a wallet refuses a mint on another number.
    assert_eq!(h["wire_version"], sp_wire::WIRE_VERSION);
}

#[test]
fn the_first_issue_starts_the_rotation_clock_once_and_survives_a_restart() {
    let dir = scratch("first-issue").join("fsp");
    std::fs::create_dir_all(&dir).unwrap();
    let on = fsp_mint::keysets::Rotation {
        first_issue: None,
        activation_secs: 100,
        period_secs: 1000,
    };
    let sp = Spender::open(&dir, on, 8, NOW).unwrap();
    assert_eq!(
        sp.rotation().first_issue,
        None,
        "nothing issued yet: no rotation"
    );
    assert_eq!(sp.rotation().epoch_at(u64::MAX / 2), 0);
    assert_eq!(sp.note_first_issue(NOW).unwrap(), NOW);
    assert_eq!(sp.note_first_issue(NOW + 50).unwrap(), NOW, "written once");
    drop(sp);
    let sp = Spender::open(&dir, on, 8, NOW + 200).unwrap();
    assert_eq!(sp.rotation().first_issue, Some(NOW));
    assert_eq!(
        sp.keysets(NOW + 200).unwrap().len(),
        2,
        "rotated at NOW + 100"
    );
    let off = fsp_mint::keysets::Rotation {
        activation_secs: 0,
        ..on
    };
    let sp2 = Spender::open(&dir, off, 8, NOW + 5000).unwrap();
    assert_eq!(sp2.rotation().epoch_at(u64::MAX / 2), 0, "0 hours is off");
}

#[test]
fn outputs_prepared_before_a_rotation_are_signed_by_the_new_set_and_the_reply_says_which() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("rotate-prepared");
    let (esp_at, esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_at, "120");
    let genesis = current(&f);
    // Prepared under the genesis set: a note to spend, its outputs, and an
    // issue request with its outputs (blinding does not depend on the key).
    let (n, s) = fsp_note(&genesis, 16);
    let spend_out = Out::new(16);
    let day = Terms::new_at(service(), NOW).redeemable_after_day;
    let issue_out = Out::new(4);
    let req = IssueRequest {
        service: service(),
        groups: vec![IssueGroup {
            redeemable_after_day: day,
            notes: vec![esp_note(&esp, 4, service(), day)],
            outputs: vec![issue_out.mint()],
        }],
    };

    // The rotation happens before either is sent.
    f.clock.store(NOW + 5 * 86_400, Ordering::SeqCst);
    let now_set = current(&f);
    assert_ne!(now_set.id, genesis.id);
    let keys: KeysResponse = reqwest::blocking::get(format!("{}/keys", f.url))
        .unwrap()
        .json()
        .unwrap();

    // Spend: signed by the current set, named in the reply, and the note
    // unblinds with that set's published key.
    let (st, body) = post(
        &format!("{}/spend", f.url),
        &spend_request(&genesis.id, &[(n, s, 16)], &[spend_out.mint()], 0),
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["keyset"], now_set.id.as_str());
    let reply: fsp_mint::api::SpendResponse = serde_json::from_value(body).unwrap();
    let published = keys.keysets[&reply.keyset][&16];
    assert_eq!(reply.keys[0], published);
    let (note, _) = spend_out.note(&reply.signatures[0], &published);
    assert!(bdhke::verify_message(
        now_set.key(Amount::from_units(16)).unwrap(),
        note.signature,
        &note.nonce.as_hash_preimage()
    ));

    // Issue: the same.
    let (st, body) = post(&format!("{}/issue", f.url), &req);
    assert_eq!(st, 200, "{body}");
    let issued: IssueResponse = serde_json::from_value(body.clone()).unwrap();
    let GroupResult::Issued {
        keyset,
        signatures,
        keys: got,
        ..
    } = &issued.groups[0]
    else {
        panic!("{body}");
    };
    assert_eq!(keyset, &now_set.id);
    assert_eq!(got[0], keys.keysets[keyset][&4]);
    let (note, _) = issue_out.note(&signatures[0], &got[0]);
    assert!(bdhke::verify_message(
        now_set.key(Amount::from_units(4)).unwrap(),
        note.signature,
        &note.nonce.as_hash_preimage()
    ));
    // The ESP the service kept names the Issuer's keyset that signed it.
    assert_eq!(
        f.state.issues.lock().unwrap().data.kept[0].keyset,
        esp_mint::rotation::keyset_id(&esp.master)
    );

    // After the next rotation the stored answer, replayed by request hash,
    // still names the set that signed it.
    f.clock.store(NOW + 15 * 86_400, Ordering::SeqCst);
    assert_ne!(current(&f).id, now_set.id);
    let (st, again) = post(&format!("{}/issue", f.url), &req);
    assert_eq!((st, &again), (200, &body));
}

// ---- the operator audit ----

/// Issue `units` of FSP for one ESP note of `day`; the FSP note and its key.
fn issue_one(esp: &EspState, f: &Fsp, units: u64, day: u64) -> (Note, SecretKey, String) {
    let out = Out::new(units);
    let req = IssueRequest {
        service: service(),
        groups: vec![IssueGroup {
            redeemable_after_day: day,
            notes: vec![esp_note(esp, units, service(), day)],
            outputs: vec![out.mint()],
        }],
    };
    let (s, body) = post(&format!("{}/issue", f.url), &req);
    assert_eq!(s, 200, "{body}");
    let reply: IssueResponse = serde_json::from_value(body).unwrap();
    let GroupResult::Issued {
        keyset,
        signatures,
        keys,
        ..
    } = &reply.groups[0]
    else {
        panic!("not issued")
    };
    let (n, sk) = out.note(&signatures[0], &keys[0]);
    (n, sk, keyset.clone())
}

fn get_audit(f: &Fsp) -> Report {
    reqwest::blocking::get(format!("{}/audit", f.url))
        .unwrap()
        .json()
        .unwrap()
}

fn line<'a>(r: &'a Report, keyset: &str) -> &'a audit::KeysetLine {
    r.fsp
        .keysets
        .iter()
        .find(|k| k.keyset == keyset)
        .unwrap_or_else(|| panic!("no line for {keyset}"))
}

fn units(a: &audit::Amount) -> i128 {
    a.units.0
}

#[test]
fn the_audit_counts_issue_spend_and_change_and_the_esp_reserve() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("audit-basic");
    let (esp_at, esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_at, "0");
    let today = NOW / 86_400;
    let empty = get_audit(&f);
    assert_eq!(units(&empty.fsp.outstanding), 0);
    assert_eq!(empty.esp_reserve.total.notes, 0);
    assert!(empty.complete && empty.consistent, "{:?}", empty.warnings);

    // 4 + 1 FSP for ESP that is mature today, 8 FSP for ESP that is not.
    let day_m = today;
    let day_u = today + 30;
    let outs = [Out::new(4), Out::new(1), Out::new(8)];
    let req = IssueRequest {
        service: service(),
        groups: vec![
            IssueGroup {
                redeemable_after_day: day_m,
                notes: vec![
                    esp_note(&esp, 4, service(), day_m),
                    esp_note(&esp, 1, service(), day_m),
                ],
                outputs: vec![outs[0].mint(), outs[1].mint()],
            },
            IssueGroup {
                redeemable_after_day: day_u,
                notes: vec![esp_note(&esp, 8, service(), day_u)],
                outputs: vec![outs[2].mint()],
            },
        ],
    };
    let (s, body) = post(&format!("{}/issue", f.url), &req);
    assert_eq!(s, 200, "{body}");
    let (s, again) = post(&format!("{}/issue", f.url), &req);
    assert_eq!((s, &again), (200, &body), "a retry is the stored answer");
    let reply: IssueResponse = serde_json::from_value(body).unwrap();

    let r = get_audit(&f);
    let ks = current(&f).id.clone();
    assert_eq!(units(&r.fsp.outstanding), 13, "counted once, not twice");
    assert_eq!(units(&line(&r, &ks).issued), 13);
    assert_eq!(r.fsp.outstanding.psp, "0.00000000013");
    assert_eq!(
        (
            r.esp_reserve.matured.notes,
            units(&r.esp_reserve.matured.esp)
        ),
        (2, 5)
    );
    assert_eq!(
        (
            r.esp_reserve.unmatured.notes,
            units(&r.esp_reserve.unmatured.esp)
        ),
        (1, 8)
    );
    assert_eq!(r.esp_reserve.unmatured.esp.psp, "0.00000000008");
    assert_eq!(units(&r.esp_reserve.total.esp), 13);
    assert_eq!(r.esp_reserve.by_day.len(), 2);
    assert!(r.complete && r.consistent, "{:?}", r.warnings);

    // Pay 9 of the 13, 4 back as change.
    let mut notes = Vec::new();
    for (g, group_outs) in [(0usize, vec![0usize, 1]), (1, vec![2])] {
        let GroupResult::Issued {
            signatures, keys, ..
        } = &reply.groups[g]
        else {
            unreachable!()
        };
        for (i, o) in group_outs.into_iter().enumerate() {
            let (n, sk) = outs[o].note(&signatures[i], &keys[i]);
            notes.push((n, sk, outs[o].amount));
        }
    }
    let spend = spend_request(&ks, &notes, &[Out::new(4).mint()], 9);
    assert_eq!(post(&format!("{}/spend", f.url), &spend).0, 200);
    let r = get_audit(&f);
    let l = line(&r, &ks);
    assert_eq!(
        (
            units(&l.issued),
            units(&l.change),
            units(&l.spent),
            units(&l.outstanding)
        ),
        (13, 4, 13, 4)
    );
    assert_eq!((units(&r.fsp.outstanding), units(&r.fsp.paid)), (4, 9));

    // The same spend again, and the same notes in another spend: no change.
    assert_eq!(post(&format!("{}/spend", f.url), &spend).0, 200);
    let other = spend_request(&ks, &notes, &[Out::new(4).mint()], 9);
    assert_eq!(post(&format!("{}/spend", f.url), &other).0, 400);
    assert_eq!(get_audit(&f).fsp, r.fsp);

    // The ESP does not change with spends: it is the reserve for them.
    assert_eq!(units(&get_audit(&f).esp_reserve.total.esp), 13);
    // The clock moving past day_u moves the 8 from unmatured to matured.
    f.clock
        .store((day_u + 1) * 86_400, std::sync::atomic::Ordering::SeqCst);
    let later = get_audit(&f);
    assert_eq!(units(&later.esp_reserve.unmatured.esp), 0);
    assert_eq!(units(&later.esp_reserve.matured.esp), 13);
}

#[test]
fn the_audit_follows_rotation_expiry_and_a_restart() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("audit-rotate");
    let (esp_at, esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_at, "120");
    let day = NOW / 86_400 + 365;
    let g = current(&f).id.clone();
    let (a, a_key, ks_a) = issue_one(&esp, &f, 16, day);
    let (_b, _b_key, ks_b) = issue_one(&esp, &f, 8, day);
    assert_eq!((ks_a.as_str(), ks_b.as_str()), (g.as_str(), g.as_str()));
    let r = get_audit(&f);
    assert_eq!(
        (units(&r.fsp.outstanding), units(&line(&r, &g).issued)),
        (24, 24)
    );

    // One period on: the first set is the previous one. The 16 is swapped
    // into the new set, no payment: the total does not move, the sets do.
    f.clock
        .store(NOW + 5 * 86_400, std::sync::atomic::Ordering::SeqCst);
    let out16 = Out::new(16);
    let (st, body) = post(
        &format!("{}/spend", f.url),
        &spend_request(&g, &[(a, a_key, 16)], &[out16.mint()], 0),
    );
    assert_eq!(st, 200, "{body}");
    let first: SpendResponse = serde_json::from_value(body).unwrap();
    let c = first.keyset.clone();
    assert_ne!(c, g);
    let r = get_audit(&f);
    assert_eq!(
        r.fsp
            .keysets
            .iter()
            .map(|k| (k.keyset.as_str(), k.status.as_str()))
            .collect::<Vec<_>>(),
        [(g.as_str(), "live"), (c.as_str(), "live")]
    );
    assert_eq!(units(&line(&r, &g).outstanding), 8);
    assert_eq!(units(&line(&r, &c).outstanding), 16);
    assert_eq!(
        (units(&r.fsp.outstanding), units(&r.fsp.paid)),
        (24, 0),
        "a swap moves value between sets and pays nothing"
    );

    // Pay 6 out of the swapped 16, 10 as change.
    let (n16, k16) = out16.note(&first.signatures[0], &first.keys[0]);
    let (st, _) = post(
        &format!("{}/spend", f.url),
        &spend_request(
            &c,
            &[(n16, k16, 16)],
            &[Out::new(8).mint(), Out::new(2).mint()],
            6,
        ),
    );
    assert_eq!(st, 200);
    let r = get_audit(&f);
    assert_eq!(units(&line(&r, &c).outstanding), 10);
    assert_eq!((units(&r.fsp.outstanding), units(&r.fsp.paid)), (18, 6));

    // Another period: the first set is deleted with the 8 nobody spent.
    f.clock
        .store(NOW + 15 * 86_400, std::sync::atomic::Ordering::SeqCst);
    let after = get_audit(&f);
    let gone = line(&after, &g);
    assert_eq!(gone.status, "retired");
    assert_eq!(units(&gone.outstanding), 8);
    assert_eq!(units(&after.fsp.expired), 8, "expired, not outstanding");
    assert_eq!(units(&after.fsp.outstanding), 10);
    assert_eq!(units(&after.fsp.paid), 6);
    assert!(after.complete && after.consistent, "{:?}", after.warnings);
    assert!(dir.join("fsp").join(audit::LEDGER_FILE).exists());
    assert!(!dir.join("fsp").join(fsp_mint::spend::log_name(&g)).exists());

    // The files alone give the same report (what `fsp-mint audit` reads) ...
    let now = NOW + 15 * 86_400;
    assert_eq!(audit::from_dir(&dir.join("fsp"), now).unwrap(), {
        let mut r = after.clone();
        r.now = now;
        r
    });
    // ... and so does a mint started again on them.
    let off = f.state.spender.rotation();
    let again = Spender::open(&dir.join("fsp"), off, 64, now).unwrap();
    let view = again.audit_view(now).unwrap();
    let issues = Issues::open(&dir.join("fsp")).unwrap();
    assert_eq!(audit::build(now, &view, &issues.data).fsp, after.fsp);
}

#[test]
fn a_key_set_deleted_before_its_counters_were_saved_is_folded_at_the_next_start() {
    let dir = scratch("audit-orphan").join("fsp");
    std::fs::create_dir_all(&dir).unwrap();
    let rt = Runtime::new().unwrap();
    let off = fsp_mint::keysets::Rotation {
        first_issue: None,
        activation_secs: 0,
        period_secs: 1,
    };
    let sp = Spender::open(&dir, off, 64, NOW).unwrap();
    let k = sp.keysets(NOW).unwrap().last().cloned().unwrap();
    let (n, s) = fsp_note(&k, 16);
    let req = spend_request(
        &k.id,
        &[(n, s, 16)],
        &[Out::new(8).mint(), Out::new(2).mint()],
        6,
    );
    rt.block_on(sp.spend(&req, NOW)).unwrap();
    drop(sp);

    // A reader next to a mint sees a half-written last line and leaves it.
    use std::io::Write;
    let log = dir.join(fsp_mint::spend::log_name(&k.id));
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(b"{\"h\":\"ab")
        .unwrap();
    let len = std::fs::metadata(&log).unwrap().len();
    let counted = fsp_mint::spend::replay_counters(&dir).unwrap();
    assert_eq!(counted[&k.id].spent.0, 16);
    assert_eq!(counted[&k.id].change.0, 10);
    assert_eq!(std::fs::metadata(&log).unwrap().len(), len, "read only");

    // The crash: the key file moved on (the set is gone), the counters were
    // never copied. The log is still there.
    fsp_mint::keysets::Keysets::open(&dir)
        .unwrap()
        .rotate_to(2)
        .unwrap();
    assert!(log.exists());
    let sp = Spender::open(&dir, off, 64, NOW).unwrap();
    assert!(!log.exists(), "the log of a deleted set is removed");
    let view = sp.audit_view(NOW).unwrap();
    let c = view.retired[&k.id];
    assert_eq!((c.spent.0, c.change.0), (16, 10));
    assert!(view.counters.is_empty());
    // This test's note was never issued, so the books do not balance, and
    // the audit says so.
    let r = audit::build(NOW, &view, &IssueData::default());
    assert!(!r.consistent && !r.warnings.is_empty());
    assert_eq!(units(&r.fsp.paid), 6);
    // Started again, the ledger is read and nothing is counted twice.
    drop(sp);
    let sp = Spender::open(&dir, off, 64, NOW).unwrap();
    assert_eq!(sp.audit_view(NOW).unwrap().retired[&k.id], c);
}

#[test]
fn what_was_answered_before_the_counters_is_reported_as_not_counted() {
    // An issue answered by 0.3.0: a stored answer, no counter.
    let mut d = IssueData::default();
    d.entries.insert(
        "h".into(),
        Entry::Done(IssueResponse {
            groups: vec![],
            fee_per_byte: 0,
        }),
    );
    d.start_counting();
    d.start_counting();
    assert_eq!(d.uncounted_requests, Some(1), "set once");
    let r = audit::build(NOW, &audit::SpendView::default(), &d);
    assert!(!r.complete);
    assert!(
        r.warnings[0].contains("1 issue request"),
        "{:?}",
        r.warnings
    );

    // A spend line without amounts.
    let dir = scratch("audit-legacy").join("fsp");
    std::fs::create_dir_all(&dir).unwrap();
    let rt = Runtime::new().unwrap();
    let off = fsp_mint::keysets::Rotation {
        first_issue: None,
        activation_secs: 0,
        period_secs: 1,
    };
    let sp = Spender::open(&dir, off, 64, NOW).unwrap();
    let k = sp.keysets(NOW).unwrap().last().cloned().unwrap();
    let (n, s) = fsp_note(&k, 2);
    rt.block_on(sp.spend(
        &spend_request(&k.id, &[(n, s, 2)], &[Out::new(1).mint()], 1),
        NOW,
    ))
    .unwrap();
    drop(sp);
    let log = dir.join(fsp_mint::spend::log_name(&k.id));
    let mut v: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&log).unwrap().trim()).unwrap();
    assert!(v.as_object_mut().unwrap().remove("a").is_some());
    std::fs::write(&log, format!("{v}\n")).unwrap();
    let counted = fsp_mint::spend::replay_counters(&dir).unwrap();
    assert_eq!(counted[&k.id].uncounted_spends, 1);
    assert_eq!(counted[&k.id].spent.0, 0);
    let view = audit::SpendView {
        live: vec![k.id.clone()],
        counters: counted,
        retired: Default::default(),
    };
    let r = audit::build(NOW, &view, &IssueData::default());
    assert!(
        !r.complete && r.warnings[0].contains("1 spend"),
        "{:?}",
        r.warnings
    );
}

// ---------------------------------------------------------------- over AIR

/// One group of one ESP note of `units`, for `day`.
fn one_group(esp: &EspState, units: u64, day: u64) -> IssueRequest {
    IssueRequest {
        service: service(),
        groups: vec![IssueGroup {
            redeemable_after_day: day,
            notes: vec![esp_note(esp, units, service(), day)],
            outputs: vec![Out::new(units).mint()],
        }],
    }
}

/// `Create` bytes in a mailbox slot of the ESP mint, as anyone may: `Created`
/// says the slot was free, so nothing had been written to it.
fn put(server: &ServerRef, key: &DidKey, bytes: Vec<u8>) -> client::Put {
    block_on_new_runtime(move || async move {
        let client = client::connect(server).await.unwrap();
        client::create(&client, key, bytes, 10).await.unwrap()
    })
    .unwrap()
}

fn issues_file(dir: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(dir.join("fsp").join("issues.json")).unwrap()).unwrap()
}

#[test]
fn the_esp_mints_network_and_current_keyset_are_read_over_air() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("reads");
    let (esp_at, esp) = start_esp(&rt, &dir);
    let mut cfg = Config::default();
    for (key, value) in esp_at.settings("20", "2") {
        cfg.set(key, &value).unwrap();
    }
    let issuer = AirIssuer::new(&cfg).unwrap();
    assert_eq!(issuer.network().unwrap().as_deref(), Some("main"));
    assert_eq!(
        issuer.current_keyset().unwrap(),
        esp_mint::rotation::keyset_id(&esp.master)
    );
    assert!(issuer.describe().contains(&esp_at.did.to_hex()));
}

#[test]
fn an_esp_mint_that_does_not_answer_leaves_the_issue_pending_and_the_retry_finishes_it_with_one_transfer()
 {
    let rt = Runtime::new().unwrap();
    let dir = scratch("silent");
    let (esp_at, esp) = start_esp(&rt, &dir);
    // One short try: an ESP mint that is down is found out quickly (but long
    // enough for a listener that is up).
    let f = start_fsp_waiting(&rt, &dir, &esp_at, "0", "5", "1");
    let day = Terms::new_at(service(), NOW).redeemable_after_day;

    // A first issue: the keyset read is slot 0, its transfer slot 1.
    let first = one_group(&esp, 2, day);
    let (s, body) = post(&format!("{}/issue", f.url), &first);
    assert_eq!(s, 200, "{body}");
    esp_at.wait_for_next(2);

    // The ESP mint goes quiet. The keyset is cached, so the transfer is sent,
    // written to slot 2, and not answered.
    esp_at.silence();
    let second = one_group(&esp, 4, day);
    let hash = second.hash();
    let (s, body) = post(&format!("{}/issue", f.url), &second);
    assert_eq!(s, 502, "{body}");
    assert_eq!(body["code"], "issuer_unavailable", "{body}");
    assert_eq!(f.state.issues.lock().unwrap().data.kept.len(), 1);
    // The request in flight is kept in `issues.json`, beside its transfer.
    let file = issues_file(&dir);
    let air = file["entries"][&hash]["groups"][0]["air"].clone();
    assert_eq!(air["mint"], esp_at.did.to_hex(), "{file}");
    assert_eq!(air["slots"], serde_json::json!([2]), "{file}");
    let request_id = air["request_id"].as_str().unwrap().to_string();
    assert_eq!(request_id.len(), 64);

    // The ESP mint comes back and answers the request that was waiting.
    esp_at.listen();
    esp_at.wait_for_next(3);

    // A restart of fsp-mint, then the retry: it reads the reply cell of the same
    // request, finds the answer, and writes nothing.
    drop(f);
    let f = start_fsp(&rt, &dir, &esp_at, "0");
    let (s, body) = post(&format!("{}/issue", f.url), &second);
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["groups"][0]["result"], "issued", "{body}");
    assert_eq!(f.state.issues.lock().unwrap().data.kept.len(), 2);
    // Done: the stored answer, and no request kept.
    let file = issues_file(&dir);
    assert_eq!(file["entries"][&hash]["entry"], "done", "{file}");
    assert!(!file.to_string().contains(&request_id), "{file}");
    // No second request was written: slot 3 is still free.
    let probe = put(
        &esp_at.server,
        &addr::mailbox_secret(&esp_at.did, 3),
        vec![0u8; 8],
    );
    assert!(
        matches!(probe, client::Put::Created(_)),
        "the retry wrote a second request"
    );
    // And the transfer happened once at the ESP mint: 2 + 4 units transferred.
    let audit: serde_json::Value = reqwest::blocking::get(format!("{}/audit", esp_at.url))
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(
        audit["redeemed"]
            .as_u64()
            .or(audit["redeemed"].as_str().and_then(|s| s.parse().ok())),
        Some(6),
        "{audit}"
    );
}

/// Run `fsp-mint` with `args` and exactly the environment `env` (plus the data
/// folder `base`), and wait for it to end. A `serve` that does not stop is a
/// failure of the test, not a hang.
fn fsp_run(base: &Path, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_fsp-mint"))
        .args(args)
        .env_clear()
        .env("FSP_DATA_DIR", base)
        .envs(env.iter().copied())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(90);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() > until {
            let _ = child.kill();
            panic!("`fsp-mint {}` did not stop", args.join(" "));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn serve_refuses_to_start_when_the_esp_mint_is_on_another_network() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("mismatch");
    let (esp_at, _esp) = start_esp(&rt, &dir); // reports "main"
    let base = scratch("mismatch-run");
    let did = esp_at.did.to_hex();
    let air = [
        ("FSP_NETWORK", "regtest"),
        ("FSP_AIR_ESP_DID", did.as_str()),
        ("FSP_AIR_SERVER", esp_at.server.name.as_str()),
        ("FSP_AIR_HOST", esp_at.server.host.as_str()),
        ("FSP_AIR_REPLY_WAIT_SECS", "20"),
        ("FSP_AIR_ATTEMPTS", "2"),
    ];
    // The service key first, so that serve gets as far as the network check.
    let made = fsp_run(&base, &["service-key"], &air);
    let text = String::from_utf8_lossy(&made.stdout).to_string();
    assert!(made.status.success(), "{text}");
    let service_did = text
        .lines()
        .find_map(|l| l.strip_prefix("service.did = "))
        .expect("the service DID is printed")
        .trim()
        .to_string();
    let mut env = air.to_vec();
    env.push(("FSP_SERVICE_DID", service_did.as_str()));
    let out = fsp_run(&base, &["serve"], &env);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "serve started: {err}");
    assert!(
        err.contains("the ESP mint is on main") && err.contains("set to regtest"),
        "{err}"
    );
    assert!(err.contains("Refusing to start"), "{err}");
}

#[test]
fn serve_refuses_to_start_without_the_air_settings_and_names_each_missing_one() {
    let base = scratch("no-air");
    // None of the three.
    let out = fsp_run(&base, &["serve"], &[]);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "serve started");
    for missing in ["air.esp_did", "air.server", "air.host"] {
        assert!(err.contains(missing), "{missing} is not named: {err}");
    }
    // Only the host set: the other two are named, the host is not.
    let out = fsp_run(&base, &["serve"], &[("FSP_AIR_HOST", "127.0.0.1")]);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "serve started");
    assert!(
        err.contains("air.esp_did") && err.contains("air.server"),
        "{err}"
    );
    assert!(!err.contains("FSP_AIR_HOST"), "{err}");
    // The old settings are gone, with a message that says what replaces them.
    let out = fsp_run(
        &base,
        &["config"],
        &[("FSP_ESP_URL", "http://127.0.0.1:3100")],
    );
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "config accepted esp.url");
    assert!(err.contains("esp.url") && err.contains("is gone"), "{err}");
    assert!(err.contains("air.esp_did"), "{err}");
}
