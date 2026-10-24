//! `fsp-mint export-esp`: write the ESP this mint has kept to hand-over files.
//!
//! One file for each redeemable-after day, `esp-<day>-<now>.json`, for the
//! service's ecash-wallet to `receive` and `redeem`. The file is
//! [`sp_wire::esp::handover::Handover`], the type the wallet reads it with: the
//! service, the day, and each note with its amount, spend secret and the ESP
//! keyset that signed it. **Whoever holds the file holds the notes** (the notes
//! come with their spend secrets): the files are made readable by their owner
//! only (0600), in a folder only the owner can enter (0700).
//!
//! **It works while `serve` runs.** `serve` holds the data folder's writer lock
//! ([`crate::lock`]) and keeps `issues.json` in memory behind its issues Mutex, so
//! a second process must never write that file. When a `serve` is running,
//! `export-esp` asks it, on its loopback route `POST /export-esp`
//! ([`crate::server::export_outcome`]), and `serve` does the export under the
//! same Mutex that an issue holds: an issue and an export never run at the same
//! moment, so a note kept by an issue is exported by the next export or by this
//! one, never missed and never written twice. When no `serve` is running,
//! `export-esp` takes the lock itself, opens `issues.json` and does the same,
//! unless it is given `--serve-only`: then it only asks, and with no `serve`
//! answering it fails having taken no lock and opened no data file, so a caller
//! running as another account (aso's redeem job, as root) never leaves a file in
//! the data folder that `serve` cannot write.
//!
//! **Where the files are made.** Always in [`folder`], `export/` in the data
//! folder, never in a place the caller names: the route takes no path, so a
//! process that can reach the loopback port cannot make `serve` write anywhere.
//! The command then moves the files to `--out DIR` ([`deliver`]). Files left in
//! `export/` (the command died before it moved them) are moved by the next
//! `export-esp`, so a note is never stranded.
//!
//! **Order of the writes**, so that a crash loses nothing and repeats nothing:
//! the files are written under the name `<name>.part` and synced; the notes are
//! then marked `exported` in `issues.json` (one atomic save); only then are the
//! `.part` files renamed to their final names. The next export first settles any
//! `.part` file a crash left: one whose notes are all marked exported is renamed
//! (the save had happened), one whose notes are not is deleted (it had not).
//!
//! A kept note with no keyset (kept before keysets existed) cannot be written:
//! the hand-over file requires one, and the wallet refuses a file without it.
//! The export stops before it writes anything and names each such note.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sp_wire::ServiceDid;
use sp_wire::esp::handover::{HandedNote, Handover};

use crate::issue::{Issues, KeptEsp};
use crate::store;

/// The folder, in the data folder, where hand-over files are made.
pub const FOLDER: &str = "export";

/// The suffix of a file that is written but whose notes are not yet marked.
const PART: &str = ".part";

/// Where `data_dir`'s hand-over files are made.
pub fn folder(data_dir: &Path) -> PathBuf {
    data_dir.join(FOLDER)
}

/// Write every kept ESP note that has not been exported, for `service`, as
/// hand-over files in `folder` (made if it is missing), and mark the notes
/// exported in `issues` and in `issues.json`. `now` (Unix seconds) is part of
/// each file's name. Returns each file written with its number of notes, by day;
/// nothing to export is an empty list.
///
/// The caller must be the only writer of `issues.json`: `serve`, with `issues`
/// locked, or an `export-esp` that holds the data folder's lock. If the save
/// fails, the marks are taken back and the files removed.
pub fn export_kept(
    issues: &mut Issues,
    folder: &Path,
    service: ServiceDid,
    now: u64,
) -> Result<Vec<(PathBuf, usize)>, String> {
    store::create_private_dir(folder).map_err(|e| e.to_string())?;
    settle_parts(issues, folder)?;
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
    // 1. The files, under their `.part` names.
    let mut parts: Vec<(PathBuf, PathBuf, usize)> = Vec::new();
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
        let file = unused_name(folder, *day, now);
        let part = part_of(&file);
        if let Err(e) = store::save_private(&part, &handover) {
            remove_all(parts.iter().map(|(p, _, _)| p));
            let _ = fs::remove_file(&part);
            return Err(format!(
                "cannot write {}: {e}. Nothing was exported",
                part.display()
            ));
        }
        parts.push((part, file, idx.len()));
    }
    // 2. The marks, in one save.
    for idx in by_day.values() {
        for &i in idx {
            issues.data.kept[i].exported = true;
        }
    }
    if let Err(e) = issues.save() {
        for idx in by_day.values() {
            for &i in idx {
                issues.data.kept[i].exported = false;
            }
        }
        remove_all(parts.iter().map(|(p, _, _)| p));
        return Err(format!(
            "cannot save {}: {e}. Nothing was exported",
            crate::issue::FILE_NAME
        ));
    }
    // 3. The final names.
    let mut written = Vec::new();
    for (part, file, notes) in parts {
        fs::rename(&part, &file).map_err(|e| {
            format!(
                "the notes are marked exported, but {} could not be renamed to {}: {e}. The next export-esp renames it",
                part.display(),
                file.display()
            )
        })?;
        written.push((file, notes));
    }
    store::sync_dir(folder);
    Ok(written)
}

/// Settle the `.part` files a crash left in `folder`: rename one whose notes
/// `issues` already marks exported, delete one whose notes it does not. A file
/// that is neither (it names a note this mint does not keep, or notes of both
/// kinds) is an error and is left alone.
fn settle_parts(issues: &Issues, folder: &Path) -> Result<(), String> {
    let entries = fs::read_dir(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    for entry in entries.flatten() {
        let part = entry.path();
        if !part
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("esp-") && n.ends_with(PART))
        {
            continue;
        }
        let bytes = fs::read(&part).map_err(|e| format!("{}: {e}", part.display()))?;
        let handover = Handover::read(&bytes)
            .map_err(|e| format!("{} is not a hand-over file: {e:?}", part.display()))?;
        let states: Vec<Option<bool>> = handover
            .notes
            .iter()
            .map(|n| {
                issues
                    .data
                    .kept
                    .iter()
                    .find(|k| k.note.nonce == n.note.nonce)
                    .map(|k| k.exported)
            })
            .collect();
        if states.iter().all(|s| *s == Some(true)) {
            let name = part.with_extension("");
            let final_name = if name.exists() {
                unused_name_like(&name)
            } else {
                name
            };
            fs::rename(&part, &final_name).map_err(|e| format!("{}: {e}", part.display()))?;
        } else if states.iter().all(|s| *s == Some(false)) {
            fs::remove_file(&part).map_err(|e| format!("{}: {e}", part.display()))?;
        } else {
            return Err(format!(
                "{} holds notes that this mint's records do not agree with (some exported, some not, or not kept here). It was left as it is: look at it before exporting again",
                part.display()
            ));
        }
    }
    Ok(())
}

/// The final name for a file of `day` at `now`: `esp-<day>-<now>.json`, or with
/// `-2`, `-3`, ... before the extension if that name (or its `.part`) is taken,
/// as it is when two exports run in the same second.
fn unused_name(folder: &Path, day: u64, now: u64) -> PathBuf {
    unused_name_like(&folder.join(format!("esp-{day}-{now}.json")))
}

fn unused_name_like(first: &Path) -> PathBuf {
    let taken = |p: &Path| p.exists() || part_of(p).exists();
    if !taken(first) {
        return first.to_path_buf();
    }
    let stem = first
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("esp")
        .to_string();
    (2u64..)
        .map(|n| first.with_file_name(format!("{stem}-{n}.json")))
        .find(|p| !taken(p))
        .expect("an unused name")
}

fn part_of(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_os_string();
    name.push(PART);
    PathBuf::from(name)
}

fn remove_all<'a>(paths: impl Iterator<Item = &'a PathBuf>) {
    for p in paths {
        let _ = fs::remove_file(p);
    }
}

/// Move every finished hand-over file in `from` (the export folder) to `out`
/// (made, 0700, if it is missing). A file whose name is already in `out` is not
/// overwritten: it stays in `from` and the move stops with an error. Returns each
/// file's new path with its number of notes.
pub fn deliver(from: &Path, out: &Path) -> Result<Vec<(PathBuf, usize)>, String> {
    let mut names: Vec<PathBuf> = match fs::read_dir(from) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("esp-") && n.ends_with(".json"))
            })
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("{}: {e}", from.display())),
    };
    names.sort();
    if names.is_empty() {
        return Ok(Vec::new());
    }
    store::create_private_dir(out).map_err(|e| format!("cannot make {}: {e}", out.display()))?;
    let mut moved = Vec::new();
    for file in names {
        let name = file.file_name().expect("a file name");
        let target = out.join(name);
        if target.exists() {
            return Err(format!(
                "{} already exists, so {} was not moved there. It is still in {}",
                target.display(),
                file.display(),
                from.display()
            ));
        }
        let bytes = fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let notes = Handover::read(&bytes)
            .map_err(|e| format!("{} is not a hand-over file: {e:?}", file.display()))?
            .notes
            .len();
        // A rename keeps the file's mode; across file systems it is copied with
        // the same owner-only mode and then removed.
        if fs::rename(&file, &target).is_err() {
            copy_private(&bytes, &target)?;
            fs::remove_file(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        }
        moved.push((target, notes));
    }
    store::sync_dir(out);
    Ok(moved)
}

fn copy_private(bytes: &[u8], target: &Path) -> Result<(), String> {
    use std::io::Write;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(target)
        .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    f.write_all(bytes)
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("cannot write {}: {e}", target.display()))
}

/// Ask a running `serve` on `bind` (`server.bind`, a loopback `IP:port`) to do
/// the export (`POST /export-esp`), and return each file it wrote in its export
/// folder, by name, with its number of notes. A refusal from `serve` is the
/// error, with its reason; nothing answering on `bind` is the error
/// `no_answer(error)` makes, which says what that means for the caller.
///
/// A few lines over `std::net`, not an HTTP client: the request has no body and
/// the answer is short JSON, and a client crate would be a dependency for this
/// one call.
pub fn ask_serve(
    bind: &str,
    no_answer: &dyn Fn(&std::io::Error) -> String,
) -> Result<Vec<(String, usize)>, String> {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    let addr: SocketAddr = bind
        .parse()
        .map_err(|_| format!("server.bind `{bind}` is not an IP:port"))?;
    if !addr.ip().is_loopback() {
        return Err(format!("server.bind `{bind}` is not a loopback address"));
    }
    let mut stream =
        TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| no_answer(&e))?;
    // An export waits for an issue in progress, which waits for the ESP mint.
    let wait = Some(Duration::from_secs(600));
    let _ = stream.set_read_timeout(wait);
    let _ = stream.set_write_timeout(wait);
    stream
        .write_all(
            format!(
                "POST /export-esp HTTP/1.1\r\nHost: {bind}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .map_err(|e| format!("cannot ask fsp-mint serve at {bind}: {e}"))?;
    let mut reply = Vec::new();
    stream
        .read_to_end(&mut reply)
        .map_err(|e| format!("no answer from fsp-mint serve at {bind}: {e}. The export may or may not have happened: run export-esp again, it moves what was written"))?;
    let text = String::from_utf8_lossy(&reply);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_ref(), ""));
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("{bind} did not answer like fsp-mint serve: {head}"))?;
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| format!("{bind} did not answer like fsp-mint serve: {head} {body}"))?;
    if status != 200 {
        return Err(format!(
            "fsp-mint serve refused the export ({status}): {}",
            json["error"].as_str().unwrap_or(body)
        ));
    }
    Ok(json["files"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .map(|f| {
                    (
                        f["name"].as_str().unwrap_or_default().to_string(),
                        f["notes"].as_u64().unwrap_or(0) as usize,
                    )
                })
                .collect()
        })
        .unwrap_or_default())
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
