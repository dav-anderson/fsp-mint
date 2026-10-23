//! The operator audit: what the mint owes and what it holds.
//!
//! **Outstanding FSP.** For each key set, `issued + change - spent`:
//!
//! - `issued` is the FSP signed for the key set by an issue (kind 11). It is counted
//!   in `issues.json`, in the same atomic save that stores the answer, so a
//!   crash cannot separate the counter from the issue it counts, and a retried
//!   request (a stored answer) is never counted twice.
//! - `change` is the FSP signed by a spend (kind 12) as change, and `spent` is the
//!   value of the notes a spend used up. Both are written in the spend's own
//!   log line (`spends-<keyset>.jsonl`), so the counters are rebuilt from the
//!   logs at every start. A spend that was never answered is not in the log and
//!   not in the counters, and its notes are unspent again.
//!
//! Over all key sets `issued + change - spent` is `issued - paid`: a spend
//! moves value between key sets and only `pay` leaves. When a key set is
//! deleted at rotation, its spend counters are first copied into
//! `ledger.json` (once per key set, never changed), because the log that held
//! them goes with the key set. Its notes have expired: they are reported as
//! `expired`, not as outstanding. A crash between the deletion and the copy is
//! repaired at the next start from the logs, which are still on disk.
//!
//! **ESP in reserve.** The ESP this mint keeps from issues (`kept` in
//! `issues.json`), split into matured (`now / 86400 >= redeemable_after_day`,
//! the rule `sp-wire` and esp-mint use) and unmatured. A note written out by
//! `export-esp` is still in the figures, and is also shown as `exported_esp`:
//! the mint cannot tell whether the service's wallet has redeemed it yet.
//!
//! Dollar figures are not here: they need a payout rate, which this mint does
//! not hold (see `spi`).

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sp_wire::units::ESP_PER_PSP;

use crate::issue::{self, IssueData, KeptEsp};
use crate::store;

pub const LEDGER_FILE: &str = "ledger.json";

const SECONDS_PER_DAY: u64 = 86_400;

/// A whole number that crosses JSON as a decimal string, so that no reader
/// rounds it. Signed: an outstanding figure below zero is a finding, and is
/// reported as it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Num(pub i128);

impl fmt::Display for Num {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for Num {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Num {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Text(String),
            Number(u64),
        }
        match Either::deserialize(d)? {
            Either::Text(t) => t.parse().map(Num).map_err(serde::de::Error::custom),
            Either::Number(n) => Ok(Num(n as i128)),
        }
    }
}

/// The spend side of one key set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    /// FSP signed as change, by spends answered with this key set.
    #[serde(default)]
    pub change: Num,
    /// FSP of notes of this key set used up by spends.
    #[serde(default)]
    pub spent: Num,
}

/// The counters of key sets that have been deleted, copied before their logs
/// went. A key set is added once and never changed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub retired: BTreeMap<String, Counters>,
}

impl Ledger {
    pub fn load(dir: &Path) -> io::Result<Self> {
        Ok(store::load(&dir.join(LEDGER_FILE))?.unwrap_or_default())
    }

    pub fn save(&self, dir: &Path) -> io::Result<()> {
        store::save(&dir.join(LEDGER_FILE), self)
    }
}

/// The spend side of the mint, as the audit needs it.
#[derive(Clone, Debug, Default)]
pub struct SpendView {
    /// The live key sets, oldest first.
    pub live: Vec<String>,
    /// Counters of the live key sets (a key set without activity may be absent).
    pub counters: HashMap<String, Counters>,
    /// Counters of the deleted key sets.
    pub retired: BTreeMap<String, Counters>,
}

/// An amount in FSP or ESP units (they are the same unit) and in PSP.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Amount {
    pub units: Num,
    /// Decimal, exact: `units / ESP_PER_PSP`.
    pub psp: String,
}

pub fn amount(units: i128) -> Amount {
    Amount {
        units: Num(units),
        psp: psp_text(units),
    }
}

/// `units` as PSP, in decimals and exact (`ESP_PER_PSP` is a power of ten).
pub fn psp_text(units: i128) -> String {
    let per = ESP_PER_PSP as u128;
    let width = per.to_string().len() - 1;
    let abs = units.unsigned_abs();
    let sign = if units < 0 { "-" } else { "" };
    format!("{sign}{}.{:0width$}", abs / per, abs % per)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeysetLine {
    pub keyset: String,
    /// `live`, or `retired` (deleted at rotation: its notes expired).
    pub status: String,
    pub issued: Amount,
    pub change: Amount,
    pub spent: Amount,
    /// `issued + change - spent`.
    pub outstanding: Amount,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FspAudit {
    /// Live key sets first (oldest first), then retired ones.
    pub keysets: Vec<KeysetLine>,
    /// Outstanding FSP of the live key sets: what can still be spent.
    pub outstanding: Amount,
    /// Issued over all key sets, and the part that left as payments:
    /// `issued - paid` is the outstanding plus the expired.
    pub issued: Amount,
    pub paid: Amount,
    /// FSP that was outstanding in key sets that have since been deleted. It
    /// can no longer be spent.
    pub expired: Amount,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    pub notes: u64,
    pub esp: Amount,
    /// Of `esp`, the part written out by `export-esp`.
    pub exported_esp: Amount,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayLine {
    pub redeemable_after_day: u64,
    pub matured: bool,
    pub notes: u64,
    pub esp: Amount,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EspReserve {
    pub matured: Bucket,
    pub unmatured: Bucket,
    pub total: Bucket,
    pub by_day: Vec<DayLine>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub now: u64,
    /// `now / 86400`: the day matured is measured against.
    pub today: u64,
    pub fsp: FspAudit,
    pub esp_reserve: EspReserve,
    /// False when a key set has spent more than it issued and received.
    pub consistent: bool,
    pub warnings: Vec<String>,
}

#[derive(Default)]
struct Sum {
    notes: u64,
    esp: i128,
    exported: i128,
}

impl Sum {
    fn add(&mut self, k: &KeptEsp) {
        self.notes += 1;
        self.esp += k.amount as i128;
        if k.exported {
            self.exported += k.amount as i128;
        }
    }

    fn bucket(&self) -> Bucket {
        Bucket {
            notes: self.notes,
            esp: amount(self.esp),
            exported_esp: amount(self.exported),
        }
    }
}

/// The ESP in the kept list, matured and unmatured as of `today` (a day number).
pub fn reserve(kept: &[KeptEsp], today: u64) -> EspReserve {
    let (mut matured, mut unmatured) = (Sum::default(), Sum::default());
    let mut days: BTreeMap<u64, (u64, i128)> = BTreeMap::new();
    for k in kept {
        if today >= k.redeemable_after_day {
            matured.add(k);
        } else {
            unmatured.add(k);
        }
        let d = days.entry(k.redeemable_after_day).or_default();
        d.0 += 1;
        d.1 += k.amount as i128;
    }
    let total = Sum {
        notes: matured.notes + unmatured.notes,
        esp: matured.esp + unmatured.esp,
        exported: matured.exported + unmatured.exported,
    };
    EspReserve {
        matured: matured.bucket(),
        unmatured: unmatured.bucket(),
        total: total.bucket(),
        by_day: days
            .into_iter()
            .map(|(day, (notes, esp))| DayLine {
                redeemable_after_day: day,
                matured: today >= day,
                notes,
                esp: amount(esp),
            })
            .collect(),
    }
}

/// The report from the spend side, the issue side and the clock.
pub fn build(now: u64, view: &SpendView, issues: &IssueData) -> Report {
    let today = now / SECONDS_PER_DAY;
    let mut ids: Vec<(String, bool)> = view.live.iter().map(|k| (k.clone(), true)).collect();
    let mut gone: Vec<String> = view
        .retired
        .keys()
        .chain(issues.issued.keys())
        .filter(|k| !view.live.contains(k))
        .cloned()
        .collect();
    gone.sort();
    gone.dedup();
    ids.extend(gone.into_iter().map(|k| (k, false)));

    let mut lines = Vec::new();
    let (mut outstanding, mut expired) = (0i128, 0i128);
    let (mut issued_all, mut spent_all, mut change_all) = (0i128, 0i128, 0i128);
    let mut consistent = true;
    for (id, live) in ids {
        let c = if live {
            view.counters.get(&id).copied().unwrap_or_default()
        } else {
            view.retired.get(&id).copied().unwrap_or_default()
        };
        let issued = issues.issued.get(&id).copied().unwrap_or_default().0;
        let net = issued + c.change.0 - c.spent.0;
        consistent &= net >= 0;
        issued_all += issued;
        change_all += c.change.0;
        spent_all += c.spent.0;
        if live {
            outstanding += net;
        } else {
            expired += net;
        }
        lines.push(KeysetLine {
            keyset: id,
            status: if live { "live" } else { "retired" }.into(),
            issued: amount(issued),
            change: amount(c.change.0),
            spent: amount(c.spent.0),
            outstanding: amount(net),
        });
    }

    let mut warnings = Vec::new();
    if !consistent {
        warnings.push(
            "a key set has spent more than it issued and received as change: this is a fault to look into, not a rounding"
                .into(),
        );
    }
    Report {
        now,
        today,
        fsp: FspAudit {
            keysets: lines,
            outstanding: amount(outstanding),
            issued: amount(issued_all),
            // Value in (issue) plus change out minus value used up is what
            // is left; what left the system is what was used up beyond change.
            paid: amount(spent_all - change_all),
            expired: amount(expired),
        },
        esp_reserve: reserve(&issues.kept, today),
        consistent,
        warnings,
    }
}

/// The report from the files of a data directory, without opening the mint:
/// for `fsp-mint audit`, and safe while the mint runs (nothing is written, a
/// half-written last log line is ignored). Key sets are read as the mint last
/// wrote them: one that is due to retire is still shown live until the mint
/// next runs.
pub fn from_dir(dir: &Path, now: u64) -> io::Result<Report> {
    let issues: IssueData = store::load(&dir.join(issue::FILE_NAME))?.unwrap_or_default();
    let ledger = Ledger::load(dir)?;
    let live: Vec<String> = if dir.join(crate::keysets::FILE_NAME).exists() {
        crate::keysets::Keysets::open(dir)?
            .all()
            .iter()
            .map(|k| k.id.clone())
            .collect()
    } else {
        Vec::new()
    };
    let all = crate::spend::replay_counters(dir)?;
    let mut view = SpendView {
        live: live.clone(),
        counters: HashMap::new(),
        retired: ledger.retired,
    };
    for (id, c) in all {
        if live.contains(&id) {
            view.counters.insert(id, c);
        } else {
            // A key set deleted but not yet copied to the ledger: its log is
            // still on disk, so it is counted from there.
            view.retired.entry(id).or_insert(c);
        }
    }
    Ok(build(now, &view, &issues))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psp_text_is_exact_for_large_and_negative_values() {
        assert_eq!(psp_text(0), "0.00000000000");
        assert_eq!(psp_text(1), "0.00000000001");
        assert_eq!(psp_text(ESP_PER_PSP as i128), "1.00000000000");
        assert_eq!(
            psp_text(i128::from(u64::MAX) * 1_000_000),
            "184467440737095.51615000000"
        );
        assert_eq!(psp_text(-150_000_000_000), "-1.50000000000");
    }

    #[test]
    fn num_crosses_json_as_text_and_reads_numbers() {
        let n = Num(i128::MAX);
        let text = serde_json::to_string(&n).unwrap();
        assert!(text.starts_with('"'));
        assert_eq!(serde_json::from_str::<Num>(&text).unwrap(), n);
        assert_eq!(serde_json::from_str::<Num>("42").unwrap(), Num(42));
    }

    fn kept(amount: u64, day: u64, exported: bool) -> KeptEsp {
        let sk = secp256k1::SecretKey::from_slice(&[1u8; 32]).unwrap();
        KeptEsp {
            amount,
            note: {
                let pk = sk.public_key(secp256k1::SECP256K1);
                mint_types::Note {
                    nonce: mint_types::Nonce(pk),
                    signature: bdhke::unblind_message(&pk, &sk, &pk),
                }
            },
            spend_secret: sk,
            redeemable_after_day: day,
            keyset: String::new(),
            request: String::new(),
            exported,
        }
    }

    #[test]
    fn the_reserve_splits_at_the_start_of_the_redeemable_day() {
        let list = [kept(5, 10, false), kept(7, 11, true), kept(11, 12, false)];
        let r = reserve(&list, 11);
        assert_eq!((r.matured.notes, r.matured.esp.units.0), (2, 12));
        assert_eq!(r.matured.exported_esp.units.0, 7);
        assert_eq!((r.unmatured.notes, r.unmatured.esp.units.0), (1, 11));
        assert_eq!(r.total.esp.units.0, 23);
        assert_eq!(
            r.by_day.iter().map(|d| d.matured).collect::<Vec<_>>(),
            [true, true, false]
        );
    }

    #[test]
    fn totals_do_not_overflow_on_large_amounts() {
        let big = 1u64 << 63;
        let list = [
            kept(big, 1, false),
            kept(big, 1, false),
            kept(big, 99, false),
        ];
        let r = reserve(&list, 50);
        assert_eq!(r.matured.esp.units.0, 1i128 << 64);
        assert_eq!(r.total.esp.units.0, 3 * (1i128 << 63));
    }
}
