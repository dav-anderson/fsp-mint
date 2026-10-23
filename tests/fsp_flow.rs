//! fsp-mint end to end: a real ESP mint (the Issuer) in-process, fsp-mint in
//! front of it, and the app side built the way the wallet builds it.
//!
//! Both mints are reached over AIR only, so every test here runs the ESP mint's
//! AIR listener and fsp-mint's own AIR listener against a real `air_server`
//! started in this process (`sp_wire::air_request::local_server`). fsp-mint's
//! Issuer is the real `AirIssuer`, and the app side asks fsp-mint's DID over AIR
//! the way the wallet does (`tests/common`). The AIR client always dials port
//! 5702 (upstream item 12), so these tests need that port free, and they take
//! turns on the one shared server (it has a single storage worker, so tests
//! running in parallel starve each other past their timeouts). Each test has its
//! own service key, so no two tests touch the same mailbox. HTTP is left for the
//! operator's `/audit` (fsp-mint's and the ESP mint's), which the tests read.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use axum::Router;
use common::{
    Air, Listener, Server, fsp_run, scratch, server, start_esp_listening, start_listening,
};
use esp_mint::exchange::{Burn, Psp};
use esp_mint::ledger::Movements;
use esp_mint::server::EspState;
use esp_mint::terms::Terms;
use fsp_mint::api::*;
use fsp_mint::audit::{self, Report};
use fsp_mint::config::Config;
use fsp_mint::esp_air::AirIssuer;
use fsp_mint::esp_client::Issuer;
use fsp_mint::export::export_esp;
use fsp_mint::issue::{Entry, IssueData, Issues, KeptEsp};
use fsp_mint::keysets::Keyset;
use fsp_mint::server::{self, FspState};
use fsp_mint::spend::Spender;
use fsp_mint::store;
use mint_types::{Amount, BlindNonce, MintInput, MintOutput, Nonce, Note};
use rand::rngs::OsRng;
use secp256k1::{Keypair, PublicKey, SECP256K1, SecretKey};
use sp_wire::ServiceDid;
use sp_wire::air_request::addr;
use sp_wire::air_request::client::{self, ServerRef, block_on_new_runtime};
use sp_wire::esp::exchange::ExchangeResponse;
use sp_wire::esp::handover::Handover;
use sp_wire::seal::SecretKey as DidKey;
use tokio::runtime::Runtime;

const NOW: u64 = 1_791_571_184;

thread_local! {
    /// This test's service key: a test runs on a thread of its own, and the
    /// service's mailboxes on the shared AIR server are the DID's, so a key shared
    /// by tests would have each one's listener read the earlier ones' requests.
    static SERVICE_SECRET: [u8; 32] = SecretKey::new(&mut OsRng).secret_bytes();
}

fn service_keypair() -> Keypair {
    let b = SERVICE_SECRET.with(|b| *b);
    Keypair::from_secret_key(SECP256K1, &SecretKey::from_slice(&b).unwrap())
}

/// The same key as the AIR listener's, made from the hex of `service_key.hex`
/// as `fsp-mint serve` makes it.
fn service_air_key() -> DidKey {
    let b = SERVICE_SECRET.with(|b| *b);
    addr::secret_from_hex(&hex::encode(b)).unwrap()
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

/// An HTTP API on a port of its own (only `/audit` and `/health` are left).
fn serve_http(rt: &Runtime, app: Router) -> String {
    let l = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let a = l.local_addr().unwrap();
    rt.spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{a}")
}

/// The ESP mint as fsp-mint reaches it: its DID and the AIR server (what the
/// `air.esp_*` settings name), its listener (which a test may stop and start
/// again, as an ESP mint that is down and comes back), and its HTTP base, which
/// only the tests read (`/audit`).
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
    fn serve(&self) -> Arc<esp_mint::air_listener::Serve> {
        let state = Arc::clone(&self.state);
        Arc::new(move |kind: u8, body: &[u8]| esp_mint::server::air_served(&state, kind, body))
    }

    /// Start the listener (the ESP mint is answering).
    fn listen(&self) {
        let listener = start_esp_listening(&self.server, &self.key, &self.data, self.serve());
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
            let now = esp_mint::air_listener::read_next(&self.data).unwrap();
            if now == want {
                return;
            }
            assert!(
                Instant::now() < until,
                "the ESP mint's listener did not take {want} mailbox slots within 15 s (it is at {now})"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// The `air.esp_*` settings that point fsp-mint at this ESP mint (and the
    /// listener's own `air.server` and `air.host`, which are the same AIR server
    /// here: the DIDs differ, so the mailboxes do).
    fn settings(&self, wait_secs: &str, attempts: &str) -> Vec<(&'static str, String)> {
        vec![
            ("air.esp_did", self.did.to_hex()),
            ("air.esp_server", self.server.name.clone()),
            ("air.esp_host", self.server.host.clone()),
            ("air.server", self.server.name.clone()),
            ("air.host", self.server.host.clone()),
            ("air.reply_wait_secs", wait_secs.to_string()),
            ("air.attempts", attempts.to_string()),
        ]
    }
}

/// A real ESP mint with its HTTP routes (for `/audit`) and its AIR listener, on
/// the shared AIR server. It is the mint `esp-mint serve` builds
/// (`EspState::open`): default config, the system clock, rotation on.
fn start_esp(rt: &Runtime, dir: &Path) -> (EspAt, Arc<EspState>) {
    let server = server();
    let data = dir.join("esp");
    let state = Arc::new(
        EspState::open(
            &data,
            Box::new(NoPsp),
            esp_mint::config::Config::default(),
            0,
        )
        .unwrap(),
    );
    let key = DidKey::new();
    let at = EspAt {
        url: serve_http(rt, esp_mint::server::router(Arc::clone(&state))),
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

/// Book in the ESP mint's ledger what a test signed straight from the mint
/// (`issue_with_key`), the way an exchange books the outputs it signs: the entry
/// is put and then finished with its movements in one write.
///
/// Why the note is booked at all: the tests need notes of 1, 2, 4 and 8 units on
/// chosen days, which the exchange (exact, at least 10^11 units) cannot give, so
/// they sign the note themselves. A note signed past the ledger leaves the ESP
/// mint's counters saying the ESP is outstanding and its ledger saying it is
/// not, so its `/audit` and a transfer of the note would disagree with what was
/// issued. Booking it makes the ESP mint look as it does after a real exchange.
/// This is esp-mint's own `tests/common::record_issued`, copied: tests cannot
/// share a helper across crates without putting test-only code in the library.
fn book_issued(state: &EspState, service: ServiceDid, day: u64, units: u64) {
    static BOOKED: AtomicU64 = AtomicU64::new(0);
    let hash = format!("fixture-issue-{}", BOOKED.fetch_add(1, Ordering::Relaxed));
    let mut movements = Movements::new();
    movements.issue(day, u128::from(units));
    let mut responses = state.responses.lock().unwrap();
    responses
        .put(
            &hash,
            esp_mint::responses::Entry::Pending {
                redeemable_after_day: day,
                inputs: vec![],
                outputs: vec![],
            },
        )
        .unwrap();
    responses
        .finish(
            &hash,
            esp_mint::responses::Entry::Done(ExchangeResponse {
                keyset: esp_mint::rotation::keyset_id(&state.master),
                service,
                redeemable_after_day: day,
                signatures: vec![],
                keys: vec![],
                fee_per_byte: 0,
                fee_esp: 0,
                size_bytes: 0,
            }),
            &movements,
        )
        .unwrap();
}

/// An ESP note the Issuer issued for `service` and `day`: signed with the ESP
/// mint's key for the amount and terms, and booked in its ledger.
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
    book_issued(state, service, day, units);
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
    /// fsp-mint's AIR listener. First, so that it stops before the rest goes.
    _listener: Listener,
    /// The app side: asks the service's DID over AIR.
    air: Air,
    /// The HTTP base of what is left: `/audit`.
    http: String,
    state: Arc<FspState>,
    clock: Arc<AtomicU64>,
}

impl Fsp {
    /// `POST route` over AIR: the status and JSON of the outcome.
    fn post(&self, route: &str, body: &impl serde::Serialize) -> (u16, serde_json::Value) {
        self.air.post_json(route, body)
    }

    /// `GET route` over AIR, which must be answered; its JSON as a `T`.
    fn get_as<T: serde::de::DeserializeOwned>(&self, route: &str) -> T {
        let (status, body) = self.air.get_json(route);
        assert_eq!(status, 200, "{route}: {body}");
        serde_json::from_value(body).unwrap()
    }
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
    start_fsp_with(rt, dir, esp, activation, wait_secs, attempts, &[])
}

/// [`start_fsp_waiting`] with more settings (`extra`, applied last).
fn start_fsp_with(
    rt: &Runtime,
    dir: &Path,
    esp: &EspAt,
    activation: &str,
    wait_secs: &str,
    attempts: &str,
    extra: &[(&str, &str)],
) -> Fsp {
    let mut cfg = Config::default();
    for (key, value) in esp.settings(wait_secs, attempts) {
        cfg.set(key, &value).unwrap();
    }
    cfg.set("service.did", &service().to_string()).unwrap();
    cfg.set("rotation.activation_hours", activation).unwrap();
    cfg.set("rotation.period_hours", "240").unwrap();
    for (key, value) in extra {
        cfg.set(key, value).unwrap();
    }
    // The mint first issued at NOW (the record a real first issue writes).
    std::fs::create_dir_all(dir.join("fsp")).unwrap();
    fsp_mint::keysets::record_first_issue(&dir.join("fsp"), NOW).unwrap();
    cfg.validate().unwrap();
    cfg.require_air().unwrap();
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
    // The listener `serve` starts: the service's own key, the network's data
    // folder for the slot counter, and the settings' AIR server.
    let key = service_air_key();
    let did = addr::key_to_did(&key.public_key());
    assert!(did == service(), "the AIR key is the service's DID key");
    let answering = Arc::clone(&state);
    let listener = start_listening(
        &esp.server,
        &key,
        &dir.join("fsp"),
        Arc::new(move |kind: u8, body: &[u8]| server::air_served(&answering, kind, body)),
    );
    Fsp {
        _listener: listener,
        air: Air::new(&esp.server, did, "main"),
        http: serve_http(rt, server::router(Arc::clone(&state))),
        state,
        clock,
    }
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
    let (status, body) = f.post("/issue", &req);
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
    let (status2, body2) = f.post("/issue", &req);
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
    let (s3, b3) = f.post("/issue", &again);
    // all three ESP notes (two in the first group, one in the second) are named,
    // counted across the groups in order
    assert_eq!(
        (s3, b3["code"].as_str()),
        (400, Some("note_spent_elsewhere")),
        "{b3}"
    );
    assert_eq!(b3["spent"], serde_json::json!([0, 1, 2]), "{b3}");

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
    let (s, body) = f.post("/spend", &spend);
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["paid"], 9);
    let (s, body_again) = f.post("/spend", &spend);
    assert_eq!(
        (s, &body_again),
        (200, &body),
        "a retry gets the stored answer"
    );
    let other = spend_request(keyset, &notes, &[Out::new(4).mint()], 9);
    let (s, b) = f.post("/spend", &other);
    assert_eq!(
        (s, b["code"].as_str()),
        (400, Some("note_spent_elsewhere")),
        "{b}"
    );
    assert_eq!(b["spent"], serde_json::json!([0, 1, 2]), "{b}");

    // Wrong service and not exact are refused before anything.
    let mut wrong = req.clone();
    wrong.service = ServiceDid::from(Keypair::new(SECP256K1, &mut OsRng).public_key());
    assert_eq!(f.post("/issue", &wrong).1["code"], "wrong_service");
    let mut inexact = req.clone();
    inexact.groups[1].outputs = vec![Out::new(4).mint()];
    assert_eq!(f.post("/issue", &inexact).1["code"], "not_exact");
}

/// Two ESP notes of a request were already transferred to this service by
/// earlier issues, neither of them the first: the refusal names both, counted
/// across the groups in order, and nothing else is transferred.
#[test]
fn two_transferred_esp_notes_not_at_the_start_are_both_named() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("issue-two-spent");
    let (esp_at, esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_at, "0");
    let day1 = Terms::new_at(service(), NOW).redeemable_after_day;
    let day2 = day1 + 30;
    let x0 = esp_note(&esp, 2, service(), day1);
    let x1 = esp_note(&esp, 1, service(), day1);
    let x2 = esp_note(&esp, 4, service(), day2);
    let x3 = esp_note(&esp, 8, service(), day2);
    // `x1` and `x2` are transferred first, each by an issue of its own.
    for (note, day, units) in [(x1.clone(), day1, 1), (x2.clone(), day2, 4)] {
        let one = IssueRequest {
            service: service(),
            groups: vec![IssueGroup {
                redeemable_after_day: day,
                notes: vec![note],
                outputs: vec![Out::new(units).mint()],
            }],
        };
        let (status, body) = f.post("/issue", &one);
        assert_eq!(status, 200, "{body}");
    }
    // Both days in one request, `x1` second and `x2` third counted across the
    // groups: refused whole, both named.
    let all = IssueRequest {
        service: service(),
        groups: vec![
            IssueGroup {
                redeemable_after_day: day1,
                notes: vec![x0.clone(), x1.clone()],
                outputs: vec![Out::new(2).mint(), Out::new(1).mint()],
            },
            IssueGroup {
                redeemable_after_day: day2,
                notes: vec![x2.clone(), x3.clone()],
                outputs: vec![Out::new(4).mint(), Out::new(8).mint()],
            },
        ],
    };
    let (status, body) = f.post("/issue", &all);
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "note_spent_elsewhere", "{body}");
    assert_eq!(body["spent"], serde_json::json!([1, 2]), "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("ESP notes 1, 2 of this request were already transferred"),
        "{body}"
    );
    // Only the two single issues kept ESP.
    assert_eq!(f.state.issues.lock().unwrap().data.kept.len(), 2);
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
    let (s, body) = f.post("/issue", &req);
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
    let before: RotationResponse = f.get_as("/rotation");
    assert!(!before.active && before.previous.is_none());
    assert_eq!(before.current.current_until, Some(NOW + 5 * 86_400));
    let (n, s) = fsp_note(&genesis, 16);
    let (n2, s2) = fsp_note(&genesis, 16);
    f.clock.store(NOW + 5 * 86_400, Ordering::SeqCst);
    let keys: KeysResponse = f.get_as("/keys");
    assert_eq!(keys.previous.as_deref(), Some(genesis.id.as_str()));
    assert_ne!(keys.current, genesis.id);
    assert_eq!(keys.next_rotation, Some(NOW + 15 * 86_400));
    let w: RotationResponse = f.get_as("/rotation");
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
    let (st, body) = f.post(
        "/spend",
        &spend_request(&genesis.id, &[(n, s, 16)], &[o.mint()], 0),
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["keyset"], keys.current.as_str());
    // A full period later the genesis set is gone with its spent list.
    f.clock.store(NOW + 15 * 86_400, Ordering::SeqCst);
    let (st, body) = f.post(
        "/spend",
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
    assert_eq!(e.code, "note_spent_elsewhere");
    assert_eq!(e.spent, vec![1]);
    assert_eq!(e.why, "note 1 is already spent");
    assert!(sp.is_spent(&k.id, &a.nonce.0.serialize()));
    assert!(!sp.is_spent(&k.id, &b.nonce.0.serialize()));
    assert!(!sp.is_spent(&k.id, &c.nonce.0.serialize()));
}

#[test]
fn two_spent_notes_not_at_the_start_are_both_named_and_nothing_else_is_spent() {
    let dir = scratch("two-spent").join("fsp");
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
    let (d, sd) = fsp_note(&k, 2);
    // `b` and `c` are spent first, each alone.
    for (n, s) in [(b, sb), (c, sc)] {
        let one = spend_request(&k.id, &[(n, s, 2)], &[Out::new(1).mint()], 1);
        rt.block_on(sp.spend(&one, NOW)).unwrap();
    }
    // Four notes, the second and the third already spent: refused, and both are
    // named, so the wallet drops them in one go. `a` and `d` stay unspent.
    let all = spend_request(
        &k.id,
        &[(a, sa, 2), (b, sb, 2), (c, sc, 2), (d, sd, 2)],
        &[Out::new(8).mint()],
        0,
    );
    let e = rt.block_on(sp.spend(&all, NOW)).unwrap_err();
    assert_eq!(e.status, 400);
    assert_eq!(e.code, "note_spent_elsewhere");
    assert_eq!(e.spent, vec![1, 2]);
    assert_eq!(e.why, "notes 1, 2 are already spent");
    assert!(!sp.is_spent(&k.id, &a.nonce.0.serialize()));
    assert!(!sp.is_spent(&k.id, &d.nonce.0.serialize()));
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
    let fees: FeesResponse = f.get_as("/fees");
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

/// The ceiling is the setting (`server.max_request_bytes`, 16384 at the least):
/// over it a call is refused before it is parsed, with the `413` JSON the HTTP
/// route had and the `http_status` a refusal carries over AIR; at it the body is
/// read, and is not a request, so a `400`.
#[test]
fn a_body_over_the_ceiling_is_refused_before_parsing() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("limit");
    let (esp_at, _esp) = start_esp(&rt, &dir);
    let f = start_fsp_with(
        &rt,
        &dir,
        &esp_at,
        "0",
        "20",
        "2",
        &[("server.max_request_bytes", "16384")],
    );
    for route in ["/issue", "/spend"] {
        // One byte over: refused for its size, not for its content.
        let big = vec![b' '; 16385];
        let (status, body) = f.air.call(route, &big);
        assert_eq!(status, 413, "{route}: {body}");
        assert_eq!(body["code"], "request_too_large", "{route}");
        assert_eq!(body["limit_bytes"], 16384, "{route}");
        assert_eq!(body["size_bytes"], 16385, "{route}");
        // At the limit the body is read and is not a request.
        let at = vec![b' '; 16384];
        let (status, body) = f.air.call(route, &at);
        assert_eq!(status, 400, "{route}: {body}");
        assert_eq!(body["code"], "malformed_request", "{route}");
    }
    // Nothing was issued or spent by any of it.
    assert_eq!(f.state.issues.lock().unwrap().data.kept.len(), 0);
    let w: RotationResponse = f.get_as("/rotation");
    assert!(
        !w.active
            && w.activation.is_none()
            && w.rotation_activation_hours == 0
            && w.next_rotation.is_none()
            && w.current.retires.is_none()
    );
    assert_eq!(w.rotation_period_hours, 240);
    // The mint info is the health JSON.
    let h: serde_json::Value = f.get_as("/health");
    assert_eq!(h["status"], "ok");
    assert_eq!(h["network"], "main");
    // The wire format this mint speaks; a wallet refuses a mint on another number.
    assert_eq!(h["wire_version"], sp_wire::WIRE_VERSION);
}

/// A refused spend is a signed refusal with the route's JSON (a `400`, no
/// `http_status`), and nothing is spent by it.
#[test]
fn a_refused_spend_is_a_signed_refusal_with_the_routes_json() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("refused-spend");
    let (esp_at, _esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_at, "0");
    let k = current(&f);
    let (n, s) = fsp_note(&k, 4);
    // No notes: refused before anything else.
    let none = spend_request(&k.id, &[], &[Out::new(1).mint()], 1);
    let (status, body) = f.post("/spend", &none);
    assert_eq!(
        (status, body["code"].as_str()),
        (400, Some("no_inputs")),
        "{body}"
    );
    assert!(
        body.get("http_status").is_none(),
        "a 400 has the route's JSON as it is"
    );
    // A memo over the setting (256 bytes): refused, the note is not spent.
    let mut long = spend_request(&k.id, &[(n, s, 4)], &[Out::new(1).mint()], 3);
    long.memo = "x".repeat(300);
    let (status, body) = f.post("/spend", &long);
    assert_eq!(
        (status, body["code"].as_str()),
        (400, Some("memo_too_long")),
        "{body}"
    );
    assert!(!f.state.spender.is_spent(&k.id, &n.nonce.0.serialize()));
    // A note the mint never signed: refused, and still not spent.
    let other_keys = fsp_mint::keysets::Keysets::open(&scratch("other-keys")).unwrap();
    let (forged, fs) = fsp_note(other_keys.current(), 4);
    let (status, body) = f.post(
        "/spend",
        &spend_request(&k.id, &[(forged, fs, 4)], &[Out::new(1).mint()], 3),
    );
    assert!((400..500).contains(&status), "{status}: {body}");
    assert!(body["code"].is_string(), "{body}");
    assert!(!f.state.spender.is_spent(&k.id, &forged.nonce.0.serialize()));
}

/// Over HTTP only `/audit` and `/health` answer (the operator's report and the
/// local health monitor's check): the routes wallets used (they use AIR) are gone,
/// and both are read-only.
#[test]
fn only_the_audit_and_the_health_check_answer_over_http() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("http-left");
    let (esp_at, _esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_at, "0");
    let http = reqwest::blocking::Client::new();
    for route in ["keys", "fees", "rotation"] {
        let r = http.get(format!("{}/{route}", f.http)).send().unwrap();
        assert_eq!(r.status().as_u16(), 404, "GET /{route}");
    }
    for route in ["issue", "spend"] {
        let r = http
            .post(format!("{}/{route}", f.http))
            .body("{}")
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 404, "POST /{route}");
    }
    let r = http.get(format!("{}/health", f.http)).send().unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let h: serde_json::Value = r.json().unwrap();
    assert_eq!(h["status"], "ok");
    assert_eq!(h["wire_version"], sp_wire::WIRE_VERSION);
    let r = http.post(format!("{}/health", f.http)).send().unwrap();
    assert_eq!(r.status().as_u16(), 405, "/health reads only");
    let r = http.get(format!("{}/audit", f.http)).send().unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let report: Report = r.json().unwrap();
    assert!(report.consistent, "{:?}", report.warnings);
    let r = http.post(format!("{}/audit", f.http)).send().unwrap();
    assert_eq!(r.status().as_u16(), 405, "/audit reads only");
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
    let keys: KeysResponse = f.get_as("/keys");

    // Spend: signed by the current set, named in the reply, and the note
    // unblinds with that set's published key.
    let (st, body) = f.post(
        "/spend",
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
    let (st, body) = f.post("/issue", &req);
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
    let (st, again) = f.post("/issue", &req);
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
    let (s, body) = f.post("/issue", &req);
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
    reqwest::blocking::get(format!("{}/audit", f.http))
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
    assert!(empty.consistent, "{:?}", empty.warnings);

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
    let (s, body) = f.post("/issue", &req);
    assert_eq!(s, 200, "{body}");
    let (s, again) = f.post("/issue", &req);
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
    assert!(r.consistent, "{:?}", r.warnings);

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
    assert_eq!(f.post("/spend", &spend).0, 200);
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
    assert_eq!(f.post("/spend", &spend).0, 200);
    let other = spend_request(&ks, &notes, &[Out::new(4).mint()], 9);
    assert_eq!(f.post("/spend", &other).0, 400);
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
    let (st, body) = f.post(
        "/spend",
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
    let (st, _) = f.post(
        "/spend",
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
    assert!(after.consistent, "{:?}", after.warnings);
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

// ---- export-esp ----

/// A kept ESP note as `issues.json` holds it (real types, not JSON text).
fn kept(amount: u64, day: u64, keyset: &str, request: &str) -> KeptEsp {
    let kp = Keypair::new(SECP256K1, &mut OsRng);
    KeptEsp {
        amount,
        note: Note {
            nonce: Nonce(kp.public_key()),
            signature: Keypair::new(SECP256K1, &mut OsRng).public_key(),
        },
        spend_secret: kp.secret_key(),
        redeemable_after_day: day,
        keyset: keyset.into(),
        request: request.into(),
        exported: false,
    }
}

#[test]
fn export_esp_writes_a_handover_file_for_each_day_once_and_the_audit_still_counts_the_esp() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("export");
    let (esp_at, esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_at, "0");
    let data = dir.join("fsp");
    let day1 = Terms::new_at(service(), NOW).redeemable_after_day;
    let day2 = day1 + 30;
    issue_one(&esp, &f, 4, day1);
    issue_one(&esp, &f, 2, day1);
    issue_one(&esp, &f, 8, day2);
    let before = audit::from_dir(&data, NOW).unwrap();
    assert_eq!(
        (
            before.esp_reserve.total.notes,
            units(&before.esp_reserve.total.esp)
        ),
        (3, 14)
    );
    assert_eq!(units(&before.esp_reserve.total.exported_esp), 0);

    // One file for each day, in the folder the export makes.
    let out = scratch("export-out").join("handover");
    let written = export_esp(&data, &out, service(), 1_234).unwrap();
    let file1 = out.join(format!("esp-{day1}-1234.json"));
    let file2 = out.join(format!("esp-{day2}-1234.json"));
    assert_eq!(written, vec![(file1.clone(), 2), (file2.clone(), 1)]);

    // Each file reads back as the hand-over type, with the service, the day and
    // every note with the ESP keyset that signed it.
    let esp_keyset = esp_mint::rotation::keyset_id(&esp.master);
    let kept_now = Issues::open(&data).unwrap().data.kept;
    assert_eq!(kept_now.len(), 3);
    for (file, day, amounts) in [(&file1, day1, vec![4, 2]), (&file2, day2, vec![8])] {
        let handover = Handover::read(&std::fs::read(file).unwrap()).unwrap();
        assert_eq!(handover.service, service());
        assert_eq!(handover.redeemable_after_day, day);
        assert_eq!(
            handover.notes.iter().map(|n| n.amount).collect::<Vec<_>>(),
            amounts
        );
        for n in &handover.notes {
            assert_eq!(n.keyset, esp_keyset);
            // It is the note the service kept, with the secret that spends it.
            let k = kept_now
                .iter()
                .find(|k| k.note.nonce == n.note.nonce)
                .expect("a kept note");
            assert_eq!(
                (k.amount, k.note, k.spend_secret, k.redeemable_after_day),
                (n.amount, n.note, n.spend_secret, day)
            );
        }
    }

    // The notes are marked exported, and a second run writes nothing.
    assert!(kept_now.iter().all(|k| k.exported));
    let again = export_esp(&data, &out, service(), 1_235).unwrap();
    assert!(again.is_empty(), "{again:?}");
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 2);

    // The audit counts the exported ESP as before, and shows it as exported.
    let after = audit::from_dir(&data, NOW).unwrap();
    assert_eq!(
        (
            after.esp_reserve.total.notes,
            units(&after.esp_reserve.total.esp)
        ),
        (3, 14)
    );
    assert_eq!(units(&after.esp_reserve.total.exported_esp), 14);
}

#[test]
fn export_esp_refuses_a_kept_note_with_no_keyset_and_names_it_before_writing_anything() {
    let data = scratch("export-old");
    let out = scratch("export-old-out").join("handover");
    let old = kept(4, 20_500, "", "old-request");
    let new = kept(2, 20_501, "0102030405060708", "new-request");
    let issues = data.join("issues.json");
    let write = |notes: Vec<KeptEsp>| {
        store::save(
            &issues,
            &IssueData {
                kept: notes,
                ..Default::default()
            },
        )
        .unwrap()
    };
    write(vec![old.clone(), new.clone()]);

    let e = export_esp(&data, &out, service(), 1_234).unwrap_err();
    assert_eq!(
        e,
        format!(
            "cannot export: 1 kept ESP note names no keyset (kept before keysets), and a hand-over file needs one on every note. Nothing was written. note {} (4 units, redeemable after day 20500, from issue request old-request)",
            hex::encode(old.note.nonce.0.serialize())
        )
    );
    // Nothing was written, and no note was marked exported (the one with a
    // keyset, on another day, included).
    assert!(!out.exists());
    let after: IssueData = store::load(&issues).unwrap().unwrap();
    assert_eq!(after.kept.len(), 2);
    assert!(after.kept.iter().all(|k| !k.exported));

    // With the keyset on that note too, the same notes export.
    let mut filled = old.clone();
    filled.keyset = "0a0b0c0d0e0f1011".into();
    write(vec![filled, new]);
    let written = export_esp(&data, &out, service(), 1_234).unwrap();
    assert_eq!(
        written,
        vec![
            (out.join("esp-20500-1234.json"), 1),
            (out.join("esp-20501-1234.json"), 1)
        ]
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

/// `issues.json` as the types that write it.
fn issues_file(dir: &Path) -> IssueData {
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
    let (s, body) = f.post("/issue", &first);
    assert_eq!(s, 200, "{body}");
    esp_at.wait_for_next(2);

    // The ESP mint goes quiet. The keyset is cached, so the transfer is sent,
    // written to slot 2, and not answered.
    esp_at.silence();
    let second = one_group(&esp, 4, day);
    let hash = second.hash();
    let (s, body) = f.post("/issue", &second);
    // A signed "cannot answer now": the 502 of the HTTP route, a 5xx, so the
    // wallet keeps the issue pending and its retry gets the stored answer.
    assert_eq!(s, 503, "{body}");
    assert_eq!(f.state.issues.lock().unwrap().data.kept.len(), 1);
    // The request in flight is kept in `issues.json`, beside its transfer.
    let file = issues_file(&dir);
    let Some(Entry::Pending(pending)) = file.entries.get(&hash) else {
        panic!("expected a pending entry: {file:?}");
    };
    assert_eq!(pending.groups.len(), 1, "{file:?}");
    let air = pending.groups[0].air.clone().expect("a request in flight");
    assert_eq!(air.mint, esp_at.did.to_hex(), "{file:?}");
    assert_eq!(air.slots, vec![2], "{file:?}");
    assert_eq!(air.request_id.len(), 64, "{file:?}");

    // The ESP mint comes back and answers the request that was waiting.
    esp_at.listen();
    esp_at.wait_for_next(3);

    // A restart of fsp-mint, then the retry: it reads the reply cell of the same
    // request, finds the answer, and writes nothing.
    drop(f);
    let f = start_fsp(&rt, &dir, &esp_at, "0");
    let (s, body) = f.post("/issue", &second);
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["groups"][0]["result"], "issued", "{body}");
    assert_eq!(f.state.issues.lock().unwrap().data.kept.len(), 2);
    // Done: the stored answer, and no request kept.
    let file = issues_file(&dir);
    assert!(
        matches!(file.entries.get(&hash), Some(Entry::Done(_))),
        "{file:?}"
    );
    assert!(
        file.entries.values().all(|e| matches!(e, Entry::Done(_))),
        "a request is still kept: {file:?}"
    );
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

/// The five AIR settings of `serve` as environment: the ESP mint's (DID, AIR
/// server, host) and this mint's own listener's (AIR server, host).
fn air_env<'a>(esp_at: &'a EspAt, did: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("FSP_AIR_ESP_DID", did),
        ("FSP_AIR_ESP_SERVER", esp_at.server.name.as_str()),
        ("FSP_AIR_ESP_HOST", esp_at.server.host.as_str()),
        ("FSP_AIR_SERVER", esp_at.server.name.as_str()),
        ("FSP_AIR_HOST", esp_at.server.host.as_str()),
        ("FSP_AIR_REPLY_WAIT_SECS", "20"),
        ("FSP_AIR_ATTEMPTS", "2"),
    ]
}

#[test]
fn serve_refuses_to_start_when_the_esp_mint_is_on_another_network() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("mismatch");
    let (esp_at, _esp) = start_esp(&rt, &dir); // reports "main"
    let base = scratch("mismatch-run");
    let did = esp_at.did.to_hex();
    let mut air = air_env(&esp_at, &did);
    air.push(("FSP_NETWORK", "regtest"));
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
    // None of the five.
    let out = fsp_run(&base, &["serve"], &[]);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "serve started");
    for missing in [
        "air.esp_did",
        "air.esp_server",
        "air.esp_host",
        "air.server",
        "air.host",
    ] {
        assert!(err.contains(missing), "{missing} is not named: {err}");
    }
    // Only the ESP host set: the other four are named, that one is not.
    let out = fsp_run(&base, &["serve"], &[("FSP_AIR_ESP_HOST", "127.0.0.1")]);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "serve started");
    for missing in [
        "(FSP_AIR_ESP_DID)",
        "(FSP_AIR_ESP_SERVER)",
        "(FSP_AIR_SERVER)",
        "(FSP_AIR_HOST)",
    ] {
        assert!(err.contains(missing), "{missing} is not named: {err}");
    }
    assert!(!err.contains("(FSP_AIR_ESP_HOST)"), "{err}");
    // Only the ESP mint's server missing (the other four set): that one is
    // named, none of the others is, and the message says it is not set.
    let did = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
    let out = fsp_run(
        &base,
        &["serve"],
        &[
            ("FSP_AIR_ESP_DID", did),
            ("FSP_AIR_ESP_HOST", "127.0.0.1"),
            ("FSP_AIR_SERVER", did),
            ("FSP_AIR_HOST", "127.0.0.1"),
        ],
    );
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "serve started");
    assert!(err.contains("FSP_AIR_ESP_SERVER"), "{err}");
    assert!(
        !err.contains("FSP_AIR_ESP_HOST")
            && !err.contains("FSP_AIR_ESP_DID")
            && !err.contains("(FSP_AIR_SERVER)")
            && !err.contains("(FSP_AIR_HOST)"),
        "{err}"
    );
    assert!(err.contains("is not set"), "{err}");
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

    // With all five set the check passes, and the listener takes its own server
    // and host. `serve` goes on to the network, which needs an AIR server, so
    // this half calls the same `Config` functions directly.
    let mut c = Config::default();
    c.set("air.esp_did", did).unwrap();
    c.set("air.esp_server", did).unwrap();
    c.set("air.esp_host", "127.0.0.1").unwrap();
    c.set("air.server", did).unwrap();
    c.set("air.host", "127.0.0.1").unwrap();
    c.require_air().unwrap();
    let l = c.air_listener(Path::new("/x"));
    assert_eq!((l.server.host.as_str(), l.held_reads), ("127.0.0.1", 16));
    c.set("air.esp_host", "10.0.0.9").unwrap();
    assert_eq!(
        c.air_listener(Path::new("/x")).server.host,
        "127.0.0.1",
        "the listener's host is air.host, not the ESP mint's"
    );
}

/// The listener always runs, so `serve` needs its settings even when the ESP
/// mint's are all there: it names the two that are missing, and only those.
#[test]
fn serve_refuses_to_start_without_its_own_listener_settings() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("no-listener");
    let (esp_at, _esp) = start_esp(&rt, &dir);
    let base = scratch("no-listener-run");
    let did = esp_at.did.to_hex();
    let env: Vec<(&str, &str)> = air_env(&esp_at, &did)
        .into_iter()
        .filter(|(k, _)| *k != "FSP_AIR_SERVER" && *k != "FSP_AIR_HOST")
        .collect();
    let out = fsp_run(&base, &["serve"], &env);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "serve started: {err}");
    assert!(
        err.contains("air.server (FSP_AIR_SERVER)") && err.contains("air.host (FSP_AIR_HOST)"),
        "{err}"
    );
    for set in [
        "(FSP_AIR_ESP_DID)",
        "(FSP_AIR_ESP_SERVER)",
        "(FSP_AIR_ESP_HOST)",
    ] {
        assert!(!err.contains(set), "{set} is set and is named: {err}");
    }
}

/// What is left on `server.bind` is for the operator on this machine.
#[test]
fn serve_refuses_a_bind_that_is_not_loopback() {
    let base = scratch("bind");
    let out = fsp_run(&base, &["serve"], &[("FSP_BIND", "0.0.0.0:3200")]);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "serve started: {err}");
    assert!(
        err.contains("server.bind") && err.contains("not a loopback address"),
        "{err}"
    );

    // Loopback addresses are accepted. `serve` would go on to the AIR settings,
    // so this half calls the same `Config::validate` directly.
    let mut c = Config::default();
    c.set("server.bind", "[::1]:3200").unwrap();
    c.validate().unwrap();
    c.set("server.bind", "127.0.0.1:3200").unwrap();
    c.validate().unwrap();
}
