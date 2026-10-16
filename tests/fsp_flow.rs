//! fsp-mint end to end: a real ESP mint (the Issuer) in-process, fsp-mint in
//! front of it, and the app side built the way the wallet builds it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use esp_mint::exchange::{Burn, Psp};
use esp_mint::responses::Responses;
use esp_mint::server::EspState;
use esp_mint::terms::Terms;
use fsp_mint::api::*;
use fsp_mint::esp_client::HttpIssuer;
use fsp_mint::issue::Issues;
use fsp_mint::keysets::Keyset;
use fsp_mint::server::{self, FspState};
use fsp_mint::spend::Spender;
use mint_types::{Amount, BlindNonce, MintInput, MintOutput, Nonce, Note};
use rand::rngs::OsRng;
use secp256k1::{Keypair, PublicKey, SECP256K1, SecretKey, XOnlyPublicKey};
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

fn service() -> XOnlyPublicKey {
    service_keypair().x_only_public_key().0
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

fn start_esp(rt: &Runtime, dir: &PathBuf) -> (String, Arc<EspState>) {
    let (logic, master) = esp_mint::mint::open(&dir.join("esp")).unwrap();
    let responses = Responses::open(&dir.join("esp").join("responses.json")).unwrap();
    let state = Arc::new(EspState {
        logic: Mutex::new(logic),
        master,
        psp: Box::new(NoPsp),
        responses: Mutex::new(responses),
        queue_path: dir.join("esp").join("queue.json"),
        queue_lock: Mutex::new(()),
        in_flight: Default::default(),
        config: esp_mint::config::Handle::default(),
        rotor: None,
        rollovers: None,
    });
    (
        serve(rt, esp_mint::server::router(Arc::clone(&state))),
        state,
    )
}

/// An ESP note the Issuer issued for `service` and `day`.
fn esp_note(state: &EspState, units: u64, service: XOnlyPublicKey, day: u64) -> HandedEsp {
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

fn start_fsp(rt: &Runtime, dir: &PathBuf, esp_url: &str, activation: &str) -> Fsp {
    let mut cfg = fsp_mint::config::Config::default();
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
    let c = Arc::clone(&clock);
    let state = Arc::new(FspState {
        config: cfg,
        spender,
        issues: Mutex::new(issues),
        issuer: Box::new(HttpIssuer::new(esp_url, 10).unwrap()),
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
    let (esp_url, esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_url, "0");
    let day1 = Terms::new_at(service(), NOW).redeemable_after_day;
    let day2 = day1 + 30; // not mature: FSP ignores maturity
    let a = vec![
        esp_note(&esp, 4, service(), day1),
        esp_note(&esp, 1, service(), day1),
    ];
    let b = vec![esp_note(&esp, 8, service(), day2)];
    let outs_a = vec![Out::new(4), Out::new(1)];
    let outs_b = vec![Out::new(8)];
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
    let audit: serde_json::Value = reqwest::blocking::get(format!("{esp_url}/audit"))
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
    wrong.service = Keypair::new(SECP256K1, &mut OsRng).x_only_public_key().0;
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

#[test]
fn an_esp_note_for_another_service_or_day_is_refused_by_the_issuer_for_its_group_only() {
    let rt = Runtime::new().unwrap();
    let dir = scratch("partial");
    let (esp_url, esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_url, "0");
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
    let (esp_url, _esp) = start_esp(&rt, &dir);
    // Activation 120 hours after the first issue (NOW), period 240 hours.
    let f = start_fsp(&rt, &dir, &esp_url, "120");
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
    let (esp_url, _esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_url, "0");
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
    let (esp_url, _esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_url, "0");
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
    let (esp_url, esp) = start_esp(&rt, &dir);
    let f = start_fsp(&rt, &dir, &esp_url, "120");
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
