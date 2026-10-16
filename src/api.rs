//! The requests and replies of fsp-mint, shared with the wallet.

use std::collections::BTreeMap;

use mint_types::{MintInput, MintOutput, Note};
use secp256k1::{PublicKey, SecretKey, XOnlyPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The longest spend memo, in bytes.
pub const MAX_MEMO_BYTES: usize = 256;

/// An ESP note handed to the service, with its spend secret (the wallet's
/// hand-over format).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandedEsp {
    pub amount: u64,
    pub note: Note,
    pub spend_secret: SecretKey,
    /// The ESP keyset that signed the note (16 hex). Required: the Issuer
    /// verifies each note by the keyset it names.
    #[serde(default)]
    pub keyset: String,
}

/// The ESP notes of one redeemable-after day, and the FSP outputs they pay
/// for. Each group is exact on its own: the notes are worth the outputs plus
/// the group's issue fee.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueGroup {
    pub redeemable_after_day: u64,
    pub notes: Vec<HandedEsp>,
    pub outputs: Vec<MintOutput>,
}

/// `POST /issue`: ESP named for this service in, FSP out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueRequest {
    pub service: XOnlyPublicKey,
    pub groups: Vec<IssueGroup>,
}

const ISSUE_TAG: &[u8] = b"fsp-mint/issue-request/v2";

impl IssueRequest {
    /// The key the request is stored under: a hash of everything in it. A
    /// retry sends the same request and gets the same stored answer.
    pub fn hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(ISSUE_TAG);
        h.update(self.service.serialize());
        h.update((self.groups.len() as u64).to_be_bytes());
        for g in &self.groups {
            h.update(g.redeemable_after_day.to_be_bytes());
            h.update((g.notes.len() as u64).to_be_bytes());
            for n in &g.notes {
                h.update(n.amount.to_be_bytes());
                h.update(n.note.nonce.0.serialize());
                h.update(n.note.signature.serialize());
                h.update(n.spend_secret.secret_bytes());
                // The keyset field too, with its length: two requests that
                // differ only in a keyset never share a hash.
                h.update((n.keyset.len() as u64).to_be_bytes());
                h.update(n.keyset.as_bytes());
            }
            h.update((g.outputs.len() as u64).to_be_bytes());
            for o in &g.outputs {
                h.update(o.amount.units.to_be_bytes());
                h.update(o.blind_nonce.0.serialize());
            }
        }
        hex::encode(h.finalize())
    }
}

/// The canonical size of one issue group, the fee is worked out on:
/// 16 bytes (day and counts), 106 a note (amount 8, nonce 33, signature 33,
/// secret 32), 41 an output (amount 8, blind nonce 33).
pub fn issue_group_size(notes: usize, outputs: usize) -> u64 {
    16 + 106 * notes as u64 + 41 * outputs as u64
}

/// What happened to one group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum GroupResult {
    /// The ESP was transferred to the service and kept, and the FSP outputs
    /// were signed: one blind signature and key per output, in order.
    Issued {
        keyset: String,
        signatures: Vec<PublicKey>,
        keys: Vec<PublicKey>,
        fee: u64,
    },
    /// The Issuer refused the ESP transfer (nothing was burned there), so no
    /// FSP was issued for this group. `code` is fsp-mint's, `why` the Issuer's.
    Refused { code: String, why: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueResponse {
    pub groups: Vec<GroupResult>,
    /// The issue fee rate the request was accepted at.
    pub fee_per_byte: u64,
}

/// An FSP note being spent: its keyset and the note with a spend signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FspInput {
    pub keyset: String,
    pub input: MintInput,
}

/// `POST /spend`: a swap with change. The inputs are worth `pay` plus the
/// outputs. `pay` goes to the service (0 for a plain swap, for example from the
/// previous keyset to the current one); the outputs are new notes for the
/// payer, in the current keyset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpendRequest {
    pub inputs: Vec<FspInput>,
    pub outputs: Vec<MintOutput>,
    pub pay: u64,
    #[serde(default)]
    pub memo: String,
}

const SPEND_DIGEST_TAG: &[u8] = b"fsp-mint/spend/v1";
const SPEND_HASH_TAG: &[u8] = b"fsp-mint/spend-request/v2";

impl SpendRequest {
    /// What every spend signature covers.
    pub fn digest(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(SPEND_DIGEST_TAG);
        h.update(self.pay.to_be_bytes());
        h.update((self.memo.len() as u64).to_be_bytes());
        h.update(self.memo.as_bytes());
        h.update((self.inputs.len() as u64).to_be_bytes());
        for i in &self.inputs {
            h.update(i.keyset.as_bytes());
            h.update(i.input.amount.units.to_be_bytes());
            h.update(i.input.note.nonce.0.serialize());
        }
        h.update((self.outputs.len() as u64).to_be_bytes());
        for o in &self.outputs {
            h.update(o.amount.units.to_be_bytes());
            h.update(o.blind_nonce.0.serialize());
        }
        h.finalize().into()
    }

    /// The key the answer is stored under (digest and every signature).
    pub fn hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(SPEND_HASH_TAG);
        h.update(self.digest());
        for i in &self.inputs {
            h.update(i.input.spend_signature.as_ref());
        }
        // Every keyset field, length-prefixed (the digest has them without
        // lengths), so no two different requests share a hash.
        for i in &self.inputs {
            h.update((i.keyset.len() as u64).to_be_bytes());
            h.update(i.keyset.as_bytes());
        }
        hex::encode(h.finalize())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpendResponse {
    pub keyset: String,
    pub signatures: Vec<PublicKey>,
    pub keys: Vec<PublicKey>,
    pub paid: u64,
}

/// `GET /keys`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeysResponse {
    pub service: Option<XOnlyPublicKey>,
    pub current: String,
    pub previous: Option<String>,
    /// For each live keyset, the public key for each amount.
    pub keysets: BTreeMap<String, BTreeMap<u64, PublicKey>>,
    /// When the current keyset is replaced (Unix seconds), if rotation is on.
    pub next_rotation: Option<u64>,
}

/// `GET /fees`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeesResponse {
    pub issue_fee_per_byte: u64,
    pub issue_group_fixed_bytes: u64,
    pub issue_note_bytes: u64,
    pub issue_output_bytes: u64,
    pub max_request_bytes: u64,
    /// `issue.max_items` and `spend.max_items`: the most notes or outputs in one
    /// issue group or spend. `None` from a mint that does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_max_items: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_max_items: Option<u64>,
}

/// A refusal's body: `{"error", "code"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub error: String,
    pub code: String,
}

/// One live keyset in `GET /rotation`. Times are Unix seconds (UTC).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeysetWindow {
    pub id: String,
    pub epoch: u64,
    /// When it became current. `None` for the genesis keyset (before rotation).
    pub starts: Option<u64>,
    /// When it stops being current (the next rotation). `None` while rotation is off.
    pub current_until: Option<u64>,
    /// When it is deleted with its spent list: its notes cannot be spent after
    /// this. `None` while rotation is off.
    pub retires: Option<u64>,
}

/// `GET /rotation`: the key rotation schedule, public and read-only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotationResponse {
    /// Whether rotation has started (the activation time has passed).
    pub active: bool,
    /// When the mint first issued FSP (Unix seconds), if it has.
    pub first_issue: Option<u64>,
    /// `rotation.activation_hours`: hours from the first issue to the first
    /// rollover. 0 means rotation is off.
    pub rotation_activation_hours: u64,
    /// The first rollover (first issue + activation hours), once known.
    pub activation: Option<u64>,
    pub rotation_period_hours: u64,
    pub current: KeysetWindow,
    pub previous: Option<KeysetWindow>,
    /// When the next rotation happens (Unix seconds), if rotation is set.
    pub next_rotation: Option<u64>,
    /// The mint's clock when it answered.
    pub now: u64,
}
