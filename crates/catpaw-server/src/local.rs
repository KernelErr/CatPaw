//! The pages a session serves on 127.0.0.1 for its user: the approval page
//! and the hand-off viewer (ADR 0006, decisions 8 and 15).
//!
//! One server per session, started when a page is first needed. Every
//! state-changing request needs the approval key, or the token the
//! session gives for it: the pages keep that token in the browser's
//! storage for this origin (never in a cookie, which every port of the
//! host would receive), and it is worth nothing once the session ends.
//! The key itself is kept there only when the user asks; the port stays
//! the same between sessions while it is free, so the browser finds it
//! again. Requests for other host names (DNS rebinding) or from other
//! origins are refused. It answers a limited number of connections at a
//! time, each for a limited time, and stops with the session.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;

use crate::http::{Request, read_request, refuse_unread, respond};
use crate::journal::SharedJournal;

/// The port tried first, so that the page's origin (and the key the
/// browser keeps for it) stays the same between sessions.
pub const DEFAULT_PORT: u16 = 47115;
/// Connections handled at once; more are turned away.
const MAX_CONNECTIONS: usize = 16;

/// `bytes` random bytes in hex: keys and tokens of the local pages.
pub(crate) fn random_hex(bytes: usize) -> std::io::Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// What the pages share with the session.
pub(crate) struct Pages {
    pub key: String,
    /// What the pages keep in place of the key: good for this session
    /// only.
    pub token: String,
    pub key_file: PathBuf,
    pub port: u16,
    pub approvals: crate::confirm::SharedStore,
    pub handoffs: crate::handoff::SharedStore,
    pub journal: Option<SharedJournal>,
}

impl Pages {
    /// The host names this server answers to.
    fn hosts(&self) -> [String; 2] {
        [
            format!("127.0.0.1:{}", self.port),
            format!("localhost:{}", self.port),
        ]
    }

    /// What the request gives as its key: `X-CatPaw-Key`, a bearer token,
    /// or `key` in its form or JSON body.
    fn given(request: &Request) -> Option<String> {
        request
            .header("x-catpaw-key")
            .map(str::to_string)
            .or_else(|| {
                request
                    .header("authorization")
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .map(|v| v.trim().to_string())
            })
            .or_else(|| body_field(request, "key"))
            .map(|key| key.trim().to_string())
    }

    /// Whether the request carries the approval key or this session's
    /// token for it.
    pub fn key_given(&self, request: &Request) -> bool {
        Self::given(request).is_some_and(|key| {
            constant_time_eq(key.as_bytes(), self.key.as_bytes())
                | constant_time_eq(key.as_bytes(), self.token.as_bytes())
        })
    }

    /// `POST /session` with the key: this session's token, for the pages
    /// to keep in place of the key.
    fn session_token(&self, stream: TcpStream, request: &Request) -> std::io::Result<()> {
        let key = Self::given(request).unwrap_or_default();
        if request.method != "POST" || !constant_time_eq(key.as_bytes(), self.key.as_bytes()) {
            return respond(
                stream,
                "403 Forbidden",
                "application/json",
                &[],
                "{\"error\":\"wrong key\"}\n",
            );
        }
        let body = serde_json::json!({ "token": self.token });
        respond(
            stream,
            "200 OK",
            "application/json",
            &[],
            &format!("{body}\n"),
        )
    }

    /// Writes a journal record, when the session keeps a journal.
    pub fn journal(&self, kind: &str, fields: Value) {
        if let Some(journal) = &self.journal {
            journal.lock().write(kind, fields);
        }
    }
}

/// A field of a request's body, form-encoded or JSON.
pub(crate) fn body_field(request: &Request, name: &str) -> Option<String> {
    if let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&request.body) {
        return map.get(name).and_then(Value::as_str).map(str::to_string);
    }
    url::form_urlencoded::parse(&request.body)
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(crate) struct LocalServer {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LocalServer {
    /// Starts the server: on `port` when given (0 for any free one), else
    /// on [`DEFAULT_PORT`] or, when that is taken, any free one.
    pub fn start(port: Option<u16>, pages: impl FnOnce(u16) -> Pages) -> std::io::Result<Self> {
        let listener = match port {
            Some(port) => TcpListener::bind(("127.0.0.1", port))?,
            None => TcpListener::bind(("127.0.0.1", DEFAULT_PORT))
                .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))?,
        };
        let port = listener.local_addr()?.port();
        let pages = Arc::new(pages(port));
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::Builder::new()
            .name("catpaw-local".to_string())
            .spawn(move || serve(listener, &pages, &stopping))?;
        Ok(Self {
            port,
            stop,
            thread: Some(thread),
        })
    }

    #[cfg(test)]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The address of a page of this server.
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so that it sees the flag.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(listener: TcpListener, pages: &Arc<Pages>, stop: &AtomicBool) {
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            let _ = respond(
                stream,
                "503 Service Unavailable",
                "text/plain",
                &[],
                "busy\n",
            );
            continue;
        }
        active.fetch_add(1, Ordering::SeqCst);
        let pages = pages.clone();
        let done = active.clone();
        let spawned = std::thread::Builder::new()
            .name("catpaw-local-conn".to_string())
            .spawn(move || {
                let _ = handle(stream, &pages);
                done.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

fn handle(stream: TcpStream, pages: &Pages) -> std::io::Result<()> {
    let request = match read_request(&stream)? {
        Ok(request) => request,
        Err(status) => return refuse_unread(stream, status),
    };
    let hosts = pages.hosts();
    // Another name for this address (DNS rebinding) is refused.
    if !request
        .header("host")
        .is_some_and(|h| hosts.iter().any(|o| o == h))
    {
        return respond(
            stream,
            "421 Misdirected Request",
            "text/plain",
            &[],
            "wrong host\n",
        );
    }
    // A request another site's page makes is not the user's.
    if request.method != "GET"
        && let Some(origin) = request.header("origin")
        && !hosts.iter().any(|o| origin == format!("http://{o}"))
    {
        return respond(stream, "403 Forbidden", "text/plain", &[], "wrong origin\n");
    }
    let path = request.path.split('?').next().unwrap_or("").to_string();
    if path == "/session" {
        return pages.session_token(stream, &request);
    }
    if let Some(rest) = path.strip_prefix("/confirm/") {
        return crate::confirm::handle(stream, &request, rest, pages);
    }
    if let Some(rest) = path.strip_prefix("/handoff/") {
        return crate::handoff::handle(stream, &request, rest, pages);
    }
    respond(stream, "404 Not Found", "text/plain", &[], "not found\n")
}
