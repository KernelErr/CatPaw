//! Confirmations: calls the policy stops until the user approves them, and
//! the approval page where the user decides (ADR 0006, decision 8).
//!
//! The host approves through MCP elicitation when it offers that. Without
//! it, a page served on 127.0.0.1 (see [`crate::local`]) takes the
//! decision, and only with the approval key: a secret kept in a file the
//! agent is never shown (its tools never print it, and the browser it
//! drives refuses private addresses). The browser that approved once keeps
//! the key in its storage for the page's origin.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::http::{Request, escape, page, respond, respond_bytes};
use crate::local::{Pages, body_field};

/// How long a confirmation waits for the user, unless configured.
const LIFETIME: Duration = Duration::from_secs(600);
/// How long a repeated call waits for the user's decision before it says
/// the confirmation is still pending, unless configured.
const DECISION_WAIT: Duration = Duration::from_secs(45);

/// A span as a result says it: `10m`, `45s`.
pub(crate) fn span(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Where a confirmation stopped its call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// What the call started is held (a navigation, requests): approving
    /// lets it go, and the call is not carried out again.
    Held,
    /// The call has not run: approving runs it.
    BeforeRunning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Pending,
    Approved,
    Declined,
    /// The page no longer holds what it was about (it asked for something
    /// else, or moved on).
    Superseded,
}

impl State {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Approved => "approved",
            State::Declined => "declined",
            State::Superseded => "superseded",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Confirmation {
    pub id: u32,
    pub tab: u32,
    /// The call it belongs to: the tool and its arguments, without
    /// `confirmation`.
    pub fingerprint: String,
    /// What would happen, as results and the approval page say it.
    pub what: String,
    /// The action as the result's first line names it (`click e8 button
    /// "Login"`).
    pub action: String,
    pub stage: Stage,
    /// For a held stage: the hold numbers in the tab's page.
    pub holds: Vec<u64>,
    /// The exact call that resumes it (`click {"confirmation":"c1",…}`).
    pub repeat: String,
    pub state: State,
    created: Instant,
    lifetime: Duration,
}

impl Confirmation {
    pub fn expired(&self) -> bool {
        self.created.elapsed() > self.lifetime
    }

    fn minutes_left(&self) -> u64 {
        self.lifetime
            .saturating_sub(self.created.elapsed())
            .as_secs()
            .div_ceil(60)
    }
}

/// A confirmation to record.
pub(crate) struct NewConfirmation {
    pub tab: u32,
    pub fingerprint: String,
    pub what: String,
    pub action: String,
    pub stage: Stage,
    pub holds: Vec<u64>,
    pub repeat: String,
}

#[derive(Default)]
pub(crate) struct Store {
    next: u32,
    items: BTreeMap<u32, Confirmation>,
}

/// The confirmations, shared with the approval page.
pub(crate) type SharedStore = Arc<Mutex<Store>>;

fn lock(store: &SharedStore) -> std::sync::MutexGuard<'_, Store> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where the approval page finds its key, the port it listens on, and
/// how long the user has.
#[derive(Clone, Debug)]
pub struct ApprovalConfig {
    /// The file holding the approval key, made with a fresh key when
    /// missing. By default `catpaw/approval-key` in the user's data
    /// directory.
    pub key_file: Option<PathBuf>,
    /// The port of the local pages; by default a fixed one while it is
    /// free (the browser then keeps the key for it), 0 for any free one.
    pub port: Option<u16>,
    /// How long a confirmation waits for the user (10 minutes).
    pub lifetime: Duration,
    /// How long a repeated call waits for a decision still to come before
    /// it answers (45 seconds).
    pub decision_wait: Duration,
    /// Opens the local pages in the user's browser when the user is needed
    /// there (a hand-off, a confirmation the host cannot ask about); with
    /// none, the agent gives the user the address.
    pub opener: Option<crate::Opener>,
}

impl Default for ApprovalConfig {
    fn default() -> Self {
        Self {
            key_file: None,
            port: None,
            lifetime: LIFETIME,
            decision_wait: DECISION_WAIT,
            opener: None,
        }
    }
}

/// The approval key and the file it is kept in (made when missing): for
/// the user to give the local pages by hand when they were not opened for
/// them, never for the agent.
pub fn approval_key(config: &ApprovalConfig) -> std::io::Result<(PathBuf, String)> {
    let path = key_file(config)?;
    let key = load_or_make_key(&path)?;
    Ok((path, key))
}

pub struct Confirmations {
    store: SharedStore,
    config: ApprovalConfig,
}

impl Confirmations {
    pub fn new(config: ApprovalConfig) -> Self {
        Self {
            store: Arc::default(),
            config,
        }
    }

    pub fn config(&self) -> &ApprovalConfig {
        &self.config
    }

    pub(crate) fn shared(&self) -> SharedStore {
        self.store.clone()
    }

    /// Records a confirmation; its id (the N of `cN`).
    pub(crate) fn create(&self, new: NewConfirmation) -> u32 {
        let mut store = lock(&self.store);
        store.next += 1;
        let id = store.next;
        store.items.insert(
            id,
            Confirmation {
                id,
                tab: new.tab,
                fingerprint: new.fingerprint,
                what: new.what,
                action: new.action,
                stage: new.stage,
                holds: new.holds,
                repeat: new.repeat,
                state: State::Pending,
                created: Instant::now(),
                lifetime: self.config.lifetime,
            },
        );
        id
    }

    /// Sets the call that resumes a confirmation.
    pub fn set_repeat(&self, id: u32, repeat: String) {
        if let Some(c) = lock(&self.store).items.get_mut(&id) {
            c.repeat = repeat;
        }
    }

    /// Starts a confirmation's time again (when the host could not ask
    /// and the approval page takes over).
    pub fn restart(&self, id: u32) {
        if let Some(c) = lock(&self.store).items.get_mut(&id) {
            c.created = Instant::now();
        }
    }

    /// Marks the pending confirmations of `tab` whose holds all went
    /// (`dropped`) as superseded; their ids.
    pub fn supersede(&self, tab: u32, dropped: &[u64]) -> Vec<u32> {
        let mut store = lock(&self.store);
        let mut gone = Vec::new();
        for c in store.items.values_mut() {
            if c.tab == tab
                && c.state == State::Pending
                && c.stage == Stage::Held
                && !c.holds.is_empty()
                && c.holds.iter().all(|h| dropped.contains(h))
            {
                c.state = State::Superseded;
                gone.push(c.id);
            }
        }
        gone
    }

    /// Voids the pending confirmations of a tab that closed.
    pub fn close_for_tab(&self, tab: u32) {
        for c in lock(&self.store).items.values_mut() {
            if c.tab == tab && c.state == State::Pending {
                c.state = State::Superseded;
            }
        }
    }

    pub fn get(&self, id: u32) -> Option<Confirmation> {
        lock(&self.store).items.get(&id).cloned()
    }

    /// Approves or declines a pending confirmation that has not run out;
    /// `false` when there is none.
    pub fn decide(&self, id: u32, approve: bool) -> bool {
        decide(&self.store, id, approve)
    }

    /// Forgets a confirmation once it was used.
    pub fn remove(&self, id: u32) {
        lock(&self.store).items.remove(&id);
    }
}

fn decide(store: &SharedStore, id: u32, approve: bool) -> bool {
    let mut store = lock(store);
    match store.items.get_mut(&id) {
        Some(c) if c.state == State::Pending && !c.expired() => {
            c.state = if approve {
                State::Approved
            } else {
                State::Declined
            };
            true
        }
        _ => false,
    }
}

/// The file the approval key is kept in.
pub(crate) fn key_file(config: &ApprovalConfig) -> std::io::Result<PathBuf> {
    match &config.key_file {
        Some(path) => Ok(path.clone()),
        None => default_key_file()
            .ok_or_else(|| std::io::Error::other("no data directory for the approval key")),
    }
}

/// `catpaw/approval-key` in the user's data directory.
fn default_key_file() -> Option<PathBuf> {
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let base = if cfg!(windows) {
        var("APPDATA")?
    } else if cfg!(target_os = "macos") {
        var("HOME")?.join("Library/Application Support")
    } else {
        var("XDG_DATA_HOME").or_else(|| var("HOME").map(|h| h.join(".local/share")))?
    };
    Some(base.join("catpaw").join("approval-key"))
}

/// The key in `path`, or a fresh one written there (readable by the user
/// alone).
pub(crate) fn load_or_make_key(path: &Path) -> std::io::Result<String> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let key = text.trim().to_string();
        if key.len() >= 32 {
            return Ok(key);
        }
    }
    let key = crate::local::random_hex(32)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::profile::private_file(path)?.write_all(format!("{key}\n").as_bytes())?;
    Ok(key)
}

/// What the approval page's script may do: post the decision to its own
/// address.
const PAGE: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; form-action 'none'";

/// `/confirm/cN`: the page (GET) and the decision (POST, with the key).
pub(crate) fn handle(
    stream: TcpStream,
    request: &Request,
    rest: &str,
    pages: &Pages,
) -> std::io::Result<()> {
    let Some(id) = rest.strip_prefix('c').and_then(|n| n.parse::<u32>().ok()) else {
        return respond(stream, "404 Not Found", "text/plain", &[], "not found\n");
    };
    let Some(confirmation) = lock(&pages.approvals).items.get(&id).cloned() else {
        return respond_bytes(
            stream,
            "404 Not Found",
            "text/html; charset=utf-8",
            PAGE,
            &[],
            page(
                "CatPaw",
                "<h1>No such confirmation</h1><p>It was used, or it never was.</p>",
            )
            .as_bytes(),
        );
    };
    let state = if confirmation.state == State::Pending && confirmation.expired() {
        "expired"
    } else {
        confirmation.state.as_str()
    };
    match request.method.as_str() {
        "GET" => {
            let body = approval_page(id, &confirmation, state, &pages.key_file);
            respond_bytes(
                stream,
                "200 OK",
                "text/html; charset=utf-8",
                PAGE,
                &[],
                page(&format!("CatPaw: approve c{id}?"), &body).as_bytes(),
            )
        }
        "POST" => {
            if !pages.key_given(request) {
                return respond(
                    stream,
                    "403 Forbidden",
                    "application/json",
                    &[],
                    "{\"error\":\"wrong key\"}\n",
                );
            }
            let approve = match body_field(request, "decision").as_deref() {
                Some("approve") => true,
                Some("decline") => false,
                _ => {
                    return respond(
                        stream,
                        "400 Bad Request",
                        "application/json",
                        &[],
                        "{\"error\":\"decision: approve or decline\"}\n",
                    );
                }
            };
            let changed = decide(&pages.approvals, id, approve);
            if changed {
                pages.journal(
                    "decision",
                    json!({"id": format!("c{id}"), "approved": approve, "via": "page"}),
                );
            }
            let now = lock(&pages.approvals)
                .items
                .get(&id)
                .map(|c| c.state.as_str())
                .unwrap_or("gone");
            let body = json!({"id": format!("c{id}"), "state": now, "changed": changed});
            respond(
                stream,
                "200 OK",
                "application/json",
                &[],
                &format!("{body}\n"),
            )
        }
        _ => respond(
            stream,
            "405 Method Not Allowed",
            "text/plain",
            &[],
            "GET or POST\n",
        ),
    }
}

fn approval_page(id: u32, confirmation: &Confirmation, state: &str, key_file: &Path) -> String {
    let mut body = format!(
        "<h1>Approve this action?</h1><p>An agent using CatPaw asks to:</p><pre>{}</pre>",
        escape(&confirmation.what)
    );
    if state != "pending" {
        body.push_str(&format!("<p>This confirmation is {state}.</p>"));
        return body;
    }
    body.push_str(&format!(
        r#"<p><small>c{id}, expires in {minutes} min</small></p>
<div id=keyrow><p><label>Approval key <input id=key type=password autocomplete=off></label><br>
<small>Run <code>catpaw approval-key</code> in a terminal yourself, or open <code>{file}</code>; an agent that sees the key can approve on its own. This browser keeps a pass for this session only. <label><input id=remember type=checkbox style="width:auto"> Remember the key itself here (it then works for every session)</label></small></p></div>
<p id=buttons><button id=approve>Approve</button><button id=decline>Decline</button></p>
<p id=said></p>
<script>
const said = document.getElementById('said');
const keyField = document.getElementById('key');
// A page CatPaw opened for the user brings a pass, good once: it leaves
// the address at once.
const pass = new URLSearchParams(location.search).get('pass');
if (pass) history.replaceState(null, '', location.pathname);
let key = null;
try {{ key = localStorage.getItem('catpaw-approval-key') || localStorage.getItem('catpaw-session-token'); }} catch (e) {{}}
if (key) document.getElementById('keyrow').hidden = true;
(async () => {{
  if (!pass) return;
  try {{
    const r = await fetch('/session', {{method: 'POST', headers: {{'Content-Type': 'application/json'}}, body: JSON.stringify({{pass}})}});
    if (!r.ok) return;
    key = (await r.json()).token;
    document.getElementById('keyrow').hidden = true;
    localStorage.setItem('catpaw-session-token', key);
  }} catch (e) {{}}
}})();
function forget() {{
  try {{ localStorage.removeItem('catpaw-approval-key'); localStorage.removeItem('catpaw-session-token'); }} catch (e) {{}}
  key = null;
}}
async function keep(typed) {{
  try {{
    if (document.getElementById('remember').checked) localStorage.setItem('catpaw-approval-key', typed);
    const r = await fetch('/session', {{method: 'POST', headers: {{'Content-Type': 'application/json'}}, body: JSON.stringify({{key: typed}})}});
    if (r.ok) localStorage.setItem('catpaw-session-token', (await r.json()).token);
  }} catch (e) {{}}
}}
async function decide(decision) {{
  const typed = key ? null : keyField.value.trim();
  const k = key || typed;
  const r = await fetch(location.pathname, {{method: 'POST', headers: {{'Content-Type': 'application/json'}}, body: JSON.stringify({{decision, key: k}})}});
  const s = await r.json().catch(() => ({{}}));
  if (r.status === 403) {{
    forget();
    document.getElementById('keyrow').hidden = false;
    said.textContent = typed === null ? 'The pass this browser kept is from a session that ended: enter the key.' : 'That key does not match.';
    return;
  }}
  if (r.ok && typed) await keep(typed);
  document.getElementById('buttons').hidden = true;
  said.textContent = s.state === 'approved' ? 'Approved. The agent can go on.'
    : s.state === 'declined' ? 'Declined. The agent is told so.'
    : 'This confirmation is ' + (s.state || 'gone') + '.';
}}
document.getElementById('approve').onclick = () => decide('approve');
document.getElementById('decline').onclick = () => decide('decline');
</script>"#,
        minutes = confirmation.minutes_left(),
        file = escape(&key_file.display().to_string()),
    ));
    body
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;
    use crate::local::LocalServer;

    fn raw(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).unwrap();
        out
    }

    fn post(port: u16, path: &str, headers: &str, body: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{headers}Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn only_the_key_approves() {
        let dir = std::env::temp_dir().join(format!("catpaw-approve-{}", std::process::id()));
        let key_file = dir.join("key");
        let key = load_or_make_key(&key_file).unwrap();
        assert_eq!(key.len(), 64);
        let confirmations = Confirmations::new(ApprovalConfig {
            key_file: Some(key_file.clone()),
            port: Some(0),
            ..ApprovalConfig::default()
        });
        let id = confirmations.create(NewConfirmation {
            tab: 1,
            fingerprint: "click {}".into(),
            what: "click e2 would submit".into(),
            action: "click e2".into(),
            stage: Stage::Held,
            holds: vec![1],
            repeat: "click {}".into(),
        });
        let server = LocalServer::start(Some(0), |port| Pages {
            passes: Default::default(),
            key: key.clone(),
            token: "t".repeat(64),
            key_file: key_file.clone(),
            port,
            approvals: confirmations.shared(),
            handoffs: Arc::default(),
            journal: None,
        })
        .unwrap();
        let port = server.port();
        let path = format!("/confirm/c{id}");
        let wrong = post(
            port,
            &path,
            "Authorization: Bearer nope\r\n",
            "decision=approve",
        );
        assert!(wrong.starts_with("HTTP/1.1 403"), "{wrong}");
        let elsewhere = post(
            port,
            &path,
            "Origin: https://evil.example\r\n",
            &format!("decision=approve&key={key}"),
        );
        assert!(elsewhere.starts_with("HTTP/1.1 403"), "{elsewhere}");
        // Requests too large are refused, not cut short.
        let big = raw(
            port,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 99999\r\n\r\n"
            ),
        );
        assert!(big.starts_with("HTTP/1.1 413"), "{big}");
        let long = raw(
            port,
            &format!("GET {path}?{} HTTP/1.1\r\n\r\n", "x".repeat(9000)),
        );
        assert!(long.starts_with("HTTP/1.1 414"), "{long}");
        assert_eq!(confirmations.get(id).unwrap().state, State::Pending);
        // A client that sends its request a byte at a time is cut off,
        // and others are answered meanwhile.
        let mut slow = TcpStream::connect(("127.0.0.1", port)).unwrap();
        slow.write_all(b"GET /con").unwrap();
        let started = std::time::Instant::now();
        let meanwhile = raw(
            port,
            &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        );
        assert!(meanwhile.starts_with("HTTP/1.1 200"), "{meanwhile}");
        let mut cut = false;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if slow.write_all(b"f").is_err() {
                cut = true;
                break;
            }
        }
        assert!(cut, "the slow request was cut off");
        assert!(started.elapsed() >= std::time::Duration::from_millis(700));

        // The key buys this session's pass, which decides like the key.
        let refused = post(port, "/session", "", "key=nope");
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        let token = "t".repeat(64);
        let pass = post(port, "/session", "", &format!("key={key}"));
        assert!(pass.contains(&token), "{pass}");
        let typed = post(port, &path, "", &format!("decision=approve&key={token}"));
        assert!(typed.starts_with("HTTP/1.1 200"), "{typed}");
        assert!(
            !typed.to_ascii_lowercase().contains("set-cookie"),
            "{typed}"
        );
        assert_eq!(confirmations.get(id).unwrap().state, State::Approved);

        // Decided once; another call reports it.
        let again = post(
            port,
            &path,
            &format!("Authorization: Bearer {key}\r\n"),
            "decision=decline",
        );
        assert!(again.contains("\"state\":\"approved\""), "{again}");
        assert!(again.contains("\"changed\":false"), "{again}");
        drop(server);
        assert!(
            TcpStream::connect(("127.0.0.1", port)).is_err(),
            "the server stops with its owner"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
