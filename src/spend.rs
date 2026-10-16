//! `POST /spend`, the fast path: a swap with change.
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

use crate::api::{MAX_MEMO_BYTES, SpendRequest, SpendResponse};
use crate::keysets::{Keyset, Keysets, Rotation};

pub fn log_name(keyset: &str) -> String {
    format!("spends-{keyset}.jsonl")
}

#[derive(Serialize, Deserialize)]
struct Record {
    h: String,
    n: Vec<(String, String)>,
    r: SpendResponse,
}

struct Answer {
    response: SpendResponse,
    durable: bool,
    file: String,
}

pub struct Core {
    keysets: Keysets,
    live: Vec<Arc<Keyset>>,
    spent: HashMap<String, HashSet<[u8; 33]>>,
    answers: HashMap<String, Answer>,
    rotation: Rotation,
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
}

fn refuse(code: &'static str, why: impl Into<String>) -> SpendError {
    SpendError {
        status: 400,
        code,
        why: why.into(),
    }
}

pub struct Spender {
    dir: PathBuf,
    core: Arc<Mutex<Core>>,
    tx: Mutex<mpsc::Sender<Msg>>,
    broken: Arc<AtomicBool>,
    /// `spend.max_items`; the default is only a default (see `config`).
    max_items: usize,
}

/// Read a log, cutting off a last line that a crash left half written.
fn read_log(path: &Path) -> io::Result<Vec<Record>> {
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
            OpenOptions::new()
                .write(true)
                .open(path)?
                .set_len(good_len)?;
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
        let deleted = keysets.rotate_to(rotation.epoch_at(now))?;
        let live_ids: HashSet<String> = keysets.all().iter().map(|k| k.id.clone()).collect();
        for d in deleted {
            let _ = fs::remove_file(data_dir.join(log_name(&d)));
        }
        // Logs of keysets that are gone (a crash during a rotation) go too.
        for entry in fs::read_dir(data_dir)? {
            let name = entry?.file_name().to_string_lossy().to_string();
            if let Some(id) = name
                .strip_prefix("spends-")
                .and_then(|n| n.strip_suffix(".jsonl"))
            {
                if !live_ids.contains(id) {
                    fs::remove_file(data_dir.join(&name))?;
                }
            }
        }
        let mut spent: HashMap<String, HashSet<[u8; 33]>> = HashMap::new();
        let mut answers = HashMap::new();
        for k in keysets.all() {
            for record in read_log(&data_dir.join(log_name(&k.id)))? {
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
                        file: k.id.clone(),
                    },
                );
            }
        }
        let mut core = Core {
            keysets,
            live: Vec::new(),
            spent,
            answers,
            rotation,
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
        })
    }

    /// Set `spend.max_items`, the most notes in and the most outputs out of one
    /// spend. Call it right after `open`.
    pub fn with_max_items(mut self, max_items: usize) -> Self {
        self.max_items = max_items.max(1);
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

    fn send_deletes(&self, deleted: Vec<String>) {
        if let Ok(tx) = self.tx.lock() {
            for d in deleted {
                let _ = tx.send(Msg::Delete(d));
            }
        }
    }

    /// Whether a note of a keyset is spent (for tests and the wallet's checks).
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
        if request.memo.len() > MAX_MEMO_BYTES {
            return Err(refuse(
                "memo_too_long",
                format!("the memo is at most {MAX_MEMO_BYTES} bytes"),
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
            })?;
            if let Some(a) = core.answers.get(&hash) {
                if a.durable {
                    return Ok(a.response.clone());
                }
                return Err(SpendError {
                    status: 409,
                    code: sp_wire::codes::IN_PROGRESS,
                    why: "this spend is being written: send it again".into(),
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
                });
            }
            let mut record_nonces = Vec::new();
            for (i, input) in request.inputs.iter().enumerate() {
                if core.find(&input.keyset).is_none() {
                    return Err(core.not_live(i, &input.keyset));
                }
                let n = input.input.note.nonce.0.serialize();
                if core
                    .spent
                    .get(&input.keyset)
                    .is_some_and(|s| s.contains(&n))
                {
                    return Err(refuse(
                        "already_spent",
                        format!("note {i} is already spent"),
                    ));
                }
                record_nonces.push((input.keyset.clone(), n));
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
            };
            let mut line = serde_json::to_vec(&record).map_err(|e| SpendError {
                status: 500,
                code: sp_wire::codes::INTERNAL,
                why: e.to_string(),
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
            });
        }
        match wait.await {
            Ok(Ok(())) => Ok(response),
            Ok(Err(e)) => Err(SpendError {
                status: 500,
                code: sp_wire::codes::INTERNAL,
                why: e,
            }),
            Err(_) => Err(SpendError {
                status: 500,
                code: sp_wire::codes::INTERNAL,
                why: "the spend writer has stopped".into(),
            }),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Core>, SpendError> {
        self.core.lock().map_err(|_| SpendError {
            status: 500,
            code: sp_wire::codes::INTERNAL,
            why: "the spend lock is poisoned".into(),
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
