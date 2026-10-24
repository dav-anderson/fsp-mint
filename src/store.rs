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
    if let Some(dir) = path.parent()
        && let Ok(d) = File::open(dir)
    {
        let _ = d.sync_all();
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

/// A new secret file readable only by its owner (0600 on Unix). It fails with
/// `AlreadyExists` if the file exists, as `create_new` does, and it appears
/// whole or not at all: the text is written and synced in a temporary file of
/// this process, which is then linked to `path` (a link, unlike a rename, never
/// replaces a file) and the folder synced. A crash leaves no empty or
/// half-written `path`; at most a `<name>.<pid>.tmp` beside it.
pub fn write_secret(path: &Path, text: &str) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{} is not a file name", path.display())))?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let written = (|| {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        fs::hard_link(&tmp, path)
    })();
    // The temporary name is this process's own; it goes whether the link was
    // made or not.
    let _ = fs::remove_file(&tmp);
    written?;
    if let Some(dir) = path.parent() {
        sync_dir(dir);
    }
    Ok(())
}

/// A new file readable only by its owner (0600 on Unix), holding `value` as
/// JSON, written and synced. It fails if the file exists. For files that hold
/// spend secrets (the ESP hand-over files).
pub fn save_private<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let text = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(&text)?;
    f.sync_all()
}

/// Make a folder (and its parents) that only its owner can enter (0700 on
/// Unix). A folder that already exists is set to 0700 too, so files of spend
/// secrets never sit in a folder others can enter.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// Sync a folder, so that a rename in it is on disk.
pub fn sync_dir(path: &Path) {
    if let Ok(d) = File::open(path) {
        let _ = d.sync_all();
    }
}
