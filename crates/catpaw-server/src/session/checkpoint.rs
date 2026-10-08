//! Checkpoints: the `session` tool's saved cookies, storage and tabs.

use serde::{Deserialize, Serialize};

use super::*;

/// The longest checkpoint name, in bytes.
const MAX_NAME: usize = 64;

/// A saved session (`session` tool): cookies, storage, and the tabs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Checkpoint {
    /// The cookie jar, as the jar writes it.
    cookies: String,
    /// `localStorage` by origin.
    storage: BTreeMap<String, Vec<(String, String)>>,
    /// The tabs, the current one first.
    tabs: Vec<SavedTab>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedTab {
    url: String,
    scroll: (f32, f32),
}

impl Checkpoint {
    /// Checks what restoring relies on, before anything is torn down.
    fn check(&self) -> Result<(), String> {
        catpaw_net::cookies::CookieJar::new()
            .load_json(&self.cookies)
            .map_err(|e| format!("its cookies: {e}"))?;
        for tab in &self.tabs {
            url::Url::parse(&tab.url).map_err(|e| format!("tab {}: {e}", quote(&tab.url)))?;
        }
        Ok(())
    }
}

/// The name a call gives, checked.
fn checked_name(name: Option<&str>) -> Result<String, Failure> {
    let name = name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| Failure::bad_argument("save and restore need a name"))?;
    if name.len() > MAX_NAME {
        return Err(Failure::bad_argument(format!(
            "a checkpoint name is at most {MAX_NAME} bytes"
        )));
    }
    Ok(name.to_string())
}

/// A checkpoint's file name: its name with every byte but letters, digits,
/// `-` and `_` written `%XX`, so that names neither collide nor leave the
/// directory.
fn file_stem(name: &str) -> String {
    name.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// The name a checkpoint file stands for.
fn name_of(stem: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut rest = stem.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        if b == b'%' {
            let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(b);
            rest = tail;
        }
    }
    String::from_utf8(bytes).ok()
}

impl Session {
    /// The `session` tool: checkpoints of cookies, storage and tabs.
    pub(super) fn session_tool(&mut self, p: params::Session) -> CallResult {
        let file = |profile: &Profile, name: &str| {
            profile
                .checkpoints_dir()
                .join(format!("{}.json", file_stem(name)))
        };
        match p.op {
            SessionOp::Save => {
                let name = checked_name(p.name.as_deref())?;
                let mut tabs = Vec::new();
                let mut order: Vec<u32> = self.routes.keys().copied().collect();
                order.sort_by_key(|t| (Some(*t) != self.current, *t));
                for tab in order {
                    if let Some((url, scroll)) = self.place_of(tab) {
                        tabs.push(SavedTab { url, scroll });
                    }
                }
                let checkpoint = Checkpoint {
                    cookies: self.cookies().to_json(),
                    storage: self.storage().into_iter().collect(),
                    tabs,
                };
                if let Some(profile) = &self.profile {
                    let unsaved = |e: String| {
                        Failure::new(
                            ErrorCode::Unsupported,
                            format!("writing the checkpoint: {e}"),
                        )
                    };
                    let text = serde_json::to_string_pretty(&checkpoint)
                        .map_err(|e| unsaved(e.to_string()))?;
                    crate::profile::write_whole(&file(profile, &name), &text)
                        .map_err(|e| unsaved(e.to_string()))?;
                }
                let count = checkpoint.tabs.len();
                self.checkpoints.insert(name.clone(), checkpoint);
                Ok(ToolOutput::ok(format!(
                    "ok session save {} ({count} tab{})",
                    quote(&name),
                    if count == 1 { "" } else { "s" }
                )))
            }
            SessionOp::List => {
                let mut names: Vec<String> = self.checkpoints.keys().cloned().collect();
                if let Some(profile) = &self.profile
                    && let Ok(entries) = std::fs::read_dir(profile.checkpoints_dir())
                {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().is_some_and(|e| e == "json")
                            && let Some(name) =
                                path.file_stem().and_then(|s| s.to_str()).and_then(name_of)
                        {
                            names.push(name);
                        }
                    }
                }
                names.sort();
                names.dedup();
                let mut text = "ok session list".to_string();
                if names.is_empty() {
                    text.push_str("\n(nothing saved)");
                }
                for name in names {
                    text.push('\n');
                    text.push_str(&quote(&name));
                }
                Ok(ToolOutput::ok(text))
            }
            SessionOp::Restore => {
                let name = checked_name(p.name.as_deref())?;
                let checkpoint = match self.checkpoints.get(&name) {
                    Some(c) => c.clone(),
                    None => self.load_checkpoint(&name, file)?,
                };
                checkpoint.check().map_err(|e| {
                    Failure::new(
                        ErrorCode::Unsupported,
                        format!("{} cannot be restored: {e}", quote(&name)),
                    )
                })?;
                let groups: Vec<u32> = self.groups.keys().copied().collect();
                for group in groups {
                    self.drop_group(group);
                }
                self.openers.clear();
                self.current = None;
                let jar = self.net.client().cookies();
                jar.clear();
                jar.load_json(&checkpoint.cookies)
                    .map_err(|e| Failure::new(ErrorCode::Unsupported, e))?;
                self.options.storage = checkpoint.storage.clone().into_iter().collect();
                let mut opened = Vec::new();
                for saved in &checkpoint.tabs {
                    let tab = self.open_group()?;
                    let p = params::Navigate {
                        url: Some(saved.url.clone()),
                        go: None,
                        snapshot: Some(params::SnapshotMode::None),
                        confirmation: None,
                    };
                    let (x, y) = saved.scroll;
                    let loaded = self.on_tab(tab, move |g, tab, view| {
                        let result = g.navigate(tab, p, view);
                        g.scroll_to(tab, x, y);
                        result
                    });
                    let mut line = format!("t{tab} {}", truncate(&saved.url, 120));
                    if let Err(failure) = loaded {
                        line.push_str(&format!(" (not loaded: {})", failure.code.as_str()));
                    }
                    opened.push(line);
                }
                let first = self.routes.keys().next().copied();
                self.current = first;
                let mut text =
                    format!("ok session restore {}: {}", quote(&name), opened.join(", "));
                if let Some(tab) = first {
                    let snapshot = self.on_tab(tab, move |g, tab, view| {
                        g.snapshot_text(tab, &SnapRequest::default(), view)
                            .map(ToolOutput::ok)
                    })?;
                    text.push('\n');
                    text.push_str(&snapshot.text);
                }
                Ok(ToolOutput::ok(text))
            }
        }
    }

    /// A checkpoint the profile keeps.
    fn load_checkpoint(
        &self,
        name: &str,
        file: impl Fn(&Profile, &str) -> PathBuf,
    ) -> Result<Checkpoint, Failure> {
        let missing = || {
            Failure::new(
                ErrorCode::NotFound,
                format!("nothing is saved as {}", quote(name)),
            )
        };
        let profile = self.profile.as_ref().ok_or_else(missing)?;
        let text = match std::fs::read_to_string(file(profile, name)) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(missing()),
            Err(e) => {
                return Err(Failure::new(
                    ErrorCode::Unsupported,
                    format!("cannot read checkpoint {}: {e}", quote(name)),
                ));
            }
        };
        serde_json::from_str(&text).map_err(|e| {
            Failure::new(
                ErrorCode::Unsupported,
                format!("checkpoint {} is damaged: {e}", quote(name)),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_make_distinct_files_inside_the_directory() {
        assert_eq!(file_stem("before-login_2"), "before-login_2");
        assert_eq!(file_stem("a/b"), "a%2Fb");
        assert_eq!(file_stem("a_b"), "a_b");
        assert_eq!(file_stem(".."), "%2E%2E");
        for name in ["before login", "a/b", "..", "购物车", "100%"] {
            assert_eq!(name_of(&file_stem(name)).as_deref(), Some(name));
        }
        assert!(name_of("%G1").is_none());
    }
}
