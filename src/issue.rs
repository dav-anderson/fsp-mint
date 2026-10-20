//! The issue call (over AIR, kind 11): ESP named for this service in, FSP out, 1:1 less the issue
//! fee.
//!
//! One request spans two mints with different operators: the ESP transfer at
//! the Issuer (esp-mint), then the FSP issuance here. So it runs as a stored
//! pending entry that a retry finishes:
//!
//! 1. Checks that touch nothing: shape, denominations, the service, each spend
//!    secret against its note, each group exact (ESP = FSP outputs + fee).
//! 2. A stored answer for this request hash is returned as it is. A pending
//!    entry is picked up where it stopped.
//! 3. No ESP note or FSP output of the request is held by another pending
//!    issue, and no ESP note was kept before.
//! 4. One ESP transfer per group (one redeemable-after day each) is built:
//!    the app's notes in, fresh ESP notes of the same sizes out, signed by each
//!    note's secret and by the service's DID key. The pending entry, with the
//!    transfers and the new notes' secrets, is written before anything is sent.
//! 5. Each transfer is sent. Done: the new ESP notes are unblinded and kept (to
//!    redeem later with the Issuer). Refused (`400`, nothing burned): that group
//!    gets no FSP. Unknown (unreachable, timeout, `5xx`): the request answers
//!    `502` and stays pending; a retry sends the same transfer, which the Issuer
//!    answers from its store.
//! 6. When every group is settled, the FSP outputs of the transferred groups
//!    are signed (deterministic, so a crash here just signs again), the answer
//!    is stored as done, and returned.
//!
//! Nothing is issued twice: the answer is stored under the request hash, and
//! the ESP notes can be transferred only once at the Issuer.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use mint_types::{Amount, BlindNonce, MintInput, MintOutput, Nonce, Note};
use rand::rngs::OsRng;
use secp256k1::schnorr::Signature;
use secp256k1::{Keypair, SECP256K1, SecretKey};
use serde::{Deserialize, Serialize};
use sp_wire::ServiceDid;
use sp_wire::esp::transfer::{self, TransferRequest};

use crate::api::{GroupResult, IssueRequest, IssueResponse, issue_group_size};
use crate::audit::Num;
use crate::esp_client::{AirRequest, Issuer, TransferOutcome};
use crate::keysets::Keyset;
use crate::store;

pub const FILE_NAME: &str = "issues.json";

/// A fresh ESP output the service will keep.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EspOut {
    pub amount: u64,
    pub nonce_secret: SecretKey,
    pub blinding_key: SecretKey,
    pub blind_nonce: BlindNonce,
}

impl EspOut {
    fn new(amount: u64) -> Self {
        let keypair = Keypair::new(SECP256K1, &mut OsRng);
        let nonce = Nonce(keypair.public_key());
        let (point, blinding_key) = bdhke::blind_message(&nonce.as_hash_preimage(), None);
        EspOut {
            amount,
            nonce_secret: keypair.secret_key(),
            blinding_key,
            blind_nonce: BlindNonce(point),
        }
    }
}

/// An ESP note the service holds, to redeem with the Issuer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeptEsp {
    pub amount: u64,
    pub note: Note,
    pub spend_secret: SecretKey,
    pub redeemable_after_day: u64,
    /// The ESP keyset that signed it, from the Issuer's transfer reply. Empty
    /// for a note kept before keysets.
    #[serde(default)]
    pub keyset: String,
    /// The issue request it came from.
    pub request: String,
    /// Written out by `fsp-mint export-esp`.
    #[serde(default)]
    pub exported: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GroupState {
    Waiting,
    Transferred,
    Refused { why: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingGroup {
    pub transfer: TransferRequest,
    pub esp_outputs: Vec<EspOut>,
    pub state: GroupState,
    /// The request to the Issuer over AIR that carries `transfer` and has not
    /// been answered, kept here beside the transfer so that a retry reads the
    /// reply cell of the same request first and writes no second one. Absent
    /// when no request is in flight, and in files written before 0.6.0 (they
    /// read as none: the retry then makes a new request, which the Issuer
    /// answers from its stored answer if it did the transfer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub air: Option<AirRequest>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pending {
    pub request: IssueRequest,
    /// The keyset the FSP outputs are signed with, chosen at acceptance.
    pub keyset: String,
    pub fee_per_byte: u64,
    pub groups: Vec<PendingGroup>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum Entry {
    Pending(Box<Pending>),
    Done(IssueResponse),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IssueData {
    #[serde(default)]
    pub entries: BTreeMap<String, Entry>,
    #[serde(default)]
    pub kept: Vec<KeptEsp>,
    /// The app's ESP notes (nonces, hex) already transferred to the service.
    #[serde(default)]
    pub transferred: std::collections::BTreeSet<String>,
    /// FSP units issued, per key set that signed them. Counted in the same
    /// save as the stored answer of the issue (see `audit`).
    #[serde(default)]
    pub issued: BTreeMap<String, Num>,
    /// How many answered issues there were when the counters began (those
    /// issues are not in `issued`). `None` until the first open by 0.4.0.
    #[serde(default)]
    pub uncounted_requests: Option<u64>,
}

pub struct Issues {
    path: PathBuf,
    pub data: IssueData,
    /// The Issuer's current keyset as last read, with the time read (Unix
    /// seconds). Not saved: read again after a restart.
    pub esp_keyset: Option<(String, u64)>,
}

impl Issues {
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        let path = data_dir.join(FILE_NAME);
        let mut data: IssueData = store::load(&path)?.unwrap_or_default();
        data.start_counting();
        Ok(Issues {
            path,
            data,
            esp_keyset: None,
        })
    }

    pub fn save(&self) -> io::Result<()> {
        store::save(&self.path, &self.data)
    }
}

/// Why an issue request was not answered with FSP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueError {
    /// 400 refused (nothing happened), 409 held by another request, 502 the
    /// Issuer's answer is unknown (retry), 500 internal.
    pub status: u16,
    pub code: &'static str,
    pub why: String,
}

fn refuse(code: &'static str, why: impl Into<String>) -> IssueError {
    IssueError {
        status: 400,
        code,
        why: why.into(),
    }
}

fn internal(e: impl std::fmt::Display) -> IssueError {
    IssueError {
        status: 500,
        code: sp_wire::codes::INTERNAL,
        why: e.to_string(),
    }
}

/// What an issue needs from the rest of the mint.
pub struct IssueContext<'a> {
    pub service: ServiceDid,
    pub service_keypair: &'a Keypair,
    pub fee_per_byte: u64,
    pub current: &'a Keyset,
    pub previous: Option<&'a Keyset>,
    /// The clock (Unix seconds) and `esp.keyset_ttl_secs`, for the cached
    /// Issuer keyset.
    pub now: u64,
    pub esp_keyset_ttl_secs: u64,
    /// `issue.max_items`: the most groups in a request, and the most notes and
    /// outputs in a group.
    pub max_items: usize,
}

/// The Issuer's current keyset for new transfers: the cached one while it is
/// younger than the TTL, else read again. A failed read falls back to the
/// cached one (it is only replaced, never expired, by the TTL; dropped only
/// when the Issuer says it is retired); with none cached the
/// issue is refused `502 issuer_unavailable`.
fn esp_keyset(
    issues: &mut Issues,
    issuer: &dyn Issuer,
    ctx: &IssueContext,
) -> Result<String, IssueError> {
    if let Some((k, at)) = &issues.esp_keyset {
        if ctx.now < at.saturating_add(ctx.esp_keyset_ttl_secs) {
            return Ok(k.clone());
        }
    }
    match issuer.current_keyset() {
        Ok(k) => {
            issues.esp_keyset = Some((k.clone(), ctx.now));
            Ok(k)
        }
        Err(why) => match &issues.esp_keyset {
            Some((k, _)) => Ok(k.clone()),
            None => Err(IssueError {
                status: 502,
                code: sp_wire::codes::ISSUER_UNAVAILABLE,
                why: format!("could not read the Issuer's keyset: {why}; retry"),
            }),
        },
    }
}

/// Checks 1: everything that needs nothing but the request.
pub fn check(request: &IssueRequest, ctx: &IssueContext) -> Result<(), IssueError> {
    if request.service != ctx.service {
        return Err(refuse(
            "wrong_service",
            "these notes are for another service: this mint accepts only ESP named for its own DID",
        ));
    }
    if request.groups.is_empty() {
        return Err(refuse("no_groups", "the request has no groups"));
    }
    let max = ctx.max_items;
    if request.groups.len() > max {
        return Err(refuse("too_many_items", format!("at most {max} groups")));
    }
    let mut days = HashSet::new();
    let mut nonces = HashSet::new();
    let mut blinds = HashSet::new();
    for (g, group) in request.groups.iter().enumerate() {
        if !days.insert(group.redeemable_after_day) {
            return Err(refuse(
                "duplicate_day",
                format!("group {g}: one group per redeemable-after day"),
            ));
        }
        if group.notes.is_empty() || group.outputs.is_empty() {
            return Err(refuse(
                "empty_group",
                format!("group {g} needs notes and outputs"),
            ));
        }
        if group.notes.len() > max || group.outputs.len() > max {
            return Err(refuse(
                "too_many_items",
                format!("group {g}: at most {max} notes and {max} outputs"),
            ));
        }
        for (i, n) in group.notes.iter().enumerate() {
            if n.amount == 0 {
                return Err(refuse(
                    "zero_amount",
                    format!("group {g} note {i} is worth 0"),
                ));
            }
            if !nonces.insert(n.note.nonce) {
                return Err(refuse(
                    "duplicate_input",
                    format!("group {g} note {i} is in the request twice"),
                ));
            }
            if n.keyset.len() != 16 || !n.keyset.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(refuse(
                    sp_wire::codes::MISSING_KEYSET,
                    format!(
                        "group {g} note {i}: name the ESP keyset that signed the note (keyset, 16 hex)"
                    ),
                ));
            }
            if n.spend_secret.public_key(SECP256K1) != n.note.nonce.0 {
                return Err(refuse(
                    "bad_secret",
                    format!("group {g} note {i}: the spend secret is not this note's"),
                ));
            }
        }
        for (o, out) in group.outputs.iter().enumerate() {
            if !crate::is_denomination(out.amount.units) {
                return Err(refuse(
                    "bad_denomination",
                    format!("group {g} output {o}: FSP amounts are powers of two"),
                ));
            }
            if !blinds.insert(out.blind_nonce) {
                return Err(refuse(
                    "duplicate_blind_nonce",
                    format!("group {g} output {o} repeats a blind nonce"),
                ));
            }
        }
        let esp: u128 = group.notes.iter().map(|n| n.amount as u128).sum();
        let fsp: u128 = group.outputs.iter().map(|o| o.amount.units as u128).sum();
        let fee = issue_group_size(group.notes.len(), group.outputs.len()) as u128
            * ctx.fee_per_byte as u128;
        if fsp + fee != esp {
            return Err(refuse(
                sp_wire::codes::NOT_EXACT,
                format!(
                    "group {g}: the ESP is {esp} units, the FSP outputs {fsp} and the fee {fee}; the ESP must equal the outputs plus the fee"
                ),
            ));
        }
    }
    Ok(())
}

fn build_transfer(
    group: &crate::api::IssueGroup,
    service_keypair: &Keypair,
    esp_keyset: &str,
) -> PendingGroup {
    let esp_outputs: Vec<EspOut> = group.notes.iter().map(|n| EspOut::new(n.amount)).collect();
    let outputs: Vec<MintOutput> = esp_outputs
        .iter()
        .map(|o| MintOutput {
            amount: Amount::from_units(o.amount),
            blind_nonce: o.blind_nonce,
        })
        .collect();
    let dummy = Signature::from_slice(&[1u8; 64]).expect("64 bytes");
    let mut inputs: Vec<MintInput> = group
        .notes
        .iter()
        .map(|n| MintInput {
            amount: Amount::from_units(n.amount),
            note: n.note,
            spend_signature: dummy,
        })
        .collect();
    let service = ServiceDid::from(service_keypair.public_key());
    let digest = transfer::digest(&service, group.redeemable_after_day, &inputs, &outputs);
    for (input, n) in inputs.iter_mut().zip(&group.notes) {
        let kp = Keypair::from_secret_key(SECP256K1, &n.spend_secret);
        input.spend_signature = SECP256K1.sign_schnorr(&digest, &kp);
    }
    let service_signature = SECP256K1.sign_schnorr(&digest, service_keypair);
    PendingGroup {
        transfer: TransferRequest {
            service,
            redeemable_after_day: group.redeemable_after_day,
            input_keysets: group.notes.iter().map(|n| n.keyset.clone()).collect(),
            inputs,
            keyset: esp_keyset.to_string(),
            outputs,
            service_signature,
        },
        esp_outputs,
        state: GroupState::Waiting,
        air: None,
    }
}

/// Send the transfer of group `g` to the Issuer, keeping the request in flight
/// in the pending group (and `issues.json`) before it is written and after each
/// write, and clearing it when the request ended.
fn send_transfer(
    issues: &mut Issues,
    issuer: &dyn Issuer,
    pending: &mut Pending,
    hash: &str,
    g: usize,
) -> TransferOutcome {
    let transfer = pending.groups[g].transfer.clone();
    let kept = pending.groups[g].air.clone();
    let (outcome, kept) = issuer.transfer_kept(&transfer, kept, &mut |k: &AirRequest| {
        pending.groups[g].air = Some(k.clone());
        issues
            .data
            .entries
            .insert(hash.to_string(), Entry::Pending(Box::new(pending.clone())));
        issues.save().map_err(|e| e.to_string())
    });
    pending.groups[g].air = kept;
    outcome
}

/// Run one issue request to the end, or as far as the Issuer allows.
pub fn issue(
    issues: &mut Issues,
    issuer: &dyn Issuer,
    request: &IssueRequest,
    ctx: &IssueContext,
) -> Result<IssueResponse, IssueError> {
    check(request, ctx)?;
    let hash = request.hash();
    let mut pending = match issues.data.entries.get(&hash) {
        Some(Entry::Done(done)) => return Ok(done.clone()),
        Some(Entry::Pending(p)) => (**p).clone(),
        None => {
            // 3. Held by another pending issue, or kept already.
            let ours: HashSet<String> = request
                .groups
                .iter()
                .flat_map(|g| {
                    g.notes
                        .iter()
                        .map(|n| hex::encode(n.note.nonce.0.serialize()))
                })
                .collect();
            let our_blinds: HashSet<BlindNonce> = request
                .groups
                .iter()
                .flat_map(|g| g.outputs.iter().map(|o| o.blind_nonce))
                .collect();
            for entry in issues.data.entries.values() {
                if let Entry::Pending(p) = entry {
                    for g in &p.request.groups {
                        if g.notes
                            .iter()
                            .any(|n| ours.contains(&hex::encode(n.note.nonce.0.serialize())))
                            || g.outputs
                                .iter()
                                .any(|o| our_blinds.contains(&o.blind_nonce))
                        {
                            return Err(IssueError {
                                status: 409,
                                code: sp_wire::codes::NOTE_HELD,
                                why: "a note or output of this request is held by another issue that has not finished: retry that one".into(),
                            });
                        }
                    }
                }
            }
            if ours.iter().any(|n| issues.data.transferred.contains(n)) {
                return Err(refuse(
                    "already_spent",
                    "an ESP note of this request was already transferred to this service",
                ));
            }
            // The ESP outputs are asked of the Issuer's current keyset, fixed
            // here so that a retry sends the same transfer.
            let esp_keyset = esp_keyset(issues, issuer, ctx)?;
            let p = Pending {
                request: request.clone(),
                keyset: ctx.current.id.clone(),
                fee_per_byte: ctx.fee_per_byte,
                groups: request
                    .groups
                    .iter()
                    .map(|g| build_transfer(g, ctx.service_keypair, &esp_keyset))
                    .collect(),
            };
            issues
                .data
                .entries
                .insert(hash.clone(), Entry::Pending(Box::new(p.clone())));
            issues.save().map_err(internal)?;
            p
        }
    };

    // 5. Each transfer.
    for g in 0..pending.groups.len() {
        if !matches!(pending.groups[g].state, GroupState::Waiting) {
            continue;
        }
        let mut outcome = send_transfer(issues, issuer, &mut pending, &hash, g);
        // The cached keyset was deleted at the Issuer (it signs with any live
        // set, so a set that has only been rotated to previous still works):
        // drop it, read the current one, re-blind the group's ESP outputs for
        // it and try once more (nothing was burned by the refusal).
        if let TransferOutcome::Refused { code, .. } = &outcome {
            if code == sp_wire::codes::KEYSET_RETIRED {
                issues.esp_keyset = None;
                if let Ok(fresh) = esp_keyset(issues, issuer, ctx) {
                    if fresh != pending.groups[g].transfer.keyset {
                        pending.groups[g] =
                            build_transfer(&pending.request.groups[g], ctx.service_keypair, &fresh);
                        issues
                            .data
                            .entries
                            .insert(hash.clone(), Entry::Pending(Box::new(pending.clone())));
                        issues.save().map_err(internal)?;
                        outcome = send_transfer(issues, issuer, &mut pending, &hash, g);
                    }
                }
            }
        }
        match outcome {
            TransferOutcome::Done(reply) => {
                let group = &pending.groups[g];
                if reply.signatures.len() != group.esp_outputs.len()
                    || reply.keys.len() != group.esp_outputs.len()
                {
                    return Err(IssueError {
                        status: 502,
                        code: sp_wire::codes::ISSUER_UNAVAILABLE,
                        why: "the Issuer's transfer reply does not fit the request; retry".into(),
                    });
                }
                for (i, out) in group.esp_outputs.iter().enumerate() {
                    let nonce = Nonce(out.nonce_secret.public_key(SECP256K1));
                    issues.data.kept.push(KeptEsp {
                        amount: out.amount,
                        note: Note {
                            nonce,
                            signature: bdhke::unblind_message(
                                &reply.signatures[i],
                                &out.blinding_key,
                                &reply.keys[i],
                            ),
                        },
                        spend_secret: out.nonce_secret,
                        redeemable_after_day: group.transfer.redeemable_after_day,
                        keyset: reply.keyset.clone(),
                        request: hash.clone(),
                        exported: false,
                    });
                }
                for n in &group.transfer.inputs {
                    issues
                        .data
                        .transferred
                        .insert(hex::encode(n.note.nonce.0.serialize()));
                }
                pending.groups[g].state = GroupState::Transferred;
            }
            TransferOutcome::Refused { why, .. } => {
                pending.groups[g].state = GroupState::Refused { why }
            }
            TransferOutcome::Unknown(why) => {
                // Keep the request still in flight, for the retry.
                issues
                    .data
                    .entries
                    .insert(hash.clone(), Entry::Pending(Box::new(pending.clone())));
                issues.save().map_err(internal)?;
                return Err(IssueError {
                    status: 502,
                    code: sp_wire::codes::ISSUER_UNAVAILABLE,
                    why: format!(
                        "the ESP transfer at the Issuer did not answer ({why}). Nothing was issued yet; send the same request again to finish it"
                    ),
                });
            }
        }
        issues
            .data
            .entries
            .insert(hash.clone(), Entry::Pending(Box::new(pending.clone())));
        issues.save().map_err(internal)?;
    }

    // 6. Sign. The keyset chosen at acceptance, unless it has been deleted
    //    since (then the current one, so the payer still gets live notes).
    let keyset = [Some(ctx.current), ctx.previous]
        .into_iter()
        .flatten()
        .find(|k| k.id == pending.keyset)
        .unwrap_or(ctx.current);
    let mut groups = Vec::new();
    let mut issued_units: i128 = 0;
    for (pg, rg) in pending.groups.iter().zip(&request.groups) {
        groups.push(match &pg.state {
            GroupState::Transferred => {
                let mut signatures = Vec::new();
                let mut keys = Vec::new();
                for o in &rg.outputs {
                    let sk = keyset
                        .key(o.amount)
                        .ok_or_else(|| internal("not a denomination"))?;
                    signatures.push(bdhke::sign_message(sk, &o.blind_nonce.0));
                    keys.push(keyset.public[&o.amount.units]);
                    issued_units += o.amount.units as i128;
                }
                GroupResult::Issued {
                    keyset: keyset.id.clone(),
                    signatures,
                    keys,
                    fee: issue_group_size(rg.notes.len(), rg.outputs.len())
                        .saturating_mul(pending.fee_per_byte),
                }
            }
            GroupState::Refused { why } => GroupResult::Refused {
                code: sp_wire::codes::ISSUER_REFUSED.into(),
                why: why.clone(),
            },
            GroupState::Waiting => return Err(internal("a group is still waiting")),
        });
    }
    let response = IssueResponse {
        groups,
        fee_per_byte: pending.fee_per_byte,
    };
    // The counter and the stored answer are saved together, once.
    if issued_units > 0 {
        issues.data.issued.entry(keyset.id.clone()).or_default().0 += issued_units;
    }
    issues
        .data
        .entries
        .insert(hash, Entry::Done(response.clone()));
    issues.save().map_err(internal)?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{HandedEsp, IssueGroup};
    use crate::keysets::Keysets;
    use std::sync::Mutex;

    fn kp(n: u8) -> Keypair {
        let mut b = [0u8; 32];
        b[31] = n;
        Keypair::from_secret_key(SECP256K1, &SecretKey::from_slice(&b).unwrap())
    }

    /// An Issuer stand-in: answers from a script, then "done" with junk keys.
    struct Script(Mutex<Vec<TransferOutcome>>, Mutex<usize>);
    impl Issuer for Script {
        fn current_keyset(&self) -> Result<String, String> {
            Ok("11".repeat(8))
        }
        fn transfer(&self, request: &TransferRequest) -> TransferOutcome {
            *self.1.lock().unwrap() += 1;
            let mut s = self.0.lock().unwrap();
            if !s.is_empty() {
                return s.remove(0);
            }
            let k = kp(9).public_key();
            TransferOutcome::Done(sp_wire::esp::exchange::ExchangeResponse {
                service: request.service,
                redeemable_after_day: request.redeemable_after_day,
                keyset: request.keyset.clone(),
                signatures: vec![k; request.outputs.len()],
                keys: vec![k; request.outputs.len()],
                fee_per_byte: 0,
                fee_esp: 0,
                size_bytes: 0,
            })
        }
    }

    fn note(units: u64) -> HandedEsp {
        let k = Keypair::new(SECP256K1, &mut OsRng);
        HandedEsp {
            amount: units,
            note: Note {
                nonce: Nonce(k.public_key()),
                signature: kp(7).public_key(),
            },
            spend_secret: k.secret_key(),
            keyset: "22".repeat(8),
        }
    }

    fn out(units: u64) -> MintOutput {
        let k = Keypair::new(SECP256K1, &mut OsRng);
        MintOutput {
            amount: Amount::from_units(units),
            blind_nonce: BlindNonce(k.public_key()),
        }
    }

    fn setup(name: &str) -> (Issues, Keysets, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("fsp-issue-{name}-{}", {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            format!(
                "{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        }));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        (Issues::open(&d).unwrap(), Keysets::open(&d).unwrap(), d)
    }

    fn request(days: &[u64]) -> IssueRequest {
        IssueRequest {
            service: ServiceDid::from(kp(1).public_key()),
            groups: days
                .iter()
                .map(|&d| IssueGroup {
                    redeemable_after_day: d,
                    notes: vec![note(4)],
                    outputs: vec![out(4)],
                })
                .collect(),
        }
    }

    #[test]
    fn the_issue_limits_are_the_setting_not_a_constant() {
        let (_issues, keys, d) = setup("limits");
        let service_kp = kp(1);
        let ctx = IssueContext {
            service: ServiceDid::from(service_kp.public_key()),
            service_keypair: &service_kp,
            fee_per_byte: 0,
            current: keys.current(),
            previous: None,
            now: 0,
            esp_keyset_ttl_secs: 3600,
            max_items: 2,
        };
        let e = check(&request(&[10, 11, 12]), &ctx).unwrap_err();
        assert_eq!(e.code, "too_many_items");
        assert!(e.why.contains("at most 2 groups"), "{}", e.why);
        let mut big = request(&[10]);
        big.groups[0].notes = vec![note(4), note(4), note(4)];
        let e = check(&big, &ctx).unwrap_err();
        assert_eq!(e.code, "too_many_items");
        assert!(e.why.contains("at most 2 notes"), "{}", e.why);
        assert!(check(&request(&[10, 11]), &ctx).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failure_between_the_mints_is_finished_by_a_retry_and_nothing_is_doubled() {
        let (mut issues, keys, d) = setup("flaky");
        let service_kp = kp(1);
        let ctx = IssueContext {
            service: ServiceDid::from(service_kp.public_key()),
            service_keypair: &service_kp,
            fee_per_byte: 0,
            current: keys.current(),
            previous: None,
            now: 0,
            esp_keyset_ttl_secs: 3600,
            max_items: crate::config::DEFAULT_MAX_ITEMS,
        };
        let req = request(&[10, 11]);
        // The first group goes through, then the Issuer stops answering.
        let issuer = Script(
            Mutex::new(vec![
                TransferOutcome::Done(
                    match (Script(Mutex::new(vec![]), Mutex::new(0))).transfer(
                        &build_transfer(&req.groups[0], &service_kp, &"11".repeat(8)).transfer,
                    ) {
                        TransferOutcome::Done(r) => r,
                        _ => unreachable!(),
                    },
                ),
                TransferOutcome::Unknown("timeout".into()),
            ]),
            Mutex::new(0),
        );
        let e = issue(&mut issues, &issuer, &req, &ctx).unwrap_err();
        assert_eq!((e.status, e.code), (502, "issuer_unavailable"));
        assert_eq!(issues.data.kept.len(), 1);
        // A crash here: reopen from disk, then the retry.
        let mut issues = Issues::open(&d).unwrap();
        assert!(matches!(
            issues.data.entries.values().next(),
            Some(Entry::Pending(_))
        ));
        // Another request with the same notes is held off meanwhile.
        let mut other = req.clone();
        other.groups[0].outputs = vec![out(4)];
        assert_eq!(
            issue(&mut issues, &issuer, &other, &ctx).unwrap_err().code,
            "note_held"
        );
        let done = issue(&mut issues, &issuer, &req, &ctx).unwrap();
        assert_eq!(done.groups.len(), 2);
        assert!(
            done.groups
                .iter()
                .all(|g| matches!(g, GroupResult::Issued { .. }))
        );
        assert_eq!(
            issues.data.kept.len(),
            2,
            "the first group's ESP is kept once"
        );
        let calls = *issuer.1.lock().unwrap();
        assert_eq!(issue(&mut issues, &issuer, &req, &ctx).unwrap(), done);
        assert_eq!(
            *issuer.1.lock().unwrap(),
            calls,
            "a done request does not call the Issuer"
        );
    }

    #[test]
    fn checks_before_anything_and_fees() {
        let (mut issues, keys, _) = setup("checks");
        let service_kp = kp(1);
        let mut ctx = IssueContext {
            service: ServiceDid::from(service_kp.public_key()),
            service_keypair: &service_kp,
            fee_per_byte: 0,
            current: keys.current(),
            previous: None,
            now: 0,
            esp_keyset_ttl_secs: 3600,
            max_items: crate::config::DEFAULT_MAX_ITEMS,
        };
        let issuer = Script(Mutex::new(vec![]), Mutex::new(0));
        let mut r = request(&[1]);
        r.groups[0].outputs = vec![out(3), out(1)];
        assert_eq!(
            issue(&mut issues, &issuer, &r, &ctx).unwrap_err().code,
            "bad_denomination"
        );
        let mut r = request(&[1, 1]);
        r.groups[1].notes = vec![note(4)];
        assert_eq!(
            issue(&mut issues, &issuer, &r, &ctx).unwrap_err().code,
            "duplicate_day"
        );
        let mut r = request(&[1]);
        r.groups[0].notes[0].spend_secret = kp(5).secret_key();
        assert_eq!(
            issue(&mut issues, &issuer, &r, &ctx).unwrap_err().code,
            "bad_secret"
        );
        // A fee of 1 a byte: one note and one output is 163 bytes.
        ctx.fee_per_byte = 1;
        let mut r = request(&[1]);
        r.groups[0].notes = vec![note(256)];
        r.groups[0].outputs = vec![out(64), out(16), out(8), out(4), out(1)];
        assert_eq!(
            issue(&mut issues, &issuer, &r, &ctx).unwrap_err().code,
            "not_exact"
        );
        r.groups[0].outputs = vec![out(64), out(8), out(4), out(1)]; // 77 + 16+106+41*4 = 77+286 > 256
        assert_eq!(
            issue(&mut issues, &issuer, &r, &ctx).unwrap_err().code,
            "not_exact"
        );
        let size = issue_group_size(1, 2); // 204
        r.groups[0].outputs = vec![out(32), out(16), out(4)];
        assert_eq!(issue_group_size(1, 3), 16 + 106 + 123);
        let _ = size;
        r.groups[0].notes = vec![note(256 + 52)];
        assert_eq!(
            issue(&mut issues, &issuer, &r, &ctx).unwrap_err().code,
            "not_exact"
        );
        r.groups[0].notes = vec![note(52 + 245)];
        let ok = issue(&mut issues, &issuer, &r, &ctx).unwrap();
        assert!(matches!(
            &ok.groups[0],
            GroupResult::Issued { fee: 245, .. }
        ));
        assert_eq!(
            *issuer.1.lock().unwrap(),
            1,
            "only the good request reached the Issuer"
        );
    }

    /// An Issuer that rotates: `current` is its keyset (or a read failure),
    /// `reads` counts keyset reads, and a transfer naming another keyset is
    /// refused `keyset_retired` as esp-mint does.
    struct Rotating {
        current: Mutex<Result<String, String>>,
        reads: Mutex<usize>,
        refused: Mutex<usize>,
    }
    impl Rotating {
        fn new(k: &str) -> Self {
            Rotating {
                current: Mutex::new(Ok(k.into())),
                reads: Mutex::new(0),
                refused: Mutex::new(0),
            }
        }
        fn reads(&self) -> usize {
            *self.reads.lock().unwrap()
        }
    }
    impl Issuer for Rotating {
        fn current_keyset(&self) -> Result<String, String> {
            *self.reads.lock().unwrap() += 1;
            self.current.lock().unwrap().clone()
        }
        fn transfer(&self, request: &TransferRequest) -> TransferOutcome {
            if let Ok(k) = &*self.current.lock().unwrap() {
                if *k != request.keyset {
                    *self.refused.lock().unwrap() += 1;
                    return TransferOutcome::Refused {
                        code: "keyset_retired".into(),
                        why: "retired".into(),
                    };
                }
            }
            Script(Mutex::new(vec![]), Mutex::new(0)).transfer(request)
        }
    }

    fn kept_keysets(issues: &Issues) -> Vec<String> {
        issues.data.kept.iter().map(|k| k.keyset.clone()).collect()
    }

    #[test]
    fn the_issuer_keyset_is_cached_for_the_ttl_and_read_again_after_it() {
        let (mut issues, keys, _) = setup("cache-ttl");
        let service_kp = kp(1);
        let mut ctx = IssueContext {
            service: ServiceDid::from(service_kp.public_key()),
            service_keypair: &service_kp,
            fee_per_byte: 0,
            current: keys.current(),
            previous: None,
            now: 1000,
            esp_keyset_ttl_secs: 60,
            max_items: crate::config::DEFAULT_MAX_ITEMS,
        };
        let issuer = Rotating::new(&"11".repeat(8));
        issue(&mut issues, &issuer, &request(&[1]), &ctx).unwrap();
        ctx.now = 1059;
        issue(&mut issues, &issuer, &request(&[1]), &ctx).unwrap();
        assert_eq!(
            issuer.reads(),
            1,
            "within the TTL the cached keyset is used"
        );
        ctx.now = 1060;
        issue(&mut issues, &issuer, &request(&[1]), &ctx).unwrap();
        assert_eq!(issuer.reads(), 2, "after the TTL it is read again");
        assert_eq!(issues.esp_keyset, Some(("11".repeat(8), 1060)));
    }

    #[test]
    fn a_keyset_retired_refusal_refreshes_the_cache_reblinds_and_retries_once() {
        let (mut issues, keys, _) = setup("cache-retired");
        let service_kp = kp(1);
        let ctx = IssueContext {
            service: ServiceDid::from(service_kp.public_key()),
            service_keypair: &service_kp,
            fee_per_byte: 0,
            current: keys.current(),
            previous: None,
            now: 1000,
            esp_keyset_ttl_secs: 3600,
            max_items: crate::config::DEFAULT_MAX_ITEMS,
        };
        let issuer = Rotating::new(&"11".repeat(8));
        issue(&mut issues, &issuer, &request(&[1]), &ctx).unwrap();
        // The Issuer rotates well inside the TTL.
        *issuer.current.lock().unwrap() = Ok("33".repeat(8));
        let r = issue(&mut issues, &issuer, &request(&[2]), &ctx).unwrap();
        assert!(matches!(r.groups[0], GroupResult::Issued { .. }));
        assert_eq!(*issuer.refused.lock().unwrap(), 1);
        assert_eq!(issuer.reads(), 2, "read again at once on keyset_retired");
        assert_eq!(kept_keysets(&issues), vec!["11".repeat(8), "33".repeat(8)]);
        assert_eq!(issues.esp_keyset, Some(("33".repeat(8), 1000)));
    }

    #[test]
    fn a_failed_read_uses_the_cached_keyset_and_502_only_with_none_cached() {
        let (mut issues, keys, _) = setup("cache-502");
        let service_kp = kp(1);
        let mut ctx = IssueContext {
            service: ServiceDid::from(service_kp.public_key()),
            service_keypair: &service_kp,
            fee_per_byte: 0,
            current: keys.current(),
            previous: None,
            now: 1000,
            esp_keyset_ttl_secs: 60,
            max_items: crate::config::DEFAULT_MAX_ITEMS,
        };
        let issuer = Rotating::new(&"11".repeat(8));
        *issuer.current.lock().unwrap() = Err("down".into());
        let e = issue(&mut issues, &issuer, &request(&[1]), &ctx).unwrap_err();
        assert_eq!((e.status, e.code), (502, "issuer_unavailable"));
        assert!(issues.data.entries.is_empty(), "nothing was stored");
        *issuer.current.lock().unwrap() = Ok("11".repeat(8));
        issue(&mut issues, &issuer, &request(&[1]), &ctx).unwrap();
        // Past the TTL with the keyset endpoint down: the cached one serves.
        ctx.now = 5000;
        *issuer.current.lock().unwrap() = Err("down".into());
        let r = issue(&mut issues, &issuer, &request(&[2]), &ctx).unwrap();
        assert!(matches!(r.groups[0], GroupResult::Issued { .. }));
        assert_eq!(kept_keysets(&issues), vec!["11".repeat(8), "11".repeat(8)]);
    }

    #[test]
    fn every_keyset_field_is_in_the_issue_and_spend_request_hashes() {
        let a = request(&[1, 2]);
        let mut b = a.clone();
        b.groups[1].notes[0].keyset = "23".repeat(8);
        assert_ne!(a.hash(), b.hash(), "a handed note's keyset is hashed");
        // Lengths keep the boundary: "ab"+"c" is not "a"+"bc".
        let mut c = a.clone();
        c.groups[1].notes[0].keyset = String::new();
        assert_ne!(a.hash(), c.hash());
        assert_eq!(a.hash(), a.clone().hash());

        let n = note(4);
        let input = |k: &str| crate::api::FspInput {
            keyset: k.into(),
            input: mint_types::MintInput {
                amount: Amount::from_units(4),
                note: n.note,
                spend_signature: secp256k1::schnorr::Signature::from_slice(&[1u8; 64]).unwrap(),
            },
        };
        let spend = |ks: &[&str]| crate::api::SpendRequest {
            inputs: ks.iter().map(|k| input(k)).collect(),
            outputs: vec![],
            pay: 8,
            memo: String::new(),
        };
        let x = spend(&["aa", "b"]);
        assert_ne!(
            x.hash(),
            spend(&["a", "ab"]).hash(),
            "keysets are length-prefixed"
        );
        assert_ne!(x.hash(), spend(&["aa", "c"]).hash());
        assert_eq!(x.hash(), spend(&["aa", "b"]).hash());
    }
    /// An Issuer stand-in that keeps a request the way the AIR one does: the
    /// first call saves one and gets no answer, and the retry must be handed it
    /// back; then it answers and the request is over.
    struct Keeping {
        seen: Mutex<Vec<Option<AirRequest>>>,
    }

    fn in_flight() -> AirRequest {
        AirRequest {
            request_id: "ab".repeat(32),
            mint: "02".repeat(33),
            r: "cd".repeat(32),
            slots: vec![4],
            created_at: 5,
            body_hash: "ef".repeat(32),
        }
    }

    impl Issuer for Keeping {
        fn current_keyset(&self) -> Result<String, String> {
            Ok("11".repeat(8))
        }
        fn transfer(&self, request: &TransferRequest) -> TransferOutcome {
            Script(Mutex::new(vec![]), Mutex::new(0)).transfer(request)
        }
        fn transfer_kept(
            &self,
            request: &TransferRequest,
            kept: Option<AirRequest>,
            save: crate::esp_client::Save,
        ) -> (TransferOutcome, Option<AirRequest>) {
            let first = kept.is_none();
            self.seen.lock().unwrap().push(kept);
            if first {
                save(&in_flight()).unwrap();
                (
                    TransferOutcome::Unknown("no reply".into()),
                    Some(in_flight()),
                )
            } else {
                (self.transfer(request), None)
            }
        }
    }

    fn context<'a>(keys: &'a Keysets, service_kp: &'a Keypair) -> IssueContext<'a> {
        IssueContext {
            service: ServiceDid::from(service_kp.public_key()),
            service_keypair: service_kp,
            fee_per_byte: 0,
            current: keys.current(),
            previous: None,
            now: 0,
            esp_keyset_ttl_secs: 3600,
            max_items: crate::config::DEFAULT_MAX_ITEMS,
        }
    }

    #[test]
    fn a_request_in_flight_is_kept_with_its_transfer_and_handed_back_to_the_retry() {
        let (mut issues, keys, d) = setup("kept");
        let service_kp = kp(1);
        let ctx = context(&keys, &service_kp);
        let req = request(&[10]);
        let issuer = Keeping {
            seen: Mutex::new(vec![]),
        };
        let e = issue(&mut issues, &issuer, &req, &ctx).unwrap_err();
        assert_eq!((e.status, e.code), (502, "issuer_unavailable"));
        // It is in the file, in the pending group of the transfer it carries.
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(d.join(FILE_NAME)).unwrap()).unwrap();
        let entry = &file["entries"][req.hash()];
        assert_eq!(entry["entry"], "pending");
        assert_eq!(entry["groups"][0]["air"]["request_id"], "ab".repeat(32));
        assert_eq!(entry["groups"][0]["air"]["slots"], serde_json::json!([4]));
        // A restart, then the retry: it is handed the same request.
        let mut issues = Issues::open(&d).unwrap();
        issue(&mut issues, &issuer, &req, &ctx).unwrap();
        let seen = issuer.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].is_none());
        assert_eq!(seen[1].as_ref(), Some(&in_flight()));
        // The request is over with the transfer: nothing is kept in the file.
        let text = std::fs::read_to_string(d.join(FILE_NAME)).unwrap();
        assert!(!text.contains("\"air\""), "{text}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_pending_group_without_a_kept_request_reads_as_none_and_is_written_without_one() {
        let (mut issues, keys, d) = setup("old-file");
        let service_kp = kp(1);
        let ctx = context(&keys, &service_kp);
        let req = request(&[10]);
        let issuer = Script(
            Mutex::new(vec![TransferOutcome::Unknown("timeout".into())]),
            Mutex::new(0),
        );
        issue(&mut issues, &issuer, &req, &ctx).unwrap_err();
        // What a 0.5.0 file looks like: the same, with no `air` key at all.
        let text = std::fs::read_to_string(d.join(FILE_NAME)).unwrap();
        assert!(!text.contains("\"air\""), "{text}");
        let issues = Issues::open(&d).unwrap();
        match issues.data.entries.get(&req.hash()) {
            Some(Entry::Pending(p)) => assert!(p.groups[0].air.is_none()),
            other => panic!("expected a pending entry, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
