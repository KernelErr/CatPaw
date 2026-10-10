//! `wait({for: "download"})`: until the latest download being saved in
//! the current tab's group is complete. The bytes arrive in the network's
//! own task; the session only looks at how far they are, answering the
//! host meanwhile.

use std::sync::Arc;
use std::time::Duration;

use catpaw_net::{DownloadProgress, DownloadState};

use super::*;

/// How long one wait waits by default: under the time hosts give a tool
/// call; the agent waits again for a long download.
const WAIT: Duration = Duration::from_secs(50);
/// The longest one wait may be asked to take.
const WAIT_MAX: Duration = Duration::from_secs(1800);

/// Where a download stands, for the agent: `saved to <path> (size)`,
/// `saving to <path>: 12 MB of 340 MB`, or `failed: <why>`.
pub(crate) fn download_status(saved: &DownloadProgress) -> String {
    let path = saved.path().display();
    let written = crate::files::size(saved.written());
    match saved.state() {
        DownloadState::Done => format!("saved to {path} ({written})"),
        DownloadState::Saving => match saved.total() {
            Some(total) => format!(
                "saving to {path}: {written} of {}",
                crate::files::size(total)
            ),
            None => format!("saving to {path}: {written} so far"),
        },
        DownloadState::Failed(why) => format!("failed ({why}); the partial file was removed"),
    }
}

impl Session {
    pub(super) fn wait_download(&mut self, p: params::Wait, host: &mut dyn Host) -> CallResult {
        let tab = self.router.current_tab()?;
        let group = self
            .router
            .routes
            .get(&tab)
            .and_then(|g| self.router.groups.get(g))
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        let latest: Option<(String, Option<Arc<DownloadProgress>>)> = group
            .call(|g| g.latest_download())
            .map_err(|_| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        let (name, saved) = latest
            .ok_or_else(|| Failure::bad_argument("no file was downloaded in this tab's group"))?;
        let name = catpaw_agent::snapshot::quote(&name);
        let Some(saved) = saved else {
            return Ok(ToolOutput::ok(format!(
                "ok wait download: {name} is kept in memory (read it with view download)"
            )));
        };
        let timeout = p
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(WAIT)
            .min(WAIT_MAX);
        let started = Instant::now();
        loop {
            match saved.state() {
                DownloadState::Done => {
                    return Ok(ToolOutput::ok(format!(
                        "ok wait download: {name} {}",
                        download_status(&saved)
                    )));
                }
                DownloadState::Failed(_) => {
                    return Err(Failure::new(
                        ErrorCode::Unsupported,
                        format!("download {name} {}", download_status(&saved)),
                    ));
                }
                DownloadState::Saving => {}
            }
            if started.elapsed() >= timeout {
                return Err(Failure::new(
                    ErrorCode::Timeout,
                    format!("download {name} is still {}", download_status(&saved)),
                )
                .with(advice::DOWNLOAD_WAIT));
            }
            if host.pause(Duration::from_millis(250)) {
                return Err(Failure::new(
                    ErrorCode::Timeout,
                    format!("cancelled while {name} was saving; it goes on saving"),
                ));
            }
        }
    }
}
