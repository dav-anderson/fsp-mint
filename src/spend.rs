//! The spend call (over AIR, kind 12), the fast path: a swap with change.
//!
//! **Spent lists.** One per live keyset, held in memory (`HashSet` of note
//! nonces). Each spend is also one JSON line in the log of the keyset that was
//! current when it was accepted (`spends-<keyset>.jsonl`), with the nonces it
//! spent (and their keysets) and its answer, so that a retry gets the stored
//! answer. At startup the logs of the live keysets are read back into the
//! lists. When a keyset is deleted at rotation, its list and its log go with it.
//!
//! **Batching (group commit).** A spend is checked, its notes are marked spent
//! in memory (so a second spend of them is refused at once), its outputs are
//! signed, and its line is handed to one writer thread. The writer takes every
//! line waiting (up to `spend.max_batch`), appends them, syncs the files once,
//! and only then are those spends answered. There is no flush timer: a batch is
//! whatever arrived while the last sync ran, so a lone spend waits for one sync
//! and a busy mint shares each sync among many spends.
//!
//! **Replay window: none.** No spend is answered before its line is on disk.
//! A crash can lose only lines whose spends were never answered: their notes are
//! unspent again after the restart, and no signature for them left the mint
//! (the payer retries the same request). If a write fails, the mint stops
//! taking spends (`/health` reports it) and keeps those notes marked spent in
//! memory until it restarts.
//!
//! Check order: shape and amounts (no lock), stored answer, keysets and
//! signatures (no lock), then the spent lists (under the lock), then sign and
//! write.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use secp256k1::SECP256K1;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::api::{SpendRequest, SpendResponse};
use crate::audit::{Counters, Ledger, SpendView};
use crate::keysets::{Keyset, Keysets, Rotation};

pub fn log_name(keyset: &str) -> String {
    format!("spends-{keyset}.jsonl")
}

#[derive(Serialize, Deserialize)]
struct Record {
    h: String,
    n: Vec<(String, String)>,
    r: SpendResponse,
    /// The amounts, for the audit.
    a: Amounts,
}

/// The units of a spend: of each note in `n` (same order) and of each output.
#[derive(Serialize, Deserialize)]
struct Amounts {
    i: Vec<u64>,
    o: Vec<u64>,
}

/// Add one logged spend to the counters: its notes as spent, under the key
/// set of each; its change under the key set that signed it (`r.keyset`).
fn apply(stats: &mut HashMap<String, Counters>, record: &Record) {
    let a = &record.a;
    for ((keyset, _), units) in record.n.iter().zip(&a.i) {
        stats.entry(keyset.clone()).or_default().spent.0 += *units as i128;
    }
    stats.entry(record.r.keyset.clone()).or_default().change.0 +=
        a.o.iter().map(|u| *u as i128).sum::<i128>();
}

struct Answer {
    response: SpendResponse,
    durable: bool,
    file: String,
}

pub struct Core {
    dir: PathBuf,
    keysets: Keysets,
    live: Vec<Arc<Keyset>>,
    spent: HashMap<String, HashSet<[u8; 33]>>,
    answers: HashMap<String, Answer>,
    rotation: Rotation,
    /// The audit counters of the live key sets.
    stats: HashMap<String, Counters>,
    /// The counters of deleted key sets.
    ledger: Ledger,
}

impl Core {
    fn refresh(&mut self) {
        self.live = self.keysets.all().iter().cloned().map(Arc::new).collect();
    }

    fn current(&self) -> Arc<Keyset> {
        Arc::clone(self.live.last().expect("a keyset"))
    }

    /// `keyset_retired` for a deleted keyset on the retired list, else
    /// `unknown_keyset`.
    fn not_live(&self, i: usize, id: &str) -> SpendError {
        if self.keysets.is_retired(id) {
            refuse(
                sp_wire::codes::KEYSET_RETIRED,
                format!(
                    "note {i}: keyset {id} was rotated out and deleted; the note expired and cannot be spent"
                ),
            )
        } else {
            refuse(
                sp_wire::codes::UNKNOWN_KEYSET,
                format!("note {i}: keyset {id} is not a keyset of this mint"),
            )
        }
    }

    fn find(&self, id: &str) -> Option<Arc<Keyset>> {
        self.live.iter().find(|k| k.id == id).cloned()
    }

    /// Rotate if the epoch moved on. Returns the deleted keyset ids.
    fn rotate(&mut self, now: u64) -> io::Result<Vec<String>> {
        let deleted = self.keysets.rotate_to(self.rotation.epoch_at(now))?;
        // The counters of a deleted key set go to the ledger before its log
        // is deleted (the caller sends the deletes only if this returns Ok; a
        // failed save leaves the logs for the next start to fold).
        let mut folded = Ok(());
        let mut changed = false;
        for d in &deleted {
            if let Some(c) = self.stats.remove(d) {
                self.ledger.retired.entry(d.clone()).or_insert(c);
                changed = true;
            }
        }
        if changed {
            folded = self.ledger.save(&self.dir);
        }
        if !deleted.is_empty()
            || self.live.len() != self.keysets.all().len()
            || self.live.last().map(|k| k.id.as_str()) != Some(self.keysets.current().id.as_str())
        {
            for d in &deleted {
                self.spent.remove(d);
                self.answers.retain(|_, a| &a.file != d);
            }
            self.refresh();
        }
        folded?;
        Ok(deleted)
    }
}

enum Msg {
    Write {
        file: String,
        hash: String,
        line: Vec<u8>,
        done: oneshot::Sender<Result<(), String>>,
    },
    Delete(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendError {
    pub status: u16,
    pub code: &'static str,
    pub why: String,
    /// With `note_spent_elsewhere`: the positions of every input that is
    /// already spent. Empty for every other refusal.
    pub spent: Vec<usize>,
}

/// The refusal for inputs that are already spent: `note_spent_elsewhere`, with
/// every spent position in `spent` and in the text.
fn spent_elsewhere(spent: Vec<usize>) -> SpendError {
    let list: Vec<String> = spent.iter().map(|i| i.to_string()).collect();
    let why = if spent.len() == 1 {
        format!("note {} is already spent", list[0])
    } else {
        format!("notes {} are already spent", list.join(", "))
    };
    SpendError {
        status: 400,
        code: sp_wire::codes::NOTE_SPENT_ELSEWHERE,
        why,
        spent,
    }
}

fn refuse(code: &'static str, why: impl Into<String>) -> SpendError {
    SpendError {
        status: 400,
        code,
        why: why.into(),
        spent: Vec::new(),
    }
}

pub struct Spender {
    dir: PathBuf,
    core: Arc<Mutex<Core>>,
    tx: Mutex<mpsc::Sender<Msg>>,
    broken: Arc<AtomicBool>,
    /// `spend.max_items`; the default is only a default (see `config`).
    max_items: usize,
    /// `spend.max_memo_bytes`; likewise only a default here.
    max_memo_bytes: usize,
}

/// Read a log. A last line that a crash left half written is cut off when
/// `repair` is set (the mint at start-up), and only ignored otherwise (a reader
/// next to a running mint, which may be in the middle of writing it).
fn read_log(path: &Path, repair: bool) -> io::Result<Vec<Record>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut records = Vec::new();
    let mut good_len: u64 = 0;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            // The last line, never finished: its spend was never answered.
            if repair {
                OpenOptions::new()
                    .write(true)
                    .open(path)?
                    .set_len(good_len)?;
            }
            break;
        }
        let record: Record = serde_json::from_slice(&line[..line.len() - 1]).map_err(|e| {
            io::Error::other(format!(
                "{} is damaged after byte {good_len}: {e}",
                path.display()
            ))
        })?;
        records.push(record);
        good_len += n as u64;
    }
    Ok(records)
}

/// The audit counters of every log in a directory, by key set, without
/// changing anything (a half-written last line is ignored). For the audit
/// command; the mint itself builds the same counters in `Spender::open`.
pub fn replay_counters(dir: &Path) -> io::Result<HashMap<String, Counters>> {
    let mut stats = HashMap::new();
    let mut names = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(stats),
        Err(e) => return Err(e),
    };
    for entry in entries {
        names.push(entry?.file_name().to_string_lossy().to_string());
    }
    names.sort();
    for name in names {
        if name.starts_with("spends-") && name.ends_with(".jsonl") {
            for record in read_log(&dir.join(&name), false)? {
                apply(&mut stats, &record);
            }
        }
    }
    Ok(stats)
}

fn parse_nonce(hex_text: &str) -> Option<[u8; 33]> {
    hex::decode(hex_text)
        .ok()
        .and_then(|b| <[u8; 33]>::try_from(b).ok())
}

impl Spender {
    /// Open the keysets and the spent logs, rotate if due, and start the writer.
    pub fn open(
        data_dir: &Path,
        rotation: Rotation,
        max_batch: usize,
        now: u64,
    ) -> io::Result<Self> {
        let mut rotation = rotation;
        rotation.first_issue = crate::keysets::load_first_issue(data_dir)?;
        let mut keysets = Keysets::open(data_dir)?;
        keysets.rotate_to(rotation.epoch_at(now))?;
        let live_ids: HashSet<String> = keysets.all().iter().map(|k| k.id.clone()).collect();
        // Every log on disk is read: the live ones for the spent lists and the
        // stored answers, and all of them for the audit counters. A key set
        // that is gone (deleted just now, or in a crash during a rotation) has
        // its counters copied to the ledger before its log is removed.
        let mut ids = Vec::new();
        for entry in fs::read_dir(data_dir)? {
            let name = entry?.file_name().to_string_lossy().to_string();
            if let Some(id) = name
                .strip_prefix("spends-")
                .and_then(|n| n.strip_suffix(".jsonl"))
            {
                ids.push(id.to_string());
            }
        }
        ids.sort();
        let mut spent: HashMap<String, HashSet<[u8; 33]>> = HashMap::new();
        let mut answers = HashMap::new();
        let mut stats: HashMap<String, Counters> = HashMap::new();
        for id in &ids {
            for record in read_log(&data_dir.join(log_name(id)), true)? {
                apply(&mut stats, &record);
                if !live_ids.contains(id) {
                    continue;
                }
                for (ks, n) in &record.n {
                    if live_ids.contains(ks) {
                        if let Some(n) = parse_nonce(n) {
                            spent.entry(ks.clone()).or_default().insert(n);
                        }
                    }
                }
                answers.insert(
                    record.h,
                    Answer {
                        response: record.r,
                        durable: true,
                        file: id.clone(),
                    },
                );
            }
        }
        let mut ledger = Ledger::load(data_dir)?;
        let mut folded = false;
        for (id, c) in &stats {
            if !live_ids.contains(id) && !ledger.retired.contains_key(id) {
                ledger.retired.insert(id.clone(), *c);
                folded = true;
            }
        }
        if folded {
            ledger.save(data_dir)?;
        }
        for id in ids.iter().filter(|id| !live_ids.contains(*id)) {
            fs::remove_file(data_dir.join(log_name(id)))?;
        }
        stats.retain(|id, _| live_ids.contains(id));
        let mut core = Core {
            dir: data_dir.to_path_buf(),
            keysets,
            live: Vec::new(),
            spent,
            answers,
            rotation,
            stats,
            ledger,
        };
        core.refresh();
        let core = Arc::new(Mutex::new(core));
        let broken = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (dir, c, b) = (
            data_dir.to_path_buf(),
            Arc::clone(&core),
            Arc::clone(&broken),
        );
        std::thread::Builder::new()
            .name("fsp-spend-writer".into())
            .spawn(move || writer(rx, dir, max_batch.max(1), c, b))?;
        Ok(Spender {
            dir: data_dir.to_path_buf(),
            core,
            tx: Mutex::new(tx),
            broken,
            max_items: crate::config::DEFAULT_MAX_ITEMS,
            max_memo_bytes: crate::api::MAX_MEMO_BYTES,
        })
    }

    /// Set `spend.max_items`, the most notes in and the most outputs out of one
    /// spend. Call it right after `open`.
    pub fn with_max_items(mut self, max_items: usize) -> Self {
        self.max_items = max_items.max(1);
        self
    }

    /// Set `spend.max_memo_bytes`, the longest memo on a spend. Call it right
    /// after `open`.
    pub fn with_max_memo_bytes(mut self, max_memo_bytes: usize) -> Self {
        self.max_memo_bytes = max_memo_bytes;
        self
    }

    /// Why the spend path is stopped, if it is.
    pub fn fault(&self) -> Option<&'static str> {
        if self.broken.load(Ordering::SeqCst) {
            Some("a spend log write failed: restart the mint")
        } else if self.core.is_poisoned() {
            Some("the spend lock is poisoned")
        } else {
            None
        }
    }

    /// The live keysets: current last.
    pub fn keysets(&self, now: u64) -> Result<Vec<Arc<Keyset>>, String> {
        let mut core = self
            .core
            .lock()
            .map_err(|_| "the spend lock is poisoned".to_string())?;
        let deleted = core.rotate(now).map_err(|e| e.to_string())?;
        self.send_deletes(deleted);
        Ok(core.live.clone())
    }

    /// Record the mint's first issuance (once; later calls change nothing) and
    /// start counting toward rotation from it.
    pub fn note_first_issue(&self, now: u64) -> io::Result<u64> {
        let mut core = self
            .core
            .lock()
            .map_err(|_| io::Error::other("the spend lock is poisoned"))?;
        if let Some(t) = core.rotation.first_issue {
            return Ok(t);
        }
        let t = crate::keysets::record_first_issue(&self.dir, now)?;
        core.rotation.first_issue = Some(t);
        Ok(t)
    }

    pub fn rotation(&self) -> Rotation {
        self.core.lock().map(|c| c.rotation).unwrap_or(Rotation {
            first_issue: None,
            activation_secs: 0,
            period_secs: 1,
        })
    }

    /// The spend side of the audit, after a rotation that is due.
    pub fn audit_view(&self, now: u64) -> Result<SpendView, String> {
        let mut core = self
            .core
            .lock()
            .map_err(|_| "the spend lock is poisoned".to_string())?;
        let deleted = core.rotate(now).map_err(|e| e.to_string())?;
        self.send_deletes(deleted);
        Ok(SpendView {
            live: core.live.iter().map(|k| k.id.clone()).collect(),
            counters: core.stats.clone(),
            retired: core.ledger.retired.clone(),
        })
    }

    fn send_deletes(&self, deleted: Vec<String>) {
        if let Ok(tx) = self.tx.lock() {
            for d in deleted {
                let _ = tx.send(Msg::Delete(d));
            }
        }
    }

    /// Whether a note of a keyset is spent. Used by tests only: nothing in the
    /// mint or the wallet API calls it.
    #[doc(hidden)]
    pub fn is_spent(&self, keyset: &str, nonce: &[u8; 33]) -> bool {
        self.core
            .lock()
            .map(|c| c.spent.get(keyset).is_some_and(|s| s.contains(nonce)))
            .unwrap_or(true)
    }

    pub async fn spend(
        &self,
        request: &SpendRequest,
        now: u64,
    ) -> Result<SpendResponse, SpendError> {
        if let Some(why) = self.fault() {
            return Err(SpendError {
                status: 503,
                code: sp_wire::codes::UNAVAILABLE,
                why: why.into(),
                spent: Vec::new(),
            });
        }
        // Shape and amounts: no lock.
        if request.inputs.is_empty() {
            return Err(refuse("no_inputs", "a spend needs at least one note"));
        }
        let max = self.max_items;
        if request.inputs.len() > max || request.outputs.len() > max {
            return Err(refuse(
                "too_many_items",
                format!("at most {max} notes and {max} outputs"),
            ));
        }
        let max_memo = self.max_memo_bytes;
        if request.memo.len() > max_memo {
            return Err(refuse(
                "memo_too_long",
                format!("the memo is at most {max_memo} bytes"),
            ));
        }
        let mut nonces = HashSet::new();
        for (i, input) in request.inputs.iter().enumerate() {
            if !crate::is_denomination(input.input.amount.units) {
                return Err(refuse(
                    "bad_denomination",
                    format!("note {i}: FSP amounts are powers of two"),
                ));
            }
            if !nonces.insert(input.input.note.nonce.0.serialize()) {
                return Err(refuse(
                    "duplicate_input",
                    format!("note {i} is in the request twice"),
                ));
            }
        }
        let mut blinds = HashSet::new();
        for (o, out) in request.outputs.iter().enumerate() {
            if !crate::is_denomination(out.amount.units) {
                return Err(refuse(
                    "bad_denomination",
                    format!("output {o}: FSP amounts are powers of two"),
                ));
            }
            if !blinds.insert(out.blind_nonce) {
                return Err(refuse(
                    "duplicate_blind_nonce",
                    format!("output {o} repeats a blind nonce"),
                ));
            }
        }
        let inputs: u128 = request
            .inputs
            .iter()
            .map(|i| i.input.amount.units as u128)
            .sum();
        let outputs: u128 = request.outputs.iter().map(|o| o.amount.units as u128).sum();
        if inputs != outputs + request.pay as u128 {
            return Err(refuse(
                sp_wire::codes::NOT_EXACT,
                format!(
                    "the notes are worth {inputs}, the payment is {} and the change {outputs}: they must add up",
                    request.pay
                ),
            ));
        }

        let hash = request.hash();
        let (live, deleted) = {
            let mut core = self.lock()?;
            let deleted = core.rotate(now).map_err(|e| SpendError {
                status: 500,
                code: sp_wire::codes::INTERNAL,
                why: e.to_string(),
                spent: Vec::new(),
            })?;
            if let Some(a) = core.answers.get(&hash) {
                if a.durable {
                    return Ok(a.response.clone());
                }
                return Err(SpendError {
                    status: 409,
                    code: sp_wire::codes::IN_PROGRESS,
                    why: "this spend is being written: send it again".into(),
                    spent: Vec::new(),
                });
            }
            (core.live.clone(), deleted)
        };
        self.send_deletes(deleted);

        // Keysets and signatures: no lock.
        let digest = request.digest();
        for (i, input) in request.inputs.iter().enumerate() {
            let Some(keyset) = live.iter().find(|k| k.id == input.keyset) else {
                return Err(self.lock()?.not_live(i, &input.keyset));
            };
            let sk = keyset.key(input.input.amount).expect("a denomination");
            if !bdhke::verify_message(
                sk,
                input.input.note.signature,
                &input.input.note.nonce.as_hash_preimage(),
            ) {
                return Err(refuse(
                    "bad_note",
                    format!(
                        "note {i} is not a genuine FSP note of keyset {}",
                        input.keyset
                    ),
                ));
            }
            let xonly = input.input.note.nonce.0.x_only_public_key().0;
            if SECP256K1
                .verify_schnorr(&input.input.spend_signature, &digest, &xonly)
                .is_err()
            {
                return Err(refuse(
                    "bad_spend_signature",
                    format!("note {i}: the spend signature does not cover this request"),
                ));
            }
        }
        let current = Arc::clone(live.last().expect("a keyset"));
        let mut signatures = Vec::with_capacity(request.outputs.len());
        let mut keys = Vec::with_capacity(request.outputs.len());
        for o in &request.outputs {
            signatures.push(bdhke::sign_message(
                current.key(o.amount).expect("a denomination"),
                &o.blind_nonce.0,
            ));
            keys.push(current.public[&o.amount.units]);
        }
        let response = SpendResponse {
            keyset: current.id.clone(),
            signatures,
            keys,
            paid: request.pay,
        };

        // The spent lists, under the lock.
        let (file, line) = {
            let mut core = self.lock()?;
            if let Some(a) = core.answers.get(&hash) {
                if a.durable {
                    return Ok(a.response.clone());
                }
                return Err(SpendError {
                    status: 409,
                    code: sp_wire::codes::IN_PROGRESS,
                    why: "this spend is being written: send it again".into(),
                    spent: Vec::new(),
                });
            }
            let mut record_nonces = Vec::new();
            let mut spent_at = Vec::new();
            for (i, input) in request.inputs.iter().enumerate() {
                if core.find(&input.keyset).is_none() {
                    if spent_at.is_empty() {
                        return Err(core.not_live(i, &input.keyset));
                    }
                    // A spent note came first, so the answer is the list of spent
                    // notes; this one cannot be looked up, and is not on it.
                    continue;
                }
                let n = input.input.note.nonce.0.serialize();
                if core
                    .spent
                    .get(&input.keyset)
                    .is_some_and(|s| s.contains(&n))
                {
                    // Every spent input is named, not only the first: a wallet
                    // that holds several stale notes drops them all at once.
                    spent_at.push(i);
                }
                record_nonces.push((input.keyset.clone(), n));
            }
            if !spent_at.is_empty() {
                return Err(spent_elsewhere(spent_at));
            }
            for (ks, n) in &record_nonces {
                core.spent.entry(ks.clone()).or_default().insert(*n);
            }
            let file = core.current().id.clone();
            let record = Record {
                h: hash.clone(),
                n: record_nonces
                    .iter()
                    .map(|(k, n)| (k.clone(), hex::encode(n)))
                    .collect(),
                r: response.clone(),
                a: Amounts {
                    i: request
                        .inputs
                        .iter()
                        .map(|i| i.input.amount.units)
                        .collect(),
                    o: request.outputs.iter().map(|o| o.amount.units).collect(),
                },
            };
            apply(&mut core.stats, &record);
            let mut line = serde_json::to_vec(&record).map_err(|e| SpendError {
                status: 500,
                code: sp_wire::codes::INTERNAL,
                why: e.to_string(),
                spent: Vec::new(),
            })?;
            line.push(b'\n');
            core.answers.insert(
                hash.clone(),
                Answer {
                    response: response.clone(),
                    durable: false,
                    file: file.clone(),
                },
            );
            (file, line)
        };

        let (done, wait) = oneshot::channel();
        let sent = self
            .tx
            .lock()
            .map_err(|_| SpendError {
                status: 500,
                code: sp_wire::codes::INTERNAL,
                why: "the writer lock is poisoned".into(),
                spent: Vec::new(),
            })?
            .send(Msg::Write {
                file,
                hash,
                line,
                done,
            });
        if sent.is_err() {
            self.broken.store(true, Ordering::SeqCst);
            return Err(SpendError {
                status: 500,
                code: sp_wire::codes::INTERNAL,
                why: "the spend writer has stopped".into(),
                spent: Vec::new(),
            });
        }
        match wait.await {
            Ok(Ok(())) => Ok(response),
            Ok(Err(e)) => Err(SpendError {
                status: 500,
                code: sp_wire::codes::INTERNAL,
                why: e,
                spent: Vec::new(),
            }),
            Err(_) => Err(SpendError {
                status: 500,
                code: sp_wire::codes::INTERNAL,
                why: "the spend writer has stopped".into(),
                spent: Vec::new(),
            }),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Core>, SpendError> {
        self.core.lock().map_err(|_| SpendError {
            status: 500,
            code: sp_wire::codes::INTERNAL,
            why: "the spend lock is poisoned".into(),
            spent: Vec::new(),
        })
    }
}

fn writer(
    rx: mpsc::Receiver<Msg>,
    dir: PathBuf,
    max_batch: usize,
    core: Arc<Mutex<Core>>,
    broken: Arc<AtomicBool>,
) {
    let mut files: HashMap<String, File> = HashMap::new();
    loop {
        let Ok(first) = rx.recv() else { return };
        let mut batch = vec![first];
        while batch.len() < max_batch {
            match rx.try_recv() {
                Ok(m) => batch.push(m),
                Err(_) => break,
            }
        }
        let mut waiting = Vec::new();
        let mut touched = HashSet::new();
        let mut result: Result<(), String> = Ok(());
        for msg in batch {
            match msg {
                Msg::Delete(id) => {
                    files.remove(&id);
                    let _ = fs::remove_file(dir.join(log_name(&id)));
                }
                Msg::Write {
                    file,
                    hash,
                    line,
                    done,
                } => {
                    if result.is_ok() {
                        let f = match files.get_mut(&file) {
                            Some(f) => Ok(f),
                            None => OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(dir.join(log_name(&file)))
                                .map(|f| files.entry(file.clone()).or_insert(f)),
                        };
                        result = f
                            .and_then(|f| f.write_all(&line))
                            .map_err(|e| format!("spend log write failed: {e}"));
                        touched.insert(file);
                    }
                    waiting.push((hash, done));
                }
            }
        }
        if result.is_ok() {
            for id in &touched {
                if let Some(f) = files.get(id) {
                    if let Err(e) = f.sync_data() {
                        result = Err(format!("spend log sync failed: {e}"));
                    }
                }
            }
        }
        if result.is_err() {
            broken.store(true, Ordering::SeqCst);
        } else if let Ok(mut c) = core.lock() {
            for (hash, _) in &waiting {
                if let Some(a) = c.answers.get_mut(hash) {
                    a.durable = true;
                }
            }
        }
        for (_, done) in waiting {
            let _ = done.send(result.clone());
        }
    }
}
