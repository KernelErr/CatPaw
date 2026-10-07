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

use crate::confirm::{ApprovalConfig, Confirmations, LIFETIME, Stage, State};
use crate::journal::{Journal, JournalConfig, redact};
use crate::output::{CallResult, Failure, ToolOutput};
use crate::policy::{Policy, Verdict};
use crate::profile::{Profile, Storage};
use crate::tab::{GroupSetup, GroupState, SnapRequest, TabSummary, View, action_limits};

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

/// Asks the user, through the host, to approve something (MCP
/// elicitation).
pub trait Asker {
    /// `Some(true)` when approved, `Some(false)` when declined, `None`
    /// when the host cannot ask.
    fn approve(&mut self, message: &str) -> Option<bool>;
}

/// A host that cannot ask the user.
pub struct NoAsker;

impl Asker for NoAsker {
    fn approve(&mut self, _message: &str) -> Option<bool> {
        None
    }
}

/// A saved session (`session` tool): cookies, storage, and the tabs.
#[derive(Clone, Debug)]
struct Checkpoint {
    cookies: String,
    storage: Storage,
    /// Each tab's URL and scroll position, the current one first.
    tabs: Vec<(String, (f32, f32))>,
}

impl Checkpoint {
    fn to_json(&self) -> Value {
        json!({
            "cookies": self.cookies,
            "storage": crate::profile::storage_json(&self.storage),
            "tabs": self.tabs.iter().map(|(url, (x, y))| json!({"url": url, "scroll": [x, y]})).collect::<Vec<_>>(),
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        let storage = crate::profile::parse_storage(value["storage"].as_str()?).ok()?;
        let tabs = value["tabs"]
            .as_array()?
            .iter()
            .filter_map(|t| {
                let url = t["url"].as_str()?.to_string();
                let x = t["scroll"][0].as_f64().unwrap_or(0.0) as f32;
                let y = t["scroll"][1].as_f64().unwrap_or(0.0) as f32;
                Some((url, (x, y)))
            })
            .collect();
        Some(Self {
            cookies: value["cookies"].as_str()?.to_string(),
            storage,
            tabs,
        })
    }
}

/// The arguments of a call as one canonical string (keys sorted), so a
/// repeated call can be recognised.
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical(&map[k])))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", items.join(","))
        }
        other => other.to_string(),
    }
}

/// A call as a confirmation remembers it: the tool and its arguments,
/// without `confirmation`.
fn fingerprint(name: &str, arguments: &Value) -> String {
    let mut args = match arguments {
        Value::Null => Value::Object(Default::default()),
        other => other.clone(),
    };
    if let Value::Object(map) = &mut args {
        map.remove("confirmation");
    }
    format!("{name} {}", canonical(&args))
}

/// The confirmation a re-issued call carries.
fn confirmation_of(call: &Call) -> Option<&str> {
    match call {
        Call::Navigate(p) => p.confirmation.as_deref(),
        Call::Click(p) => p.confirmation.as_deref(),
        Call::Type(p) => p.confirmation.as_deref(),
        Call::Press(p) => p.confirmation.as_deref(),
        Call::Select(p) => p.confirmation.as_deref(),
        Call::Act(p) => p.confirmation.as_deref(),
        Call::Evaluate(p) => p.confirmation.as_deref(),
        _ => None,
    }
}

/// Whether a tool changes pages or the context (and so may need the
/// profile saved, or a screenshot in the journal).
fn acts(tool: &str) -> bool {
    matches!(
        tool,
        "navigate"
            | "click"
            | "type"
            | "press"
            | "select"
            | "act"
            | "evaluate"
            | "wait"
            | "tabs"
            | "session"
    )
}

type Group = GroupHandle<GroupState>;

/// What a group says about its tabs after a call: ids and openers.
type TabList = Vec<(u32, Option<u32>)>;

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
    journal: Option<Journal>,
    profile: Option<Profile>,
    checkpoints: BTreeMap<String, Checkpoint>,
    tools: Vec<String>,
}

fn tab_list(state: &GroupState) -> TabList {
    state
        .tab_ids()
        .into_iter()
        .map(|id| (id, state.opener_of(id)))
        .collect()
}

/// `t2` (or `2`) as a number.
fn parse_tab(text: &str) -> Result<u32, Failure> {
    let t = text.trim();
    let digits = t.strip_prefix('t').unwrap_or(t);
    digits
        .parse()
        .map_err(|_| Failure::bad_argument(format!("{t:?} is not a tab id (t1, t2, ...)")))
}

impl Session {
    pub fn new(config: SessionConfig) -> Result<Self, EngineError> {
        let mut options = config.options;
        let profile = config.profile.map(Profile::new);
        if let Some(profile) = &profile {
            let (cookies, storage) = profile
                .load()
                .map_err(|e| EngineError::Net(catpaw_net::NetError::Replay(e)))?;
            if cookies.is_some() {
                options.net.cookies_json = cookies;
            }
            options.storage.extend(storage);
        }
        let net = SharedNet::new(options.net.clone())?;
        let journal_config = config.journal.or_else(|| {
            profile.as_ref().map(|p| JournalConfig {
                dir: p.journal_dir(),
                screens: false,
            })
        });
        let journal = journal_config.and_then(|c| {
            let start = json!({
                "catpaw": env!("CARGO_PKG_VERSION"),
                "policy": config.policy.preset.as_str(),
                "trusted": config.policy.trusted,
                "allowedDomains": config.policy.allowed_domains,
            });
            match Journal::open(&c, start) {
                Ok(journal) => Some(journal),
                Err(e) => {
                    eprintln!("catpaw: not keeping a journal in {}: {e}", c.dir.display());
                    None
                }
            }
        });
        Ok(Self {
            net,
            options,
            view: View {
                format: config.format,
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
            journal,
            profile,
            checkpoints: BTreeMap::new(),
            tools: config.tools,
        })
    }

    /// The optional tools this session offers.
    pub fn optional_tools(&self) -> &[String] {
        &self.tools
    }

    /// Where the journal is being written, when there is one.
    pub fn journal_dir(&self) -> Option<&std::path::Path> {
        self.journal.as_ref().map(Journal::dir)
    }

    /// Approves or declines confirmation `cN` as the user (for embedders
    /// that ask the user their own way); `false` when it is not pending.
    pub fn decide(&mut self, id: u32, approve: bool) -> bool {
        self.confirmations.decide(id, approve)
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
        self.call_tool_asking(name, arguments, &mut NoAsker)
    }

    /// Runs a tool, asking the user through `asker` when the policy wants
    /// an approval.
    pub fn call_tool_asking(
        &mut self,
        name: &str,
        arguments: Value,
        asker: &mut dyn Asker,
    ) -> ToolOutput {
        let started = Instant::now();
        let fingerprint = fingerprint(name, &arguments);
        let output = match params::parse(name, arguments.clone()) {
            Ok(Call::Session(_)) if !self.tools.iter().any(|t| t == "session") => {
                Failure::bad_argument("no tool is called \"session\"").render()
            }
            Ok(call) => match self.run(call, &fingerprint, asker) {
                Ok(output) => output,
                Err(failure) => failure.render(),
            },
            Err(message) => Failure::bad_argument(message).render(),
        };
        self.after_call(name, &arguments, &output, started);
        output
    }

    /// Journals a call and keeps the profile up to date.
    fn after_call(&mut self, name: &str, arguments: &Value, output: &ToolOutput, started: Instant) {
        let acted = acts(name);
        if let Some(screens) = self.journal.as_ref().map(Journal::keeps_screens) {
            let first = output.text.lines().next().unwrap_or("");
            let consequences: Vec<&str> = output
                .text
                .lines()
                .filter(|l| l.starts_with("! "))
                .take(10)
                .collect();
            let url = self
                .current
                .and_then(|tab| self.place_of(tab))
                .map(|(url, _)| url);
            let screen = match (screens && acted && !output.is_error, self.current) {
                (true, Some(tab)) => self.screen_of(tab),
                _ => None,
            };
            let tab = self.current.map(|t| format!("t{t}"));
            if let Some(journal) = &mut self.journal {
                let seq = journal.write(
                    "call",
                    json!({
                        "tool": name,
                        "args": redact(name, arguments, output.secret_input),
                        "tab": tab,
                        "result": first.split_whitespace().next().unwrap_or(""),
                        "line": first,
                        "chars": output.text.len(),
                        "ms": started.elapsed().as_millis() as u64,
                        "url": url,
                        "consequences": consequences,
                    }),
                );
                if let Some(png) = screen {
                    journal.screen(seq, &png);
                }
            }
        }
        if acted
            && self.profile.is_some()
            && let Err(e) = self.save_profile()
        {
            eprintln!("catpaw: saving the profile: {e}");
        }
    }

    fn journal(&mut self, kind: &str, fields: Value) {
        if let Some(journal) = &mut self.journal {
            journal.write(kind, fields);
        }
    }

    /// Runs a call: asks first when the policy says so, resumes a
    /// confirmed one, and asks about what the call left held.
    fn run(&mut self, call: Call, fingerprint: &str, asker: &mut dyn Asker) -> CallResult {
        if let Some(id) = confirmation_of(&call) {
            let id = id.to_string();
            return self.resume(&id, call, fingerprint, asker);
        }
        if let Some(what) = self.ask_first(&call)? {
            let tab = self.current_tab()?;
            let id = self.confirmations.create(
                tab,
                fingerprint.to_string(),
                what.clone(),
                Stage::BeforeRunning,
            );
            self.journal(
                "confirmation",
                json!({"id": format!("c{id}"), "what": what}),
            );
            match asker.approve(&approval_message(&what)) {
                Some(true) => {
                    self.confirmations.remove(id);
                    self.journal(
                        "decision",
                        json!({"id": format!("c{id}"), "approved": true, "via": "host"}),
                    );
                }
                Some(false) => {
                    self.confirmations.remove(id);
                    self.journal(
                        "decision",
                        json!({"id": format!("c{id}"), "approved": false, "via": "host"}),
                    );
                    return Ok(declined(id));
                }
                None => return self.needs_confirmation(id, &what, ""),
            }
        }
        let output = self.dispatch(call)?;
        self.ask_about_held(output, fingerprint, asker)
    }

    /// What the policy wants approved before the call runs.
    fn ask_first(&self, call: &Call) -> Result<Option<String>, Failure> {
        Ok(match call {
            Call::Act(p)
                if p.kind == ActKind::Upload && self.policy.upload() == Verdict::Confirm =>
            {
                let paths = p.files.clone().unwrap_or_default();
                if paths.is_empty() {
                    return Err(Failure::bad_argument(
                        "upload needs files: paths of local files",
                    ));
                }
                let names = crate::files::describe(self.setup.files_root.as_deref(), &paths)?;
                let target = p.target.as_deref().unwrap_or("(no target)");
                Some(format!("upload {names} into {target}"))
            }
            Call::Evaluate(p) if self.policy.evaluate() == Verdict::Confirm => Some(format!(
                "run a script in the page: {}",
                quote(&truncate(p.script.trim(), 200))
            )),
            _ => None,
        })
    }

    /// When the call left something held (a form submission, requests),
    /// asks the user about it.
    fn ask_about_held(
        &mut self,
        mut output: ToolOutput,
        fingerprint: &str,
        asker: &mut dyn Asker,
    ) -> CallResult {
        let Some(held) = output.held.take() else {
            return Ok(output);
        };
        let tab = self.current_tab()?;
        let (first, rest) = output
            .text
            .split_once('\n')
            .unwrap_or((output.text.as_str(), ""));
        let action = first.strip_prefix("ok ").unwrap_or(first).to_string();
        let rest = rest.to_string();
        let what = format!("{action} would {held}");
        let id = self
            .confirmations
            .create(tab, fingerprint.to_string(), what.clone(), Stage::Held);
        self.journal(
            "confirmation",
            json!({"id": format!("c{id}"), "what": what}),
        );
        match asker.approve(&approval_message(&what)) {
            Some(true) => {
                self.confirmations.remove(id);
                self.journal(
                    "decision",
                    json!({"id": format!("c{id}"), "approved": true, "via": "host"}),
                );
                self.release(tab, id, &action)
            }
            Some(false) => {
                self.confirmations.remove(id);
                self.journal(
                    "decision",
                    json!({"id": format!("c{id}"), "approved": false, "via": "host"}),
                );
                self.drop_held(tab);
                Ok(declined(id))
            }
            None => self.needs_confirmation(id, &what, &rest),
        }
    }

    /// `needs_confirmation cN: …` with where the user approves it.
    fn needs_confirmation(&mut self, id: u32, what: &str, rest: &str) -> CallResult {
        let url = self.confirmations.url(id).map_err(|e| {
            Failure::new(
                ErrorCode::Unsupported,
                format!(
                    "c{id} needs the user's approval, and the approval page could not start: {e}"
                ),
            )
        })?;
        let mut text = format!(
            "{} c{id}: {what}\n  {} {url} (expires in {}m), {}\"c{id}\"",
            outcome::NEEDS_CONFIRMATION,
            outcome::ASK_USER,
            LIFETIME.as_secs() / 60,
            outcome::REISSUE
        );
        if !rest.is_empty() {
            text.push('\n');
            text.push_str(rest);
        }
        Ok(ToolOutput::ok(text))
    }

    /// A call re-issued with `confirmation: "cN"`.
    fn resume(
        &mut self,
        text: &str,
        call: Call,
        fingerprint: &str,
        asker: &mut dyn Asker,
    ) -> CallResult {
        let id: u32 = text
            .trim()
            .strip_prefix('c')
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| {
                Failure::bad_argument(format!("{text:?} is not a confirmation (c1, c2, ...)"))
            })?;
        let confirmation = self
            .confirmations
            .get(id)
            .ok_or_else(|| Failure::bad_argument(format!("there is no confirmation c{id}")))?;
        if confirmation.fingerprint != fingerprint {
            return Err(Failure::bad_argument(format!(
                "c{id} is for another call ({}); repeat that call unchanged with confirmation:\"c{id}\"",
                confirmation.what
            )));
        }
        let gone = |session: &mut Self| {
            session.confirmations.remove(id);
            if confirmation.stage == Stage::Held {
                session.drop_held(confirmation.tab);
            }
        };
        if confirmation.expired() && confirmation.state == State::Pending {
            gone(self);
            return Ok(ToolOutput::ok(format!(
                "{} {}: c{id} was not approved in time",
                outcome::BLOCKED,
                outcome::EXPIRED
            )));
        }
        match confirmation.state {
            State::Pending => {
                let url = self.confirmations.url(id).map_err(|e| {
                    Failure::new(
                        ErrorCode::Unsupported,
                        format!("the approval page could not start: {e}"),
                    )
                })?;
                Ok(ToolOutput::ok(format!(
                    "{} c{id} {}: {}\n  {} {url}, {}\"c{id}\"",
                    outcome::NEEDS_CONFIRMATION,
                    outcome::STILL_PENDING,
                    confirmation.what,
                    outcome::ASK_USER,
                    outcome::REISSUE
                )))
            }
            State::Declined => {
                self.journal(
                    "decision",
                    json!({"id": format!("c{id}"), "approved": false, "via": "page"}),
                );
                gone(self);
                Ok(declined(id))
            }
            State::Approved => {
                self.journal(
                    "decision",
                    json!({"id": format!("c{id}"), "approved": true, "via": "page"}),
                );
                self.confirmations.remove(id);
                match confirmation.stage {
                    Stage::Held => {
                        let action = confirmation
                            .what
                            .split(" would ")
                            .next()
                            .unwrap_or("")
                            .to_string();
                        self.release(confirmation.tab, id, &action)
                    }
                    Stage::BeforeRunning => {
                        let output = self.dispatch(call)?;
                        self.ask_about_held(output, fingerprint, asker)
                    }
                }
            }
        }
    }

    /// Lets what an approved confirmation held go.
    fn release(&mut self, tab: u32, id: u32, action: &str) -> CallResult {
        let status = format!("ok {action} (confirmed c{id})");
        self.on_tab(tab, move |g, tab, view| g.release_held(tab, status, view))
    }

    /// Drops what a tab's page holds.
    fn drop_held(&mut self, tab: u32) {
        let _ = self.on_tab(tab, |g, _, _| {
            g.drop_held();
            Ok(ToolOutput::default())
        });
    }

    fn place_of(&self, tab: u32) -> Option<(String, (f32, f32))> {
        let group = self.groups.get(self.routes.get(&tab)?)?;
        group
            .call(move |g| g.place(tab))
            .ok()
            .flatten()
            .map(|(url, scroll)| (url.to_string(), scroll))
    }

    fn screen_of(&self, tab: u32) -> Option<Vec<u8>> {
        let group = self.groups.get(self.routes.get(&tab)?)?;
        group.call(move |g| g.screen(tab)).ok().flatten()
    }

    fn dispatch(&mut self, call: Call) -> CallResult {
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
                let tab = self.current_tab()?;
                self.on_tab(tab, move |g, tab, view| g.snapshot(tab, p, view))
            }
            Call::Click(p) => {
                let tab = self.current_tab()?;
                self.on_tab(tab, move |g, tab, view| g.click(tab, p, view))
            }
            Call::Type(p) => {
                let tab = self.current_tab()?;
                self.on_tab(tab, move |g, tab, view| g.type_text(tab, p, view))
            }
            Call::Press(p) => {
                let tab = self.current_tab()?;
                self.on_tab(tab, move |g, tab, view| g.press(tab, p, view))
            }
            Call::Select(p) => {
                let tab = self.current_tab()?;
                self.on_tab(tab, move |g, tab, view| g.select(tab, p, view))
            }
            Call::Act(p) => {
                let tab = self.current_tab()?;
                self.on_tab(tab, move |g, tab, view| g.act(tab, p, view))
            }
            Call::Read(p) => {
                let tab = self.current_tab()?;
                self.on_tab(tab, move |g, tab, _| g.read(tab, p))
            }
            Call::Screenshot(p) => {
                let tab = self.current_tab()?;
                self.on_tab(tab, move |g, tab, _| g.screenshot(tab, p))
            }
            Call::Evaluate(p) => {
                let tab = self.current_tab()?;
                self.on_tab(tab, move |g, tab, _| g.evaluate(tab, p, &action_limits()))
            }
            Call::Tabs(p) => self.tabs(p),
            Call::Wait(p) => {
                let tab = self.current_tab()?;
                self.on_tab(tab, move |g, tab, view| g.wait(tab, p, view))
            }
            Call::Logs(p) => {
                let tab = self.current_tab()?;
                self.on_tab(tab, move |g, tab, _| g.logs(tab, p))
            }
            Call::Session(p) => self.session_tool(p),
        }
    }

    fn current_tab(&self) -> Result<u32, Failure> {
        self.current
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, "no tab is open").with(advice::NO_TAB))
    }

    /// Opens a group with one blank tab, which becomes the current tab.
    fn open_group(&mut self) -> Result<u32, Failure> {
        let tab = self.next_tab.fetch_add(1, Ordering::SeqCst);
        let id = self.next_group;
        self.next_group += 1;
        let options = self.options.clone();
        let net = self.net.clone();
        let next_tab = self.next_tab.clone();
        let setup = self.setup.clone();
        let group = GroupHandle::spawn(&format!("catpaw-group-{id}"), move || {
            GroupState::new(&options, &net, tab, next_tab, setup)
        })
        .map_err(|e| Failure::new(ErrorCode::Crashed, format!("could not open a tab: {e}")))?;
        self.groups.insert(id, group);
        self.routes.insert(tab, id);
        self.openers.insert(tab, None);
        self.current = Some(tab);
        Ok(tab)
    }

    /// Runs `f` on the group of `tab`, then catches up with the tabs the
    /// call opened or closed.
    fn on_tab(
        &mut self,
        tab: u32,
        f: impl FnOnce(&mut GroupState, u32, View) -> CallResult + Send + 'static,
    ) -> CallResult {
        let group_id = *self
            .routes
            .get(&tab)
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        let group = self
            .groups
            .get(&group_id)
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        let view = self.view;
        let reply = group.call(move |state| {
            let result = f(state, tab, view);
            (result, tab_list(state))
        });
        match reply {
            Ok((result, tabs)) => {
                self.reconcile(group_id, tabs);
                let note = self.repair_current();
                result.map(|mut output| {
                    if let Some(note) = note {
                        output.text.push('\n');
                        output.text.push_str(&note);
                    }
                    output
                })
            }
            Err(_) => {
                let lost = self.drop_group(group_id);
                let note = self.repair_current();
                let mut failure = Failure::new(
                    ErrorCode::Crashed,
                    format!("the engine failed on this page; closed {}", names(&lost)),
                );
                if let Some(note) = note {
                    failure = failure.with(note);
                }
                Err(failure)
            }
        }
    }

    /// Brings the routes of a group up to date with its tabs.
    fn reconcile(&mut self, group: u32, tabs: TabList) {
        let alive: Vec<u32> = tabs.iter().map(|(id, _)| *id).collect();
        let gone: Vec<u32> = self
            .routes
            .iter()
            .filter(|&(tab, g)| *g == group && !alive.contains(tab))
            .map(|(&tab, _)| tab)
            .collect();
        for tab in gone {
            self.routes.remove(&tab);
        }
        for (id, opener) in tabs {
            self.routes.insert(id, group);
            self.openers.insert(id, opener);
        }
    }

    /// Closes a group; returns the tabs that went with it.
    fn drop_group(&mut self, group: u32) -> Vec<u32> {
        self.groups.remove(&group);
        let lost: Vec<u32> = self
            .routes
            .iter()
            .filter(|&(_, g)| *g == group)
            .map(|(&tab, _)| tab)
            .collect();
        for tab in &lost {
            self.routes.remove(tab);
        }
        lost
    }

    /// When the current tab closed, moves to its opener (or the first
    /// tab), and says so.
    fn repair_current(&mut self) -> Option<String> {
        let current = self.current?;
        if self.routes.contains_key(&current) {
            return None;
        }
        let opener = self.openers.get(&current).copied().flatten();
        self.current = opener
            .filter(|t| self.routes.contains_key(t))
            .or_else(|| self.routes.keys().next().copied());
        Some(match self.current {
            Some(tab) => format!("! current tab is now t{tab}"),
            None => "! no tab is open".to_string(),
        })
    }

    fn tabs(&mut self, p: params::Tabs) -> CallResult {
        match p.op {
            TabsOp::List => self.list_tabs(),
            TabsOp::Switch => {
                let tab = parse_tab(
                    p.tab
                        .as_deref()
                        .ok_or_else(|| Failure::bad_argument("switch needs tab"))?,
                )?;
                if !self.routes.contains_key(&tab) {
                    return Err(
                        Failure::new(ErrorCode::NoTab, format!("t{tab} is not open"))
                            .with(self.list_line()),
                    );
                }
                self.current = Some(tab);
                let snapshot = self.on_tab(tab, move |g, tab, view| {
                    g.snapshot_text(tab, &SnapRequest::default(), view)
                        .map(ToolOutput::ok)
                })?;
                Ok(ToolOutput::ok(format!(
                    "ok switch t{tab}\n{}",
                    snapshot.text
                )))
            }
            TabsOp::Open => {
                let tab = self.open_group()?;
                match p.url {
                    Some(url) => {
                        let p = params::Navigate {
                            url: Some(url),
                            go: None,
                            snapshot: None,
                            confirmation: None,
                        };
                        let mut output =
                            self.on_tab(tab, move |g, tab, view| g.navigate(tab, p, view))?;
                        if let Some(rest) = output.text.strip_prefix("ok navigate") {
                            output.text = format!("ok open t{tab}{rest}");
                        }
                        Ok(output)
                    }
                    None => Ok(ToolOutput::ok(format!("ok open t{tab} about:blank"))),
                }
            }
            TabsOp::Close => {
                let tab = match &p.tab {
                    Some(text) => parse_tab(text)?,
                    None => self.current_tab()?,
                };
                let group_id = *self
                    .routes
                    .get(&tab)
                    .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is not open")))?;
                let group = &self.groups[&group_id];
                let is_top = group.call(move |g| g.is_top(tab)).unwrap_or(true);
                let mut text = format!("ok close t{tab}");
                if is_top {
                    let lost = self.drop_group(group_id);
                    for other in lost.iter().filter(|&&t| t != tab) {
                        text.push_str(&format!("\n! {} t{other}", consequence::TAB_CLOSED));
                    }
                } else {
                    let reply = group.call(move |g| (g.close_popup(tab), tab_list(g)));
                    match reply {
                        Ok((result, tabs)) => {
                            result?;
                            self.reconcile(group_id, tabs);
                        }
                        Err(_) => {
                            self.drop_group(group_id);
                        }
                    }
                }
                if let Some(note) = self.repair_current() {
                    text.push('\n');
                    text.push_str(&note);
                }
                Ok(ToolOutput::ok(text))
            }
        }
    }

    fn summaries(&self) -> Vec<TabSummary> {
        let mut all = Vec::new();
        for group in self.groups.values() {
            if let Ok(list) = group.call(|g| g.summaries()) {
                all.extend(list);
            }
        }
        all.sort_by_key(|t| t.id);
        all
    }

    fn list_tabs(&mut self) -> CallResult {
        let mut text = "ok tabs".to_string();
        let summaries = self.summaries();
        if summaries.is_empty() {
            text.push_str("\n(no tab is open)");
        }
        for tab in summaries {
            let mark = if Some(tab.id) == self.current {
                "*"
            } else {
                ""
            };
            text.push_str(&format!(
                "\nt{}{mark} {} {}",
                tab.id,
                catpaw_agent::snapshot::truncate(&tab.url, 120),
                catpaw_agent::snapshot::quote(&catpaw_agent::snapshot::truncate(&tab.title, 80))
            ));
            if let Some(opener) = tab.opener {
                text.push_str(&format!(" (opened by t{opener})"));
            }
        }
        Ok(ToolOutput::ok(text))
    }

    /// The open tabs on one line, for errors.
    fn list_line(&self) -> String {
        let open: Vec<String> = self.routes.keys().map(|t| format!("t{t}")).collect();
        if open.is_empty() {
            "open tabs: none".to_string()
        } else {
            format!("open tabs: {}", open.join(", "))
        }
    }
}

fn approval_message(what: &str) -> String {
    format!("CatPaw: allow this? {what}")
}

fn declined(id: u32) -> ToolOutput {
    ToolOutput::ok(format!("{} {} c{id}", outcome::BLOCKED, outcome::DECLINED))
}

impl Session {
    /// The `session` tool: checkpoints of cookies, storage and tabs.
    fn session_tool(&mut self, p: params::Session) -> CallResult {
        let name = || {
            p.name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .ok_or_else(|| Failure::bad_argument("save and restore need a name"))
        };
        let file = |profile: &Profile, name: &str| {
            let safe: String = name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            profile.checkpoints_dir().join(format!("{safe}.json"))
        };
        match p.op {
            SessionOp::Save => {
                let name = name()?;
                let mut tabs = Vec::new();
                let mut order: Vec<u32> = self.routes.keys().copied().collect();
                order.sort_by_key(|t| (Some(*t) != self.current, *t));
                for tab in order {
                    if let Some(place) = self.place_of(tab) {
                        tabs.push(place);
                    }
                }
                let checkpoint = Checkpoint {
                    cookies: self.cookies().to_json(),
                    storage: self.storage(),
                    tabs,
                };
                if let Some(profile) = &self.profile {
                    let text =
                        serde_json::to_string_pretty(&checkpoint.to_json()).unwrap_or_default();
                    crate::profile::write_whole(&file(profile, &name), &text).map_err(|e| {
                        Failure::new(
                            ErrorCode::Unsupported,
                            format!("writing the checkpoint: {e}"),
                        )
                    })?;
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
                            && let Some(stem) = path.file_stem()
                        {
                            names.push(stem.to_string_lossy().into_owned());
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
                let name = name()?;
                let checkpoint = match self.checkpoints.get(&name) {
                    Some(c) => c.clone(),
                    None => self
                        .profile
                        .as_ref()
                        .and_then(|profile| std::fs::read_to_string(file(profile, &name)).ok())
                        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                        .and_then(|value| Checkpoint::from_json(&value))
                        .ok_or_else(|| {
                            Failure::new(
                                ErrorCode::NotFound,
                                format!("nothing is saved as {}", quote(&name)),
                            )
                        })?,
                };
                self.groups.clear();
                self.routes.clear();
                self.openers.clear();
                self.current = None;
                let jar = self.net.client().cookies();
                jar.clear();
                jar.load_json(&checkpoint.cookies)
                    .map_err(|e| Failure::new(ErrorCode::Unsupported, e))?;
                self.options.storage = checkpoint.storage.clone();
                let mut opened = Vec::new();
                for (url, (x, y)) in &checkpoint.tabs {
                    let tab = self.open_group()?;
                    let p = params::Navigate {
                        url: Some(url.clone()),
                        go: None,
                        snapshot: Some(params::SnapshotMode::None),
                        confirmation: None,
                    };
                    let (x, y) = (*x, *y);
                    let _ = self.on_tab(tab, move |g, tab, view| {
                        let result = g.navigate(tab, p, view);
                        g.scroll_to(tab, x, y);
                        result
                    });
                    opened.push(format!("t{tab} {}", truncate(url, 120)));
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
}

fn names(tabs: &[u32]) -> String {
    if tabs.is_empty() {
        return "no tabs".to_string();
    }
    tabs.iter()
        .map(|t| format!("t{t}"))
        .collect::<Vec<_>>()
        .join(", ")
}
