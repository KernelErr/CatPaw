//! The `settings` tool: showing the user's settings, and changing them
//! once the user approved (see `ask_first`).

use std::net::IpAddr;
use std::path::PathBuf;

use super::*;
use crate::settings::{parse_download_dir, parse_local_address};

/// A change asked for, checked.
pub(super) struct Change {
    download_dir: Option<PathBuf>,
    local_address: Option<IpAddr>,
}

/// Checks the change a call asks for: a bad value fails the call before
/// the user is asked anything.
pub(super) fn parse_change(change: &params::SettingsChange) -> Result<Change, Failure> {
    let download_dir = change
        .download_dir
        .as_deref()
        .map(parse_download_dir)
        .transpose()
        .map_err(Failure::bad_argument)?;
    let local_address = change
        .local_address
        .as_deref()
        .map(parse_local_address)
        .transpose()
        .map_err(Failure::bad_argument)?;
    if download_dir.is_none() && local_address.is_none() {
        return Err(Failure::bad_argument(
            "set names nothing to change: downloadDir, localAddress",
        ));
    }
    Ok(Change {
        download_dir,
        local_address,
    })
}

/// The question the user is asked about a change.
pub(super) fn describe_change(change: &Change) -> String {
    let mut parts = Vec::new();
    if let Some(dir) = &change.download_dir {
        parts.push(format!("save downloads in {}", dir.display()));
    }
    if let Some(address) = change.local_address {
        if address.is_loopback() {
            parts.push(format!(
                "serve the hand-off and approval pages on {address}, this machine only"
            ));
        } else {
            parts.push(format!(
                "serve the hand-off and approval pages on {address}, where other machines on that network can reach them (the approval key still guards them, over plain HTTP)"
            ));
        }
    }
    format!("change CatPaw's settings: {}", parts.join("; "))
}

impl Session {
    /// `settings`: the settings in effect, or a change made (the user
    /// approved it, unless the policy asks nothing).
    pub(super) fn settings_tool(&mut self, p: params::Settings) -> CallResult {
        let Some(change) = p.set else {
            return Ok(ToolOutput::ok(format!(
                "ok settings\n{}",
                self.settings_lines()
            )));
        };
        let change = parse_change(&change)?;
        let before = self.settings();
        let mut saved = self.saved_settings.clone();
        if let Some(dir) = change.download_dir.clone() {
            saved.download_dir = Some(dir);
        }
        if let Some(address) = change.local_address {
            saved.local_address = Some(address);
        }
        let stored = match &self.settings_file {
            Some(file) => {
                saved.save(file).map_err(|e| {
                    Failure::new(
                        ErrorCode::Unsupported,
                        format!("could not save the settings in {}: {e}", file.display()),
                    )
                })?;
                format!("saved in {}", file.display())
            }
            None => "kept for this session (there is no settings file)".to_string(),
        };
        self.saved_settings = saved;
        self.journal(
            "settings",
            json!({
                "downloadDir": change.download_dir,
                "localAddress": change.local_address.map(|a| a.to_string()),
            }),
        );
        let after = self.settings();
        self.router
            .net
            .client()
            .set_download_dir(self.download_dir());
        let mut text = format!("ok settings\n{stored}\n{}", self.settings_lines());
        if before.local_address() != after.local_address() {
            // The pages move: the next one needed starts on the new address.
            self.local = None;
            text.push_str("\n! the hand-off and approval pages move to the new address; links given before no longer work");
        }
        let pinned = [
            (
                change.download_dir.is_some() && self.pinned_settings.download_dir.is_some(),
                "downloadDir",
            ),
            (
                change.local_address.is_some() && self.pinned_settings.local_address.is_some(),
                "localAddress",
            ),
        ];
        for (pinned, name) in pinned {
            if pinned {
                text.push_str(&format!(
                    "\n! the command line sets {name} for this session: the new value applies from the next"
                ));
            }
        }
        Ok(ToolOutput::ok(text))
    }

    /// One line per setting, with where its value comes from.
    fn settings_lines(&self) -> String {
        let source = |pinned: bool, saved: bool| {
            if pinned {
                "command line"
            } else if saved {
                "settings file"
            } else {
                "default"
            }
        };
        let settings = self.settings();
        let download = self
            .download_dir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| "none: downloads are kept in memory".to_string());
        let address = settings.local_address();
        let mut lines = vec![
            format!(
                "downloadDir: {download} ({})",
                source(
                    self.pinned_settings.download_dir.is_some(),
                    self.saved_settings.download_dir.is_some()
                )
            ),
            format!(
                "localAddress: {address} ({}){}",
                source(
                    self.pinned_settings.local_address.is_some(),
                    self.saved_settings.local_address.is_some()
                ),
                if address.is_loopback() {
                    ""
                } else {
                    "; the local pages are reachable from that network"
                }
            ),
        ];
        lines.push(match &self.settings_file {
            Some(file) => format!("file: {}", file.display()),
            None => "file: none (changes last for this session)".to_string(),
        });
        lines.join("\n")
    }
}
