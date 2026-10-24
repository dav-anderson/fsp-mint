//! The data folder's writer lock.
//!
//! One fsp-mint process at a time writes a network's data folder (`issues.json`,
//! `keysets.json`, the spent logs, `air_mailbox_next`): `serve` holds this lock
//! from before it opens any of them until it exits, and `export-esp` takes it
//! when it has to do the export itself because no `serve` is running. A second
//! writer is refused with a message that says which folder and why.
//!
//! The lock is an advisory `flock` on the file `fsp-mint.lock` in the data
//! folder ([`std::fs::File::try_lock`]; the same call on macOS and Linux). The
//! operating system drops it when the process ends, however it ends (Ctrl-C,
//! `kill -9`, a crash, a power cut), so a lock file left behind by a dead process
//! never blocks the next start: nothing has to be deleted by hand. The file
//! itself stays; only the lock on it matters. `audit` writes nothing and does not
//! take the lock.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// The lock file's name, in the data folder.
pub const FILE_NAME: &str = "fsp-mint.lock";

/// Why the lock was not taken.
#[derive(Debug)]
pub enum LockError {
    /// Another process holds it: a `serve` (or an `export-esp` doing its own
    /// export) is running on this data folder.
    Held(PathBuf),
    /// The folder or the file could not be made or opened.
    Io(PathBuf, io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Held(path) => write!(
                f,
                "another fsp-mint is already running on this data folder ({} is locked). Only one process may write it at a time, so a second `serve` is refused. Stop the other one, or use a different FSP_DATA_DIR or server.network",
                path.display()
            ),
            LockError::Io(path, e) => write!(f, "cannot take the lock {}: {e}", path.display()),
        }
    }
}

impl std::error::Error for LockError {}

/// The held lock. It is released when this is dropped or the process ends.
#[derive(Debug)]
pub struct DataLock {
    _file: File,
}

impl DataLock {
    /// Take the lock on `data_dir` (made if it is missing), or fail at once if
    /// another process holds it. It never waits.
    pub fn acquire(data_dir: &Path) -> Result<DataLock, LockError> {
        let path = data_dir.join(FILE_NAME);
        fs::create_dir_all(data_dir).map_err(|e| LockError::Io(path.clone(), e))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| LockError::Io(path.clone(), e))?;
        match file.try_lock() {
            Ok(()) => Ok(DataLock { _file: file }),
            Err(TryLockError::WouldBlock) => Err(LockError::Held(path)),
            Err(TryLockError::Error(e)) => Err(LockError::Io(path, e)),
        }
    }
}
