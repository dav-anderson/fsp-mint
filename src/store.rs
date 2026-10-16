//! Small JSON files written in full on every change: a temporary file, fsync,
//! rename, then fsync of the directory. A reader sees the old file or the new
//! one, never half of either.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

pub fn save<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let text = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    let tmp = path.with_extension("tmp");
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&text)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Read a file, or `None` if it does not exist. A damaged file is an error and
/// is never replaced.
pub fn load<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| io::Error::other(format!("{} is damaged: {e}", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Write a secret file readable only by its owner (0600 on Unix).
pub fn write_secret(path: &Path, text: &str) -> io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()
}
