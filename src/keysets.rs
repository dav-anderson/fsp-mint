//! The FSP signing keys.
//!
//! A keyset is a 32-byte secret. The key for each power-of-two amount is
//! derived from it, and its id (16 hex characters) is a hash of its key for 1
//! unit, so a wallet can tell which keyset signed a note.
//!
//! **Rotation.** At genesis there is one keyset (epoch 0) and one spent list.
//! Rotation is off while `rotation.activation_hours` is 0, and until the first
//! FSP is issued. It starts `rotation.activation_hours` after that first issue
//! (recorded in `first_issue.json`). From then the epoch is
//! `1 + (hours since activation) / rotation.period_hours`.
//! Each time the epoch moves on, a new keyset is made and becomes current, the
//! one before it becomes previous, and anything older is deleted with its spent
//! list. So after activation there are at most two keysets: current (new notes
//! are signed with it) and previous (its notes can still be spent or swapped,
//! until the next rotation). A note of a deleted keyset cannot be spent: FSP left
//! offline for a whole period after it became previous is lost.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use mint_types::Amount;
use rand::RngCore;
use rand::rngs::OsRng;
use secp256k1::{PublicKey, SECP256K1, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store;

/// The most retired keyset IDs kept: 1 MB of 8-byte IDs (131,072). A
/// request naming a listed ID gets `keyset_retired` (its notes expired); any
/// other unknown ID gets `unknown_keyset`, so a wallet can tell expired notes
/// from junk and explain a loss. When the list is full the oldest ID is dropped
/// to make room, which only turns a very old `keyset_retired` into
/// `unknown_keyset`. 131,072 covers hourly rotation for about 15 years; a mint
/// rotating yearly never comes close. It is a constant, the same in every
/// mint, and deliberately not a setting: it is too cheap to be worth tuning.
pub const RETIRED_KEYSETS_CAP: usize = 131_072;
pub const FILE_NAME: &str = "keysets.json";
const KEY_TAG: &[u8] = b"fsp-mint/key/v1";
const ID_TAG: &[u8] = b"fsp-mint/keyset-id/v1";

#[derive(Clone, Serialize, Deserialize)]
struct StoredSet {
    epoch: u64,
    secret: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredSets {
    sets: Vec<StoredSet>,
    /// The ids of deleted keysets, oldest first. Kept in the key file, so the
    /// one atomic save that drops a keyset's secret also lists its id: a crash
    /// can never leave a deleted keyset unlisted.
    #[serde(default)]
    retired: Vec<String>,
}

/// One keyset: its epoch, id and the key for every power of two.
#[derive(Clone)]
pub struct Keyset {
    pub epoch: u64,
    pub id: String,
    secret: [u8; 32],
    keys: Vec<SecretKey>,
    pub public: BTreeMap<u64, PublicKey>,
}

impl std::fmt::Debug for Keyset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keyset({}, epoch {})", self.id, self.epoch)
    }
}

fn derive(secret: &[u8; 32], units: u64) -> SecretKey {
    for counter in 0..=u8::MAX {
        let mut h = Sha256::new();
        h.update(KEY_TAG);
        h.update(secret);
        h.update(units.to_be_bytes());
        h.update([counter]);
        if let Ok(k) = SecretKey::from_slice(&h.finalize()) {
            return k;
        }
    }
    unreachable!("256 consecutive hashes outside the curve order")
}

impl Keyset {
    fn from_secret(epoch: u64, secret: [u8; 32]) -> Self {
        let keys: Vec<SecretKey> = (0..64).map(|bit| derive(&secret, 1u64 << bit)).collect();
        let public = keys
            .iter()
            .enumerate()
            .map(|(bit, k)| (1u64 << bit, k.public_key(SECP256K1)))
            .collect::<BTreeMap<_, _>>();
        let mut h = Sha256::new();
        h.update(ID_TAG);
        h.update(public[&1].serialize());
        let id = hex::encode(&h.finalize()[..8]);
        Keyset {
            epoch,
            id,
            secret,
            keys,
            public,
        }
    }

    fn generate(epoch: u64) -> Self {
        let mut secret = [0u8; 32];
        OsRng.fill_bytes(&mut secret);
        Self::from_secret(epoch, secret)
    }

    /// The signing key for a power-of-two amount, or `None` for any other.
    pub fn key(&self, amount: Amount) -> Option<&SecretKey> {
        crate::is_denomination(amount.units)
            .then(|| &self.keys[amount.units.trailing_zeros() as usize])
    }
}

/// The rotation settings, in seconds. Rotation is off while `activation_secs`
/// is 0 or no FSP has been issued yet. The first rollover is `activation_secs`
/// after the first issue, then one every `period_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rotation {
    pub first_issue: Option<u64>,
    pub activation_secs: u64,
    pub period_secs: u64,
}

impl Rotation {
    /// When rotation starts (the first rollover), if it is on and FSP was issued.
    pub fn activation(&self) -> Option<u64> {
        let first = self.first_issue?;
        (self.activation_secs > 0).then(|| first.saturating_add(self.activation_secs))
    }

    /// When epoch `e` (1 and later) starts, in Unix seconds.
    pub fn start_of(&self, epoch: u64) -> Option<u64> {
        let t0 = self.activation()?;
        (epoch >= 1).then(|| t0.saturating_add((epoch - 1).saturating_mul(self.period_secs.max(1))))
    }

    /// The epoch at `now_secs`: 0 before activation (or with rotation off).
    pub fn epoch_at(&self, now_secs: u64) -> u64 {
        match self.activation() {
            Some(t0) if now_secs >= t0 => 1 + (now_secs - t0) / self.period_secs.max(1),
            _ => 0,
        }
    }
}

pub const FIRST_ISSUE_FILE: &str = "first_issue.json";

#[derive(Serialize, Deserialize)]
struct FirstIssue {
    first_issue: u64,
}

/// The time of the mint's first issuance, if there has been one.
pub fn load_first_issue(data_dir: &Path) -> io::Result<Option<u64>> {
    Ok(store::load::<FirstIssue>(&data_dir.join(FIRST_ISSUE_FILE))?.map(|f| f.first_issue))
}

/// Record the first issuance at `now`, once: if the file exists, it is left as
/// it is and its time returned. Never changed afterwards. It is written whole
/// ([`store::write_secret`]), so a crash never leaves an empty or half file that
/// would stop the next start.
pub fn record_first_issue(data_dir: &Path, now: u64) -> io::Result<u64> {
    if let Some(t) = load_first_issue(data_dir)? {
        return Ok(t);
    }
    let path = data_dir.join(FIRST_ISSUE_FILE);
    let text = serde_json::to_string(&FirstIssue { first_issue: now }).map_err(io::Error::other)?;
    match store::write_secret(&path, &text) {
        Ok(()) => Ok(now),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => load_first_issue(data_dir)?.ok_or(e),
        Err(e) => Err(e),
    }
}

/// The live keysets: at most two (previous, current), oldest first.
pub struct Keysets {
    path: PathBuf,
    sets: Vec<Keyset>,
    retired: Vec<String>,
    /// The cap on retired ids: [`RETIRED_KEYSETS_CAP`] (smaller only in tests).
    keep: usize,
}

impl Keysets {
    /// Load the key file, or make the genesis keyset if there is none.
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        let path = data_dir.join(FILE_NAME);
        let retired;
        let sets = match store::load::<StoredSets>(&path)? {
            Some(stored) => {
                retired = stored.retired;
                let mut sets = Vec::new();
                for s in stored.sets {
                    let bytes = hex::decode(&s.secret)
                        .ok()
                        .and_then(|b| <[u8; 32]>::try_from(b).ok())
                        .ok_or_else(|| {
                            io::Error::other(format!("{} is damaged: bad secret", path.display()))
                        })?;
                    sets.push(Keyset::from_secret(s.epoch, bytes));
                }
                if sets.is_empty() {
                    return Err(io::Error::other(format!(
                        "{} has no keyset",
                        path.display()
                    )));
                }
                sets
            }
            None => {
                let k = Keysets {
                    path: path.clone(),
                    sets: vec![Keyset::generate(0)],
                    retired: Vec::new(),
                    keep: RETIRED_KEYSETS_CAP,
                };
                k.save()?;
                return Ok(k);
            }
        };
        Ok(Keysets {
            path,
            sets,
            retired,
            keep: RETIRED_KEYSETS_CAP,
        })
    }

    /// Test hook: a smaller cap than [`RETIRED_KEYSETS_CAP`], applied now and
    /// saved. Production always uses the constant.
    #[doc(hidden)]
    pub fn set_retired_keep(&mut self, keep: usize) -> io::Result<()> {
        self.keep = keep;
        let before = self.retired.len();
        trim(&mut self.retired, keep);
        if self.retired.len() != before {
            self.save()?;
        }
        Ok(())
    }

    /// Whether `id` is a deleted keyset still on the retired list.
    pub fn is_retired(&self, id: &str) -> bool {
        self.retired.iter().any(|r| r == id)
    }

    /// The retired list, oldest first.
    pub fn retired(&self) -> &[String] {
        &self.retired
    }

    fn save(&self) -> io::Result<()> {
        let stored = StoredSets {
            sets: self
                .sets
                .iter()
                .map(|s| StoredSet {
                    epoch: s.epoch,
                    secret: hex::encode(s.secret),
                })
                .collect(),
            retired: self.retired.clone(),
        };
        store::save(&self.path, &stored)
    }

    pub fn current(&self) -> &Keyset {
        self.sets.last().expect("at least one keyset")
    }

    pub fn previous(&self) -> Option<&Keyset> {
        (self.sets.len() >= 2).then(|| &self.sets[self.sets.len() - 2])
    }

    pub fn get(&self, id: &str) -> Option<&Keyset> {
        self.sets.iter().find(|s| s.id == id)
    }

    pub fn all(&self) -> &[Keyset] {
        &self.sets
    }

    /// Move on to `epoch` if it is later than the current one. Returns the ids
    /// of the keysets deleted (their spent lists go with them). The key file is
    /// saved before anything is returned. Several missed epochs are applied one
    /// at a time, so a mint that was down for two periods keeps only the newest.
    pub fn rotate_to(&mut self, epoch: u64) -> io::Result<Vec<String>> {
        let mut deleted = Vec::new();
        if epoch <= self.current().epoch {
            return Ok(deleted);
        }
        let mut sets = self.sets.clone();
        let mut at = self.current().epoch;
        while at < epoch {
            at += 1;
            sets.push(Keyset::generate(at));
            while sets.len() > 2 {
                deleted.push(sets.remove(0).id);
            }
        }
        let old_retired = self.retired.clone();
        self.retired.extend(deleted.iter().cloned());
        trim(&mut self.retired, self.keep);
        let old = std::mem::replace(&mut self.sets, sets);
        if let Err(e) = self.save() {
            self.sets = old;
            self.retired = old_retired;
            return Err(e);
        }
        Ok(deleted)
    }
}

fn trim(list: &mut Vec<String>, keep: usize) {
    if list.len() > keep {
        list.drain(..list.len() - keep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fsp-keysets-{name}-{}", {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            format!(
                "{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        }));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn epochs_count_from_the_first_issue() {
        let off = Rotation {
            first_issue: Some(0),
            activation_secs: 0,
            period_secs: 10,
        };
        assert_eq!(off.epoch_at(u64::MAX / 2), 0);
        let none_yet = Rotation {
            first_issue: None,
            activation_secs: 5,
            period_secs: 10,
        };
        assert_eq!(none_yet.epoch_at(u64::MAX / 2), 0);
        let on = Rotation {
            first_issue: Some(1000),
            activation_secs: 50,
            period_secs: 10,
        };
        assert_eq!(on.activation(), Some(1050));
        assert_eq!(on.epoch_at(1049), 0);
        assert_eq!(on.epoch_at(1050), 1);
        assert_eq!(on.epoch_at(1059), 1);
        assert_eq!(on.epoch_at(1060), 2);
        assert_eq!(on.start_of(0), None);
        assert_eq!(on.start_of(3), Some(1070));
    }

    #[test]
    fn the_first_issue_is_written_once_and_survives_a_restart() {
        let d = dir("first");
        assert_eq!(load_first_issue(&d).unwrap(), None);
        assert_eq!(record_first_issue(&d, 500).unwrap(), 500);
        assert_eq!(record_first_issue(&d, 900).unwrap(), 500, "never changed");
        // Fails if the temporary file is left behind or the record is readable
        // by others.
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.join(FIRST_ISSUE_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(load_first_issue(&d).unwrap(), Some(500));
        std::fs::write(d.join(FIRST_ISSUE_FILE), "junk").unwrap();
        let e = load_first_issue(&d).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::Other);
        let shown = d.join(FIRST_ISSUE_FILE).display().to_string();
        assert!(
            e.to_string().starts_with(&format!("{shown} is damaged: ")),
            "a damaged record is an error that names the file: {e}"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn genesis_then_rotation_keeps_two_sets() {
        let d = dir("rot");
        let mut k = Keysets::open(&d).unwrap();
        let genesis = k.current().id.clone();
        assert!(k.previous().is_none());
        assert_eq!(
            Keysets::open(&d).unwrap().current().id,
            genesis,
            "reloaded, not regenerated"
        );
        assert!(k.rotate_to(0).unwrap().is_empty());
        assert!(k.rotate_to(1).unwrap().is_empty());
        assert_eq!(k.previous().unwrap().id, genesis);
        let one = k.current().id.clone();
        assert_eq!(k.rotate_to(2).unwrap(), vec![genesis.clone()]);
        assert_eq!(k.previous().unwrap().id, one);
        assert!(k.get(&genesis).is_none());
        // Down for two periods: both old sets go, and the set made for the
        // missed epoch (which signed nothing) is previous.
        let two = k.current().id.clone();
        let gone = k.rotate_to(4).unwrap();
        assert!(gone.contains(&one) && gone.contains(&two), "{gone:?}");
        assert_eq!(k.all().len(), 2);
        assert_eq!(k.previous().unwrap().epoch, 3);
        let reopened = Keysets::open(&d).unwrap();
        assert_eq!(reopened.current().id, k.current().id);
        assert_eq!(reopened.current().epoch, 4);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn deleted_keysets_are_listed_in_the_same_save_and_the_oldest_evicted_at_the_cap() {
        assert_eq!(RETIRED_KEYSETS_CAP, 131_072, "1 MB of 8-byte ids");
        let d = dir("retired");
        let mut k = Keysets::open(&d).unwrap();
        k.set_retired_keep(2).unwrap();
        let mut ids = vec![k.current().id.clone()];
        for e in 1..=4 {
            k.rotate_to(e).unwrap();
            ids.push(k.current().id.clone());
        }
        // 0, 1, 2 deleted; the cap of 2 evicted 0.
        assert_eq!(k.retired(), &ids[1..3]);
        assert!(
            !k.is_retired(&ids[0]) && k.get(&ids[0]).is_none(),
            "evicted: now unknown"
        );
        assert!(k.is_retired(&ids[2]));
        // The list is in the key file itself: reopened, it is the same, and a
        // deleted secret is never there without its id (one atomic save).
        let again = Keysets::open(&d).unwrap();
        assert_eq!(again.retired(), &ids[1..3]);
        assert_eq!(again.all().len(), 2);
    }

    #[test]
    fn keys_only_for_powers_of_two() {
        let k = Keyset::generate(0);
        assert!(k.key(Amount::from_units(1)).is_some());
        assert!(k.key(Amount::from_units(1 << 63)).is_some());
        assert!(k.key(Amount::from_units(3)).is_none());
        assert!(k.key(Amount::from_units(0)).is_none());
        assert_eq!(k.id.len(), 16);
    }
}
