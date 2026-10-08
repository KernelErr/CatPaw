//! A browsing session: one context (cookies, connections, storage) and the
//! tabs open in it. Each group of tabs that share a page (a page and the
//! popups it opened) runs on a thread of its own; the session routes each
//! call to the right one.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use catpaw_agent::Format;
use catpaw_agent::snapshot::{quote, truncate};
use catpaw_engine::{EngineError, GroupHandle, PageOptions, SharedNet};
use catpaw_protocol::params::{self, ActKind, Call, SessionOp, TabsOp};
use catpaw_protocol::wording::{ErrorCode, advice, consequence, outcome};
use serde_json::{Value, json};

use crate::confirm::{ApprovalConfig, Confirmations, Stage, State};
use crate::handoff::Handoffs;
use crate::journal::{CallRecord, Journal, JournalConfig, SharedJournal};
use crate::local::{LocalServer, Pages};
use crate::output::{CallResult, Failure, ToolOutput};
use crate::policy::{Policy, Verdict};
use crate::profile::Profile;
use crate::tab::{GroupSetup, GroupState, SnapRequest, TabSummary, View, action_limits};

mod approval;
mod checkpoint;
mod handover;
mod tabs;

use approval::fingerprint;
use checkpoint::Checkpoint;
use tabs::{Group, parse_tab};

/// The shortest time between two saves of the profile after calls.
const PROFILE_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// How a session starts.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// Network, page and event-loop settings for every tab.
    pub options: PageOptions,
    /// The snapshot line format results use until a call picks another.
    pub format: Format,
    /// What needs the user's approval, and what is not allowed.
    pub policy: Policy,
    /// The approval page, for hosts that cannot ask the user themselves.
    pub approval: ApprovalConfig,
    /// Where relative paths of uploaded files start (the working
    /// directory when unset).
    pub files_root: Option<PathBuf>,
    /// The flight recorder (a profile keeps one in `journal/`).
    pub journal: Option<JournalConfig>,
    /// A directory keeping cookies, storage, checkpoints and journals
    /// between sessions.
    pub profile: Option<PathBuf>,
    /// Optional tools to offer (`session`).
    pub tools: Vec<String>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            options: PageOptions {
                limits: action_limits(),
                ..PageOptions::default()
            },
            format: Format::Compact,
            policy: Policy::default(),
            approval: ApprovalConfig::default(),
            files_root: None,
            journal: None,
            profile: None,
            tools: Vec::new(),
        }
    }
}

/// What a host answered when asked to approve something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    Approved,
    Declined,
    /// The user dismissed the question without deciding.
    Cancelled,
    /// The host cannot ask, or gave no answer in time: the approval page
    /// takes over.
    Unavailable,
}

/// The client a session serves, as far as a call needs it: asking its
/// user to approve something (MCP elicitation), and staying responsive
/// while a call waits.
pub trait Host {
    /// Asks the user to approve `message`.
    fn approve(&mut self, message: &str) -> Approval;
    /// Waits up to `wait`, answering the host meanwhile (pings); `true`
    /// when the host cancelled the call.
    fn pause(&mut self, wait: std::time::Duration) -> bool;
}

/// A host that cannot ask the user, and never cancels.
pub struct NoHost;

impl Host for NoHost {
    fn approve(&mut self, _message: &str) -> Approval {
        Approval::Unavailable
    }

    fn pause(&mut self, wait: std::time::Duration) -> bool {
        std::thread::sleep(wait);
        false
    }
}

pub struct Session {
    net: SharedNet,
    options: PageOptions,
    view: View,
    groups: BTreeMap<u32, Group>,
    next_group: u32,
    /// Tab → group.
    routes: BTreeMap<u32, u32>,
    /// Tab → the tab that opened it.
    openers: BTreeMap<u32, Option<u32>>,
    current: Option<u32>,
    next_tab: Arc<AtomicU32>,
    policy: Policy,
    setup: GroupSetup,
    confirmations: Confirmations,
    handoffs: Handoffs,
    /// The approval page and the hand-off viewer, once one was needed.
    local: Option<LocalServer>,
    /// Shared with the local pages, which record decisions as they come.
    journal: Option<SharedJournal>,
    profile: Option<Profile>,
    /// When the profile was last saved after a call.
    profile_saved: Option<Instant>,
    /// A call changed what the profile keeps since it was last saved.
    profile_dirty: bool,
    /// Whether a failure to save the profile was reported already.
    profile_failed: bool,
    checkpoints: BTreeMap<String, Checkpoint>,
    tools: Vec<String>,
}

/// Why a session could not start.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error("profile: {0}")]
    Profile(String),
    #[error("journal in {}: {source}", dir.display())]
    Journal {
        dir: PathBuf,
        source: std::io::Error,
    },
}

impl Session {
    pub fn new(config: SessionConfig) -> Result<Self, SessionError> {
        let mut options = config.options;
        let profile = config
            .profile
            .map(Profile::open)
            .transpose()
            .map_err(SessionError::Profile)?;
        if let Some(profile) = &profile {
            let (cookies, storage) = profile.load().map_err(SessionError::Profile)?;
            if cookies.is_some() {
                options.net.cookies_json = cookies;
            }
            options.storage.extend(storage);
        }
        let net = SharedNet::new(options.net.clone()).map_err(EngineError::from)?;
        let journal_config = config.journal.or_else(|| {
            profile.as_ref().map(|p| JournalConfig {
                dir: p.journal_dir(),
                screens: false,
            })
        });
        // A journal asked for is kept, or the session does not start.
        let journal = journal_config
            .map(|c| {
                let start = json!({
                    "catpaw": env!("CARGO_PKG_VERSION"),
                    "policy": config.policy.preset.as_str(),
                    "trusted": config.policy.trusted,
                    "allowedDomains": config.policy.allowed_domains,
                });
                Journal::open(&c, start).map_err(|source| SessionError::Journal {
                    dir: c.dir.clone(),
                    source,
                })
            })
            .transpose()?;
        Ok(Self {
            net,
            options,
            view: View {
                format: config.format,
                tabs: 0,
            },
            groups: BTreeMap::new(),
            next_group: 1,
            routes: BTreeMap::new(),
            openers: BTreeMap::new(),
            current: None,
            next_tab: Arc::new(AtomicU32::new(1)),
            setup: GroupSetup {
                policy: config.policy.clone(),
                files_root: config.files_root,
            },
            policy: config.policy,
            confirmations: Confirmations::new(config.approval),
            handoffs: Handoffs::new(),
            local: None,
            journal: journal.map(SharedJournal::new),
            profile,
            profile_saved: None,
            profile_dirty: false,
            profile_failed: false,
            checkpoints: BTreeMap::new(),
            tools: config.tools,
        })
    }

    /// The address of a local page (`/confirm/c1`), starting the local
    /// server when it is not running yet.
    pub(crate) fn local_url(&mut self, path: &str) -> std::io::Result<String> {
        if self.local.is_none() {
            let config = self.confirmations.config().clone();
            let key_file = crate::confirm::key_file(&config)?;
            let key = crate::confirm::load_or_make_key(&key_file)?;
            let approvals = self.confirmations.shared();
            let handoffs = self.handoffs.shared();
            let journal = self.journal.clone();
            let token = crate::local::random_hex(32)?;
            self.local = Some(LocalServer::start(config.port, |port| Pages {
                key,
                token,
                key_file,
                port,
                approvals,
                handoffs,
                journal,
            })?);
        }
        let local = self.local.as_ref().expect("started above");
        Ok(local.url(path))
    }

    /// The optional tools this session offers.
    pub fn optional_tools(&self) -> &[String] {
        &self.tools
    }

    /// Writes cookies and storage to the profile, when there is one.
    pub fn save_profile(&self) -> std::io::Result<()> {
        match &self.profile {
            Some(profile) => profile.save(&self.cookies().to_json(), &self.storage()),
            None => Ok(()),
        }
    }

    /// Writes the HAR recording, when the session records.
    pub fn save_recording(&self) -> std::io::Result<Option<usize>> {
        self.net.client().save_recording()
    }

    /// The context's cookies.
    pub fn cookies(&self) -> &catpaw_net::CookieJar {
        self.net.client().cookies()
    }

    /// `localStorage` by origin, across the open tabs (what was loaded at
    /// start included).
    pub fn storage(&self) -> std::collections::HashMap<String, Vec<(String, String)>> {
        let mut all = self.options.storage.clone();
        for group in self.groups.values() {
            if let Ok(storage) = group.call(|g| g.storage_snapshot()) {
                all.extend(storage);
            }
        }
        all
    }

    /// Runs a tool. Every outcome, failures included, is a result for the
    /// model to read.
    pub fn call_tool(&mut self, name: &str, arguments: Value) -> ToolOutput {
        self.call_tool_with(name, arguments, &mut NoHost)
    }

    /// Runs a tool for `host`: asking its user when the policy wants an
    /// approval, and answering it while the call waits.
    pub fn call_tool_with(
        &mut self,
        name: &str,
        arguments: Value,
        host: &mut dyn Host,
    ) -> ToolOutput {
        let started = Instant::now();
        let fingerprint = fingerprint(name, &arguments);
        let parsed = params::parse(name, arguments.clone());
        // Whether the call may have changed pages or the context (and so
        // the profile is saved, and the journal keeps a screen).
        let acted = parsed.as_ref().is_ok_and(Call::changes_page);
        let mut output = match parsed {
            Ok(Call::Session(_)) if !self.tools.iter().any(|t| t == "session") => {
                Failure::bad_argument("no tool is called \"session\"").render()
            }
            Ok(call) => match self.run(name, &arguments, call, &fingerprint, host) {
                Ok(output) => output,
                Err(failure) => failure.render(),
            },
            Err(message) => Failure::bad_argument(message).render(),
        };
        self.after_call(name, &arguments, &mut output, started, acted);
        output
    }

    /// Journals a call and keeps the profile up to date.
    fn after_call(
        &mut self,
        name: &str,
        arguments: &Value,
        output: &mut ToolOutput,
        started: Instant,
        acted: bool,
    ) {
        if let Some(journal) = self.journal.clone() {
            let screens = journal.lock().keeps_screens();
            let url = self
                .current
                .and_then(|tab| self.place_of(tab))
                .map(|(url, _)| url);
            let screen = match (screens && acted && !output.is_error, self.current) {
                (true, Some(tab)) => self.screen_of(tab),
                _ => None,
            };
            let mut journal = journal.lock();
            journal.record_call(CallRecord {
                tool: name,
                args: arguments,
                secret_input: output.secret_input,
                tab: self.current,
                url,
                text: &output.text,
                took: started.elapsed(),
                screen,
            });
            // Said once, where the agent (and so the user) sees it.
            if let Some(e) = journal.take_error() {
                eprintln!("catpaw: writing the journal: {e}");
                output
                    .text
                    .push_str(&format!("\n! journal: not written ({e})"));
            }
        }
        // Saved at most every few seconds while calls come (and whatever the
        // time when the session ends): saving gathers every tab's storage.
        let due = self
            .profile_saved
            .is_none_or(|at| at.elapsed() >= PROFILE_EVERY);
        if acted && self.profile.is_some() {
            self.profile_dirty = true;
        }
        if self.profile_dirty && due {
            self.profile_saved = Some(Instant::now());
            self.profile_dirty = false;
            match self.save_profile() {
                Ok(()) => self.profile_failed = false,
                // Said once, where the agent (and so the user) sees it.
                Err(e) if !self.profile_failed => {
                    self.profile_failed = true;
                    eprintln!("catpaw: saving the profile: {e}");
                    output
                        .text
                        .push_str(&format!("\n! profile: not saved ({e})"));
                }
                Err(_) => {}
            }
        }
    }

    fn journal(&mut self, kind: &str, fields: Value) {
        if let Some(journal) = &self.journal {
            journal.lock().write(kind, fields);
        }
    }

    fn dispatch(&mut self, call: Call, host: &mut dyn Host) -> CallResult {
        match call {
            Call::Navigate(p) => {
                let tab = match self.current {
                    Some(tab) => tab,
                    None => self.open_group()?,
                };
                self.on_tab(tab, move |g, tab, view| g.navigate(tab, p, view))
            }
            Call::Snapshot(p) => {
                if let Some(format) = p.format {
                    self.view.format = match format {
                        params::Format::Compact => Format::Compact,
                        params::Format::Aria => Format::Aria,
                    };
                }
                self.on_current(move |g, tab, view| g.snapshot(tab, p, view))
            }
            Call::Click(p) => self.on_current(move |g, tab, view| g.click(tab, p, view)),
            Call::Type(p) => self.on_current(move |g, tab, view| g.type_text(tab, p, view)),
            Call::Fill(p) => self.on_current(move |g, tab, view| g.fill(tab, p, view)),
            Call::Press(p) => self.on_current(move |g, tab, view| g.press(tab, p, view)),
            Call::Select(p) => self.on_current(move |g, tab, view| g.select(tab, p, view)),
            Call::Act(p) => self.on_current(move |g, tab, view| g.act(tab, p, view)),
            Call::Read(p) => self.on_current(move |g, tab, _| g.read(tab, p)),
            Call::Screenshot(p) => self.on_current(move |g, tab, _| g.screenshot(tab, p)),
            Call::Evaluate(p) => {
                self.on_current(move |g, tab, _| g.evaluate(tab, p, &action_limits()))
            }
            Call::Tabs(p) => self.tabs(p),
            Call::Wait(p) if p.until == params::WaitFor::Handoff => self.wait_handoff(p, host),
            Call::Wait(p) => self.on_current(move |g, tab, view| g.wait(tab, p, view)),
            Call::Handoff(p) => self.handoff(p),
            Call::Logs(p) => self.on_current(move |g, tab, _| g.logs(tab, p)),
            Call::Session(p) => self.session_tool(p),
        }
    }
}

impl Drop for Session {
    /// What a call changed since the profile was last saved is kept.
    fn drop(&mut self) {
        if self.profile_dirty {
            let _ = self.save_profile();
        }
    }
}
