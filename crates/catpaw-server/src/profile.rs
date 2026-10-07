//! A profile: a directory that keeps a context between sessions —
//! `cookies.json` (the cookie jar), `storage.json` (`localStorage` by
//! origin), `checkpoints/` (saved sessions) and `journal/` (the flight
//! recorder's journals).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

/// Writes `text` to `path` through a file beside it, so that a crash
/// never leaves half a file.
pub(crate) fn write_whole(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension("partial");
    std::fs::write(&partial, text)?;
    std::fs::rename(&partial, path)
}

#[derive(Clone, Debug)]
pub struct Profile {
    dir: PathBuf,
}

impl Profile {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn journal_dir(&self) -> PathBuf {
        self.dir.join("journal")
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
