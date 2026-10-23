//! `fsp-mint export-esp`: write the ESP this mint has kept to hand-over files.
//!
//! One file for each redeemable-after day, `esp-<day>-<now>.json` in the output
//! folder, for the service's ecash-wallet to `receive` and `redeem`. The file is
//! [`sp_wire::esp::handover::Handover`], the type the wallet reads it with: the
//! service, the day, and each note with its amount, spend secret and the ESP
//! keyset that signed it. **Whoever holds the file holds the notes** (the notes
//! come with their spend secrets): keep it private.
//!
//! The notes that were written are marked `exported` in `issues.json`, so a
//! second run writes nothing for them. The mint must not be running: this opens
//! `issues.json` and saves it, as `serve` does.
//!
//! A kept note with no keyset (kept before keysets existed) cannot be written:
//! the hand-over file requires one, and the wallet refuses a file without it.
//! The export stops before it writes anything and names each such note.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sp_wire::ServiceDid;
use sp_wire::esp::handover::{HandedNote, Handover};

use crate::issue::{Issues, KeptEsp};
use crate::store;

/// Write every kept ESP note that has not been exported to `out` (made if it is
/// missing), for `service`, and mark them exported. `now` (Unix seconds) is part
/// of each file's name. Returns each file written with its number of notes, by
/// day; nothing to export is an empty list.
pub fn export_esp(
    data_dir: &Path,
    out: &Path,
    service: ServiceDid,
    now: u64,
) -> Result<Vec<(PathBuf, usize)>, String> {
    let mut issues = Issues::open(data_dir).map_err(|e| e.to_string())?;
    let mut by_day: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (i, k) in issues.data.kept.iter().enumerate() {
        if !k.exported {
            by_day.entry(k.redeemable_after_day).or_default().push(i);
        }
    }
    let without_keyset: Vec<String> = by_day
        .values()
        .flatten()
        .map(|&i| &issues.data.kept[i])
        .filter(|k| k.keyset.is_empty())
        .map(describe)
        .collect();
    if !without_keyset.is_empty() {
        return Err(format!(
            "cannot export: {} kept ESP note{} no keyset (kept before keysets), and a hand-over file needs one on every note. Nothing was written. {}",
            without_keyset.len(),
            if without_keyset.len() == 1 {
                " names"
            } else {
                "s name"
            },
            without_keyset.join("; ")
        ));
    }
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let mut written = Vec::new();
    for (day, idx) in &by_day {
        let handover = Handover {
            service,
            redeemable_after_day: *day,
            notes: idx
                .iter()
                .map(|&i| {
                    let k = &issues.data.kept[i];
                    HandedNote {
                        amount: k.amount,
                        note: k.note,
                        spend_secret: k.spend_secret,
                        keyset: k.keyset.clone(),
                    }
                })
                .collect(),
        };
        let file = out.join(format!("esp-{day}-{now}.json"));
        store::save(&file, &handover).map_err(|e| e.to_string())?;
        written.push((file, idx.len()));
    }
    for idx in by_day.values() {
        for &i in idx {
            issues.data.kept[i].exported = true;
        }
    }
    issues.save().map_err(|e| e.to_string())?;
    Ok(written)
}

/// A kept note as the refusal names it: its nonce, amount, day and the issue
/// request it came from.
fn describe(k: &KeptEsp) -> String {
    format!(
        "note {} ({} units, redeemable after day {}, from issue request {})",
        hex::encode(k.note.nonce.0.serialize()),
        k.amount,
        k.redeemable_after_day,
        k.request
    )
}
