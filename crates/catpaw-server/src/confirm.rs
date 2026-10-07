//! Confirmations: calls the policy stops until the user approves them, and
//! the local page where the user does (ADR 0006, decision 8).
//!
//! The host approves through MCP elicitation when it offers that. Without
//! it, a page served on 127.0.0.1 takes the decision, and only with the
//! approval key: a secret kept in a file the agent is never shown (its
//! tools never print it, and the browser it drives refuses private
//! addresses). A browser that approved once keeps the key in a cookie.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::http::{escape, page, read_request, respond};

/// How long a confirmation waits for the user.
pub const LIFETIME: Duration = Duration::from_secs(600);

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
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Approved => "approved",
            State::Declined => "declined",
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
    pub stage: Stage,
    pub state: State,
    created: Instant,
}

impl Confirmation {
    pub fn expired(&self) -> bool {
        self.created.elapsed() > LIFETIME
    }

    fn minutes_left(&self) -> u64 {
        LIFETIME
            .saturating_sub(self.created.elapsed())
            .as_secs()
            .div_ceil(60)
    }
}

#[derive(Default)]
struct Store {
    next: u32,
    items: BTreeMap<u32, Confirmation>,
}

/// Where the approval page finds its key, and the port it listens on.
#[derive(Clone, Debug, Default)]
pub struct ApprovalConfig {
    /// The file holding the approval key, made with a fresh key when
    /// missing. By default `catpaw/approval-key` in the user's data
    /// directory.
    pub key_file: Option<PathBuf>,
    /// The port of the approval page; 0 takes any free one.
    pub port: u16,
}

pub struct Confirmations {
    store: Arc<Mutex<Store>>,
    config: ApprovalConfig,
    /// The approval page's port, once a confirmation needed the page.
    port: Option<u16>,
}

impl Confirmations {
    pub fn new(config: ApprovalConfig) -> Self {
        Self {
            store: Arc::default(),
            config,
            port: None,
        }
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records a confirmation; its id (the N of `cN`).
    pub fn create(&self, tab: u32, fingerprint: String, what: String, stage: Stage) -> u32 {
        let mut store = self.store();
        store.next += 1;
        let id = store.next;
        store.items.insert(
            id,
            Confirmation {
                id,
                tab,
                fingerprint,
                what,
                stage,
                state: State::Pending,
                created: Instant::now(),
            },
        );
        id
    }

    pub fn get(&self, id: u32) -> Option<Confirmation> {
        self.store().items.get(&id).cloned()
    }

    /// Approves or declines a pending confirmation that has not run out;
    /// `false` when there is none.
    pub fn decide(&self, id: u32, approve: bool) -> bool {
        decide(&self.store, id, approve)
    }

    /// Forgets a confirmation once it was used.
    pub fn remove(&self, id: u32) {
        self.store().items.remove(&id);
    }

    /// The address of the approval page for `cN`, starting the page when
    /// it is not running yet.
    pub fn url(&mut self, id: u32) -> std::io::Result<String> {
        let port = match self.port {
            Some(port) => port,
            None => {
                let key_file = match &self.config.key_file {
                    Some(path) => path.clone(),
                    None => default_key_file().ok_or_else(|| {
                        std::io::Error::other("no data directory for the approval key")
                    })?,
                };
                let key = load_or_make_key(&key_file)?;
                let listener = TcpListener::bind(("127.0.0.1", self.config.port))?;
                let port = listener.local_addr()?.port();
                let shared = Arc::new(Shared {
                    store: self.store.clone(),
                    key,
                    key_file,
                    port,
                });
                std::thread::Builder::new()
                    .name("catpaw-approvals".to_string())
                    .spawn(move || serve(listener, &shared))?;
                self.port = Some(port);
                port
            }
        };
        Ok(format!("http://127.0.0.1:{port}/confirm/c{id}"))
    }
}

fn decide(store: &Mutex<Store>, id: u32, approve: bool) -> bool {
    let mut store = store.lock().unwrap_or_else(|e| e.into_inner());
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
fn load_or_make_key(path: &Path) -> std::io::Result<String> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let key = text.trim().to_string();
        if key.len() >= 32 {
            return Ok(key);
        }
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
    let key: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)?
        .write_all(format!("{key}\n").as_bytes())?;
    Ok(key)
}

/// What the page's thread shares with the session.
struct Shared {
    store: Arc<Mutex<Store>>,
    key: String,
    key_file: PathBuf,
    port: u16,
}

fn serve(listener: TcpListener, shared: &Shared) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = handle(stream, shared);
    }
}

fn handle(stream: TcpStream, shared: &Shared) -> std::io::Result<()> {
    let request = read_request(&stream)?;
    let ours = [
        format!("127.0.0.1:{}", shared.port),
        format!("localhost:{}", shared.port),
    ];
    // Another name for this address (DNS rebinding) is refused.
    if !request
        .header("host")
        .is_some_and(|h| ours.iter().any(|o| o == h))
    {
        return respond(
            stream,
            "421 Misdirected Request",
            "text/plain",
            &[],
            "wrong host\n",
        );
    }
    let id = request
        .path
        .split('?')
        .next()
        .and_then(|p| p.strip_prefix("/confirm/c"))
        .and_then(|n| n.parse::<u32>().ok());
    let Some(id) = id else {
        return respond(stream, "404 Not Found", "text/plain", &[], "not found\n");
    };
    let Some(confirmation) = shared
        .store
        .lock()
        .ok()
        .and_then(|s| s.items.get(&id).cloned())
    else {
        return respond(
            stream,
            "404 Not Found",
            "text/html; charset=utf-8",
            &[],
            &page("CatPaw", "<h1>No such confirmation</h1>"),
        );
    };
    let api = request.header("authorization").is_some();
    let bearer = request
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let form: Vec<(String, String)> = url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect();
    let field = |name: &str| {
        form.iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    let cookie_ok = request.cookie("catpaw_key") == Some(shared.key.as_str());
    let state = if confirmation.expired() && confirmation.state == State::Pending {
        "expired"
    } else {
        confirmation.state.as_str()
    };

    if request.method == "GET" {
        let mut body = format!(
            "<h1>Approve this action?</h1><p>An agent using CatPaw asks to:</p><pre>{}</pre>",
            escape(&confirmation.what)
        );
        if state == "pending" {
            body.push_str(&format!(
                "<p><small>c{id}, expires in {} min</small></p><form method=post>",
                confirmation.minutes_left()
            ));
            if !cookie_ok {
                body.push_str(&format!(
                    "<p><label>Approval key <input type=password name=key autocomplete=off required></label><br><small>From <code>{}</code>; this browser keeps it after the first time.</small></p>",
                    escape(&shared.key_file.display().to_string())
                ));
            }
            body.push_str("<button name=decision value=approve>Approve</button><button name=decision value=decline>Decline</button></form>");
        } else {
            body.push_str(&format!("<p>This confirmation is {state}.</p>"));
        }
        return respond(
            stream,
            "200 OK",
            "text/html; charset=utf-8",
            &[],
            &page(&format!("CatPaw: approve c{id}?"), &body),
        );
    }
    if request.method != "POST" {
        return respond(
            stream,
            "405 Method Not Allowed",
            "text/plain",
            &[],
            "GET or POST\n",
        );
    }
    // A form posted from another site is not the user's decision.
    if let Some(origin) = request.header("origin")
        && !ours.iter().any(|o| origin == format!("http://{o}"))
    {
        return respond(stream, "403 Forbidden", "text/plain", &[], "wrong origin\n");
    }
    let typed = field("key").map(str::trim);
    let authorized =
        cookie_ok || bearer == Some(shared.key.as_str()) || typed == Some(shared.key.as_str());
    if !authorized {
        return if api {
            respond(
                stream,
                "403 Forbidden",
                "application/json",
                &[],
                "{\"error\":\"wrong key\"}\n",
            )
        } else {
            respond(
                stream,
                "403 Forbidden",
                "text/html; charset=utf-8",
                &[],
                &page(
                    "CatPaw",
                    "<h1>That key does not match</h1><p><a href=\"\">Try again</a></p>",
                ),
            )
        };
    }
    let approve = match field("decision") {
        Some("approve") => true,
        Some("decline") => false,
        _ => {
            return respond(
                stream,
                "400 Bad Request",
                "text/plain",
                &[],
                "decision: approve or decline\n",
            );
        }
    };
    let decided = decide(&shared.store, id, approve);
    let now = shared
        .store
        .lock()
        .ok()
        .and_then(|s| s.items.get(&id).map(|c| c.state.as_str()))
        .unwrap_or("gone");
    if api {
        let body = format!("{{\"id\":\"c{id}\",\"state\":\"{now}\",\"changed\":{decided}}}\n");
        return respond(stream, "200 OK", "application/json", &[], &body);
    }
    let mut extra = Vec::new();
    if typed == Some(shared.key.as_str()) {
        extra.push(format!(
            "Set-Cookie: catpaw_key={}; Path=/; HttpOnly; SameSite=Strict",
            shared.key
        ));
    }
    let said = match (decided, approve) {
        (true, true) => "Approved. The agent can go on.".to_string(),
        (true, false) => "Declined. The agent is told so.".to_string(),
        (false, _) => format!("This confirmation is {now}; nothing changed."),
    };
    respond(
        stream,
        "200 OK",
        "text/html; charset=utf-8",
        &extra,
        &page(&format!("CatPaw: c{id}"), &format!("<h1>{said}</h1>")),
    )
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

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
        let mut confirmations = Confirmations::new(ApprovalConfig {
            key_file: Some(key_file.clone()),
            port: 0,
        });
        let id = confirmations.create(
            1,
            "click {}".into(),
            "click e2 would submit".into(),
            Stage::Held,
        );
        let url = confirmations.url(id).unwrap();
        let port: u16 = url
            .strip_prefix("http://127.0.0.1:")
            .and_then(|r| r.split('/').next())
            .unwrap()
            .parse()
            .unwrap();
        let key = std::fs::read_to_string(&key_file)
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(key.len(), 64);

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
        assert_eq!(confirmations.get(id).unwrap().state, State::Pending);

        let typed = post(port, &path, "", &format!("decision=approve&key={key}"));
        assert!(typed.starts_with("HTTP/1.1 200"), "{typed}");
        assert!(typed.contains("Set-Cookie: catpaw_key="), "{typed}");
        assert_eq!(confirmations.get(id).unwrap().state, State::Approved);

        // Decided once; an API call reports it.
        let again = post(
            port,
            &path,
            &format!("Authorization: Bearer {key}\r\n"),
            "decision=decline",
        );
        assert!(
            again.contains("\"state\":\"approved\",\"changed\":false"),
            "{again}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
