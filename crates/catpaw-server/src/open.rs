//! Opening a local page in the user's browser, so that the user need not
//! copy its address from the agent's reply or handle the approval key
//! (the address carries a pass: see [`crate::local`]).

use std::fmt;
use std::process::{Command, Stdio};
use std::sync::Arc;

/// What opens an address.
type Open = dyn Fn(&str) -> std::io::Result<()> + Send + Sync;

/// Opens an address in the user's browser.
#[derive(Clone)]
pub struct Opener(Arc<Open>);

impl Opener {
    /// An opener that calls `open` (a test sees what would be opened).
    pub fn new(open: impl Fn(&str) -> std::io::Result<()> + Send + Sync + 'static) -> Self {
        Self(Arc::new(open))
    }

    /// The user's default browser.
    pub fn browser() -> Self {
        Self::new(open_in_browser)
    }

    pub(crate) fn open(&self, url: &str) -> std::io::Result<()> {
        (self.0)(url)
    }
}

impl fmt::Debug for Opener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Opener")
    }
}

/// Opens `url`, a page of the local server, in the default browser.
/// Nothing of the launcher reaches this process's standard streams, which
/// may carry the MCP session.
fn open_in_browser(url: &str) -> std::io::Result<()> {
    if !url.starts_with("http://127.0.0.1:") {
        return Err(std::io::Error::other("only the local pages are opened"));
    }
    let mut child = launcher(url)?
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // The launcher hands the address on and exits; it is waited for so
    // that it does not linger.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(windows)]
fn launcher(url: &str) -> std::io::Result<Command> {
    // No shell: the address is one argument, whatever it holds.
    let mut command = Command::new("rundll32.exe");
    command.args(["url.dll,FileProtocolHandler", url]);
    Ok(command)
}

#[cfg(target_os = "macos")]
fn launcher(url: &str) -> std::io::Result<Command> {
    let mut command = Command::new("open");
    command.arg(url);
    Ok(command)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn launcher(url: &str) -> std::io::Result<Command> {
    let desktop = ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()));
    if !desktop {
        return Err(std::io::Error::other("no desktop to open a browser on"));
    }
    let mut command = Command::new("xdg-open");
    command.arg(url);
    Ok(command)
}

#[cfg(not(any(windows, unix)))]
fn launcher(_: &str) -> std::io::Result<Command> {
    Err(std::io::Error::other("no browser to open on this system"))
}
