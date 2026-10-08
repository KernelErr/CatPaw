//! A profile: a directory that keeps a context between sessions —
//! `cookies.json` (the cookie jar), `storage.json` (`localStorage` by
//! origin), `checkpoints/` (saved sessions) and `journal/` (the flight
//! recorder's journals).

use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

/// `localStorage` by origin.
pub type Storage = HashMap<String, Vec<(String, String)>>;

/// Reads storage as `--storage` files hold it: an object of origins, each
/// an object of keys and values.
pub fn parse_storage(text: &str) -> Result<Storage, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let origins = value.as_object().ok_or("not a JSON object")?;
    Ok(origins
        .iter()
        .filter_map(|(origin, items)| {
            let items = items
                .as_object()?
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect();
            Some((origin.clone(), items))
        })
        .collect())
}

/// Storage as [`parse_storage`] reads it, origins in order.
pub fn storage_json(storage: &Storage) -> String {
    let mut origins: Vec<_> = storage.iter().collect();
    origins.sort();
    let mut out = serde_json::Map::new();
    for (origin, items) in origins {
        let object = items
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        out.insert(origin.clone(), Value::Object(object));
    }
    serde_json::to_string_pretty(&Value::Object(out)).unwrap_or_default()
}

/// Creates (or empties) a file only its owner can read: cookies,
/// storage, keys and journals are as sensitive as the sessions they hold.
pub(crate) fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Creates a directory only its owner can enter; `AlreadyExists` when it
/// is there.
pub(crate) fn private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Where a profile in `dir` keeps its journals.
pub fn journal_dir(dir: &Path) -> PathBuf {
    dir.join("journal")
}

/// Writes `text` to `path` through a file beside it, so that a crash
/// never leaves half a file; only its owner can read it.
pub(crate) fn write_whole(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension("partial");
    private_file(&partial)?.write_all(text.as_bytes())?;
    std::fs::rename(&partial, path)
}

#[derive(Clone, Debug)]
pub struct Profile {
    dir: PathBuf,
    /// Locked while a session uses the profile: two at once would write
    /// over each other's cookies and storage.
    _lock: Arc<File>,
}

impl Profile {
    /// Takes the profile in `dir` (made when missing) for this session;
    /// refused while another session has it.
    pub fn open(dir: PathBuf) -> Result<Self, String> {
        match private_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => std::fs::create_dir_all(&dir)
                .map_err(|e| format!("cannot make {}: {e}", dir.display()))?,
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("lock"))
            .map_err(|e| format!("cannot lock {}: {e}", dir.display()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(format!(
                    "{} is in use by another session (one profile serves one session at a time)",
                    dir.display()
                ));
            }
            Err(TryLockError::Error(e)) => {
                return Err(format!("cannot lock {}: {e}", dir.display()));
            }
        }
        Ok(Self {
            dir,
            _lock: Arc::new(lock),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn journal_dir(&self) -> PathBuf {
        journal_dir(&self.dir)
    }

    pub fn checkpoints_dir(&self) -> PathBuf {
        self.dir.join("checkpoints")
    }

    /// The saved cookie jar (JSON) and storage; nothing for a new profile.
    pub fn load(&self) -> Result<(Option<String>, Storage), String> {
        let cookies = match std::fs::read_to_string(self.dir.join("cookies.json")) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("reading the profile's cookies: {e}")),
        };
        let storage = match std::fs::read_to_string(self.dir.join("storage.json")) {
            Ok(text) => {
                parse_storage(&text).map_err(|e| format!("reading the profile's storage: {e}"))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Storage::new(),
            Err(e) => return Err(format!("reading the profile's storage: {e}")),
        };
        Ok((cookies, storage))
    }

    pub fn save(&self, cookies: &str, storage: &Storage) -> std::io::Result<()> {
        write_whole(&self.dir.join("cookies.json"), cookies)?;
        write_whole(&self.dir.join("storage.json"), &storage_json(storage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_round_trips() {
        let mut storage = Storage::new();
        storage.insert(
            "https://example.com".into(),
            vec![("theme".into(), "dark".into())],
        );
        let text = storage_json(&storage);
        assert_eq!(parse_storage(&text).unwrap(), storage);
    }
}
