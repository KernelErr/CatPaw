//! Policies and confirmations end to end: a form submission waits for the
//! user's approval and then goes once; a declined one never goes; uploads
//! ask first; allowed domains block the rest; elicitation approves within
//! the call. Also checkpoints, profiles and the journal.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use catpaw_server::{
    Approval, ApprovalConfig, Host, McpServer, Policy, Preset, Session, SessionConfig,
};
use serde_json::{Value, json};

const ORDER: &str = r#"<!doctype html><title>Order</title>
<form method=post action=/placed>
  <label>Name <input name=name value=Ada></label>
  <label>Card number <input name=card value=4111111111111111></label>
  <button>Place order</button>
</form>
<form method=post enctype=multipart/form-data action=/placed>
  <input type=file name=doc id=doc><button>Send file</button>
</form>
<label>Password <input type=password id=pw></label>"#;

const LOGIN: &str = r#"<!doctype html><title>Sign in</title>
<form method=post action=/welcome>
  <label>User <input id=user name=user></label>
  <label>Password <input id=pass name=pass type=password></label>
  <button>Sign in</button>
</form>"#;

/// Two buttons, each sending a POST to another site (localhost, from a
/// page on 127.0.0.1).
const TWO_POSTS: &str = r#"<!doctype html><title>Two</title>
<script>
function send(name) {
  fetch('http://localhost:' + location.port + '/' + name, {method: 'POST', body: name}).catch(() => {});
}
</script>
<button onclick="send('one')">One</button>
<button onclick="send('two')">Two</button>"#;

type Log = Arc<Mutex<Vec<String>>>;

/// Serves `/order`, answers anything else with a page naming the request,
/// and logs each request as `METHOD /path` plus, for POSTs, the body.
fn serve() -> (u16, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let seen = seen.clone();
            std::thread::spawn(move || {
                let mut data = Vec::new();
                let mut buf = vec![0u8; 65536];
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&data);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if data.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&data).into_owned();
                let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
                let mut words = head.lines().next().unwrap_or("").split_whitespace();
                let method = words.next().unwrap_or("GET").to_string();
                let path = words.next().unwrap_or("/").to_string();
                let mut entry = format!("{method} {path}");
                if method == "POST" {
                    entry.push(' ');
                    entry.push_str(body);
                }
                seen.lock().unwrap().push(entry);
                let page = if path == "/order" {
                    ORDER.to_string()
                } else if path == "/login" {
                    LOGIN.to_string()
                } else if path == "/two-posts" {
                    TWO_POSTS.to_string()
                } else {
                    format!("<!doctype html><title>Done</title><h1>{method} {path}</h1>")
                };
                let response = if path == "/to-elsewhere" {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://localhost:{port}/order\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                        page.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes());
            });
        }
    });
    (port, log)
}

fn posts(log: &Log) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("POST"))
        .cloned()
        .collect()
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("catpaw-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Client {
    server: McpServer,
    next_id: u64,
    base: String,
    log: Log,
    key_file: PathBuf,
}

impl Client {
    fn new(name: &str, adjust: impl FnOnce(&mut SessionConfig)) -> Self {
        let (port, log) = serve();
        let key_file = temp_dir(name).join("approval-key");
        let mut config = SessionConfig::default();
        config.options.net.allow_private_network = true;
        config.approval = ApprovalConfig {
            key_file: Some(key_file.clone()),
            port: Some(0),
            decision_wait: Duration::from_millis(300),
            ..ApprovalConfig::default()
        };
        adjust(&mut config);
        Self {
            server: McpServer::new(Session::new(config).unwrap()),
            next_id: 1,
            base: format!("http://127.0.0.1:{port}"),
            log,
            key_file,
        }
    }

    fn line(&mut self, method: &str, params: Value, host: &mut dyn Host) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let reply = self
            .server
            .handle_line_with(&line.to_string(), host)
            .expect("a reply");
        serde_json::from_str(&reply).unwrap()
    }

    fn call_asking(&mut self, name: &str, args: Value, host: &mut dyn Host) -> String {
        let reply = self.line("tools/call", json!({"name": name, "arguments": args}), host);
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn call(&mut self, name: &str, args: Value) -> String {
        self.call_asking(name, args, &mut Never)
    }

    /// The approval key (the user's, never the agent's).
    fn key(&self) -> String {
        std::fs::read_to_string(&self.key_file)
            .unwrap()
            .trim()
            .to_string()
    }

    /// Decides confirmation `cN` on the approval page, with the key.
    fn decide(&self, text: &str, decision: &str) -> String {
        decide_at(text, &self.key(), decision)
    }
}

/// Decides the confirmation whose approval page `text` names, as the
/// user, with the key.
fn decide_at(text: &str, key: &str, decision: &str) -> String {
    let url = text
        .split_whitespace()
        .find(|w| w.starts_with("http://127.0.0.1:") && w.contains("/confirm/"))
        .expect("an approval URL");
    let rest = url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, path) = rest.split_once('/').unwrap();
    let body = format!("decision={decision}");
    let mut stream = TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
    let request = format!(
        "POST /{path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {key}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).unwrap();
    out
}

/// A host that cannot ask its user.
struct Never;
impl Host for Never {
    fn approve(&mut self, _: &str) -> Approval {
        Approval::Unavailable
    }

    fn pause(&mut self, wait: Duration) -> bool {
        std::thread::sleep(wait);
        false
    }
}

/// A host whose user always answers the same, and what it was asked.
struct Answer(bool, Vec<String>);
impl Host for Answer {
    fn approve(&mut self, message: &str) -> Approval {
        self.1.push(message.to_string());
        if self.0 {
            Approval::Approved
        } else {
            Approval::Declined
        }
    }

    fn pause(&mut self, wait: Duration) -> bool {
        std::thread::sleep(wait);
        false
    }
}

#[test]
fn a_submission_waits_for_approval_then_goes_once() {
    let mut client = Client::new("approve", |_| {});
    let url = format!("{}/order", client.base);
    let page = client.call("navigate", json!({ "url": url }));
    assert!(page.starts_with("ok navigate"), "{page}");

    let asked = client.call("click", json!({"target": "button \"Place order\""}));
    assert!(
        asked.starts_with("needs_confirmation c1: click e"),
        "{asked}"
    );
    assert!(
        asked.contains(&format!(
            "would submit → POST {}/placed (fields: name=Ada, card=***)",
            client.base
        )),
        "{asked}"
    );
    assert!(
        asked.contains("ask the user to approve at http://127.0.0.1:"),
        "{asked}"
    );
    assert!(
        asked.contains(
            "repeat: click {\"confirmation\":\"c1\",\"target\":\"button \\\"Place order\\\"\"}"
        ),
        "{asked}"
    );
    assert!(!asked.contains("no changes"), "{asked}");
    assert!(
        !asked.contains(client.key_file.to_str().unwrap()),
        "the key file stays out"
    );
    assert!(posts(&client.log).is_empty(), "held, not sent");

    let args = json!({"target": "button \"Place order\"", "confirmation": "c1"});
    let pending = client.call("click", args.clone());
    assert!(
        pending.starts_with("needs_confirmation c1 (still pending)"),
        "{pending}"
    );

    let other = client.call(
        "click",
        json!({"target": "css:button", "confirmation": "c1"}),
    );
    assert!(
        other.starts_with("error BadArgument c1 is for another call"),
        "{other}"
    );

    let decided = client.decide(&asked, "approve");
    assert!(decided.contains("\"state\":\"approved\""), "{decided}");
    let done = client.call("click", args.clone());
    assert!(done.starts_with("ok click e"), "{done}");
    assert!(
        done.contains(&format!(
            "(confirmed c1) → {}/placed (POST, 200)",
            client.base
        )),
        "{done}"
    );
    let sent = posts(&client.log);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(
        sent[0].contains("name=Ada&card=4111111111111111"),
        "{sent:?}"
    );

    let again = client.call("click", args);
    assert!(
        again.starts_with("error BadArgument there is no confirmation c1"),
        "{again}"
    );
    assert_eq!(posts(&client.log).len(), 1);
}

#[test]
fn a_declined_submission_never_goes() {
    let mut client = Client::new("decline", |_| {});
    let url = format!("{}/order", client.base);
    client.call("navigate", json!({ "url": url }));
    let asked = client.call("click", json!({"target": "button \"Place order\""}));
    client.decide(&asked, "decline");
    let blocked = client.call(
        "click",
        json!({"target": "button \"Place order\"", "confirmation": "c1"}),
    );
    assert_eq!(blocked, "blocked user: declined c1");
    assert!(posts(&client.log).is_empty());
}

#[test]
fn the_host_can_ask_within_the_call() {
    let mut client = Client::new("elicit", |_| {});
    let init = client.line(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {"elicitation": {}}, "clientInfo": {"name": "t", "version": "0"}}),
        &mut Never,
    );
    assert!(init["result"]["protocolVersion"].is_string());
    let url = format!("{}/order", client.base);
    client.call("navigate", json!({ "url": url }));
    let mut yes = Answer(true, Vec::new());
    let done = client.call_asking(
        "click",
        json!({"target": "button \"Place order\""}),
        &mut yes,
    );
    assert!(done.contains("(confirmed c1) → "), "{done}");
    assert!(
        yes.1[0].starts_with("CatPaw: allow this? click e"),
        "{:?}",
        yes.1
    );
    assert_eq!(posts(&client.log).len(), 1);

    client.call("navigate", json!({ "url": url }));
    let mut no = Answer(false, Vec::new());
    let blocked = client.call_asking(
        "click",
        json!({"target": "button \"Place order\""}),
        &mut no,
    );
    assert_eq!(blocked, "blocked user: declined c2");
    assert_eq!(posts(&client.log).len(), 1);
}

#[test]
fn uploads_ask_first_and_reach_the_server() {
    let dir = temp_dir("upload-files");
    std::fs::write(dir.join("hello.txt"), "hello upload").unwrap();
    let root = dir.clone();
    let mut client = Client::new("upload", move |config| {
        config.files_root = Some(root);
        config.policy = Policy {
            trusted: vec!["127.0.0.1".into()],
            ..Policy::default()
        };
    });
    let url = format!("{}/order", client.base);
    client.call("navigate", json!({ "url": url }));
    let args = json!({"kind": "upload", "target": "css:#doc", "files": ["hello.txt"]});
    let asked = client.call("act", args.clone());
    assert!(
        asked.starts_with("needs_confirmation c1: upload hello.txt (12 B) into e4 button"),
        "{asked}"
    );
    client.decide(&asked, "approve");
    let mut confirmed = args.clone();
    confirmed["confirmation"] = json!("c1");
    let chosen = client.call("act", confirmed);
    assert!(chosen.starts_with("ok upload e"), "{chosen}");
    assert!(chosen.contains("← hello.txt (12 B)"), "{chosen}");
    // The host is trusted: the submission itself needs no approval.
    let sent = client.call("click", json!({"target": "button \"Send file\""}));
    assert!(sent.contains("(POST, 200)"), "{sent}");
    let posted = posts(&client.log);
    assert!(
        posted[0].contains("filename=\"hello.txt\"") && posted[0].contains("hello upload"),
        "{posted:?}"
    );
}

#[test]
fn allowed_domains_keep_tabs_in_bounds() {
    let mut client = Client::new("domains", |config| {
        config.policy = Policy {
            preset: Preset::Open,
            allowed_domains: vec!["127.0.0.1".into()],
            ..Policy::default()
        };
    });
    let elsewhere = client.base.replace("127.0.0.1", "localhost") + "/order";
    let blocked = client.call("navigate", json!({ "url": elsewhere }));
    assert_eq!(
        blocked,
        format!("blocked policy: navigate → {elsewhere} (localhost is not an allowed domain)")
    );
    assert!(client.log.lock().unwrap().is_empty());
    let url = format!("{}/order", client.base);
    let fine = client.call("navigate", json!({ "url": url }));
    assert!(fine.starts_with("ok navigate"), "{fine}");
}

#[test]
fn checkpoints_bring_back_cookies_storage_and_tabs() {
    let mut client = Client::new("checkpoint", |config| {
        config.tools = vec!["session".into()];
    });
    let url = format!("{}/order", client.base);
    client.call("navigate", json!({ "url": url }));
    client.call(
        "evaluate",
        json!({"script": "document.cookie = 'step=one'; localStorage.cart = 'socks'; scrollTo(0, 40)"}),
    );
    let saved = client.call("session", json!({"op": "save", "name": "before"}));
    assert_eq!(saved, "ok session save \"before\" (1 tab)");
    client.call(
        "evaluate",
        json!({"script": "document.cookie = 'step=two'; localStorage.cart = 'hats'"}),
    );
    let listed = client.call("session", json!({"op": "list"}));
    assert_eq!(listed, "ok session list\n\"before\"");
    let restored = client.call("session", json!({"op": "restore", "name": "before"}));
    assert!(
        restored.starts_with(&format!("ok session restore \"before\": t2 {url}")),
        "{restored}"
    );
    let cookie = client.call(
        "evaluate",
        json!({"script": "document.cookie + ' ' + localStorage.cart"}),
    );
    assert!(cookie.ends_with("\nstep=one socks"), "{cookie}");
}

#[test]
fn the_session_tool_is_offered_only_when_asked_for() {
    let mut client = Client::new("tools", |_| {});
    let list = client.line("tools/list", json!({}), &mut Never);
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"session"), "{names:?}");
    let reply = client.line(
        "tools/call",
        json!({"name": "session", "arguments": {"op": "list"}}),
        &mut Never,
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown tool")
    );
}

#[test]
fn a_profile_keeps_cookies_and_a_journal() {
    let profile = temp_dir("profile");
    let dir = profile.clone();
    let mut client = Client::new("profile-1", move |config| config.profile = Some(dir));
    let url = format!("{}/order", client.base);
    client.call("navigate", json!({ "url": url }));
    client.call(
        "evaluate",
        json!({"script": "document.cookie = 'kept=yes; max-age=3600'"}),
    );
    client.call("type", json!({"target": "css:#pw", "text": "hunter2"}));
    drop(client);

    let dir = profile.clone();
    let mut client = Client::new("profile-2", move |config| config.profile = Some(dir));
    let url = format!("{}/order", client.base);
    client.call("navigate", json!({ "url": url }));
    let cookie = client.call("evaluate", json!({"script": "document.cookie"}));
    assert!(cookie.contains("kept=yes"), "{cookie}");

    let journals: Vec<PathBuf> = std::fs::read_dir(profile.join("journal"))
        .unwrap()
        .flatten()
        .map(|e| e.path().join("journal.jsonl"))
        .collect();
    assert_eq!(journals.len(), 2, "{journals:?}");
    let all: String = journals
        .iter()
        .map(|p| std::fs::read_to_string(p).unwrap())
        .collect();
    assert!(all.contains("\"tool\":\"type\""), "{all}");
    assert!(all.contains("(7 characters)"), "{all}");
    assert!(!all.contains("hunter2"), "{all}");
}

/// One request to a hand-off page, with the approval key when given; the
/// status line and the body.
fn handoff_request(
    url: &str,
    key: Option<&str>,
    method: &str,
    suffix: &str,
    body: &str,
) -> (String, Vec<u8>) {
    let rest = url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, path) = rest.split_once('/').unwrap();
    let (path, query) = path.split_once('?').unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
    let key = key
        .map(|k| format!("X-CatPaw-Key: {k}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} /{path}{suffix}?{query} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{key}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).unwrap();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&out[..split]).into_owned();
    (
        head.lines().next().unwrap().to_string(),
        out[split + 4..].to_vec(),
    )
}

#[test]
fn the_user_takes_over_and_gives_back() {
    let mut client = Client::new("handoff", |_| {});
    let url = format!("{}/login", client.base);
    client.call("navigate", json!({ "url": url }));
    let point = |client: &mut Client, id: &str| -> (f64, f64) {
        let text = client.call(
            "evaluate",
            json!({"script": format!("(() => {{ const r = document.getElementById('{id}').getBoundingClientRect(); return (r.x + r.width / 2) + ',' + (r.y + r.height / 2); }})()")}),
        );
        let (x, y) = text.lines().last().unwrap().split_once(',').unwrap();
        (x.parse().unwrap(), y.parse().unwrap())
    };
    let user = point(&mut client, "user");
    let pass = point(&mut client, "pass");

    let started = client.call("handoff", json!({"reason": "Log in to the shop"}));
    assert!(
        started.starts_with("ok handoff h1 t1: ask the user to open http://127.0.0.1:"),
        "{started}"
    );
    assert!(
        started.ends_with(" and log in to the shop; then wait({\"for\":\"handoff\"})"),
        "{started}"
    );
    let link = started
        .split_whitespace()
        .find(|w| w.contains("/handoff/h1?t="))
        .unwrap()
        .to_string();

    // The user's side: the viewer, a screenshot, clicks and typing; all
    // but the viewer itself need the approval key as well as the link.
    let key = client.key();
    let user_side = |method: &str, suffix: &str, body: &str| {
        handoff_request(&link, Some(&key), method, suffix, body)
    };
    let (status, page) = handoff_request(&link, None, "GET", "", "");
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(String::from_utf8_lossy(&page).contains("Hand-off h1: tab t1"));
    assert!(
        !String::from_utf8_lossy(&page).contains(&key),
        "the page never carries the key"
    );
    let (status, png) = user_side("GET", "/screen", "");
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(png.starts_with(b"\x89PNG"));
    let wrong = link.replace("?t=", "?t=0");
    assert!(
        handoff_request(&wrong, Some(&key), "GET", "", "")
            .0
            .contains("403")
    );
    let click = |at: (f64, f64)| json!({"kind": "click", "x": at.0, "y": at.1}).to_string();
    // The link alone does not let anyone act.
    assert!(
        handoff_request(&link, None, "GET", "/screen", "")
            .0
            .contains("403")
    );
    assert!(
        handoff_request(&link, None, "POST", "/input", &click(user))
            .0
            .contains("403")
    );
    user_side("POST", "/input", &click(user));
    user_side(
        "POST",
        "/input",
        &json!({"kind": "text", "text": "ada"}).to_string(),
    );
    user_side("POST", "/input", &click(pass));
    user_side(
        "POST",
        "/input",
        &json!({"kind": "text", "text": "s3cret"}).to_string(),
    );
    let (_, moved) = user_side(
        "POST",
        "/input",
        &json!({"kind": "key", "key": "Enter"}).to_string(),
    );
    let moved = String::from_utf8_lossy(&moved).into_owned();
    assert!(
        moved.contains("/welcome"),
        "the user's own submission goes: {moved}"
    );
    assert_eq!(user_side("POST", "/done", "").0, "HTTP/1.1 200 OK");

    let back = client.call("wait", json!({"for": "handoff"}));
    assert!(
        back.starts_with(&format!(
            "ok wait handoff h1: given back → {}/welcome (POST, 200)",
            client.base
        )),
        "{back}"
    );
    assert!(!back.contains("s3cret"), "{back}");
    let sent = posts(&client.log);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("user=ada&pass=s3cret"), "{sent:?}");
    // Over: the page stops answering, and there is nothing to wait for.
    assert!(
        handoff_request(&link, None, "GET", "", "")
            .0
            .contains("404")
    );
    let none = client.call("wait", json!({"for": "handoff", "timeoutMs": 10}));
    assert!(
        none.starts_with("error BadArgument no hand-off is open"),
        "{none}"
    );
}

fn strict(config: &mut SessionConfig) {
    config.policy = Policy {
        preset: Preset::Strict,
        ..Policy::default()
    };
}

#[test]
fn approving_one_confirmation_lets_go_its_own_holds_only() {
    let mut client = Client::new("own-holds", strict);
    let url = format!("{}/two-posts", client.base);
    client.call("navigate", json!({ "url": url }));
    let one = client.call("click", json!({"target": "button \"One\""}));
    assert!(one.starts_with("needs_confirmation c1: click e"), "{one}");
    let two = client.call("click", json!({"target": "button \"Two\""}));
    assert!(two.starts_with("needs_confirmation c2: click e"), "{two}");
    assert!(posts(&client.log).is_empty(), "both held");

    client.decide(&one, "approve");
    let done = client.call(
        "click",
        json!({"target": "button \"One\"", "confirmation": "c1"}),
    );
    assert!(done.starts_with("ok click e"), "{done}");
    assert_eq!(posts(&client.log), vec!["POST /one one".to_string()]);

    client.decide(&two, "decline");
    let blocked = client.call(
        "click",
        json!({"target": "button \"Two\"", "confirmation": "c2"}),
    );
    assert_eq!(blocked, "blocked user: declined c2");
    assert_eq!(posts(&client.log).len(), 1, "the declined one never goes");
}

#[test]
fn moving_on_voids_what_the_page_held() {
    let mut client = Client::new("moved-on", strict);
    let url = format!("{}/two-posts", client.base);
    client.call("navigate", json!({ "url": url }));
    let asked = client.call("click", json!({"target": "button \"One\""}));
    assert!(asked.starts_with("needs_confirmation c1"), "{asked}");
    let order = format!("{}/order", client.base);
    let moved = client.call("navigate", json!({ "url": order }));
    assert!(
        moved.contains("! c1 no longer applies: the page dropped what it held"),
        "{moved}"
    );
    let decided = client.decide(&asked, "approve");
    assert!(decided.contains("\"changed\":false"), "{decided}");
    let repeated = client.call(
        "click",
        json!({"target": "button \"One\"", "confirmation": "c1"}),
    );
    assert!(
        repeated.starts_with("blocked superseded: c1 no longer applies"),
        "{repeated}"
    );
    assert!(posts(&client.log).is_empty());
}

#[test]
fn a_repeated_call_waits_for_the_decision() {
    let mut client = Client::new("waits", |config| {
        config.approval.decision_wait = Duration::from_secs(30);
    });
    let url = format!("{}/order", client.base);
    client.call("navigate", json!({ "url": url }));
    let asked = client.call("click", json!({"target": "button \"Place order\""}));
    let key = client.key();
    let user = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        decide_at(&asked, &key, "approve")
    });
    let started = std::time::Instant::now();
    let done = client.call(
        "click",
        json!({"target": "button \"Place order\"", "confirmation": "c1"}),
    );
    assert!(done.contains("(confirmed c1) → "), "{done}");
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(user.join().unwrap().contains("\"state\":\"approved\""));
    assert_eq!(posts(&client.log).len(), 1);
}

#[test]
fn an_unanswered_confirmation_runs_out() {
    let mut client = Client::new("runs-out", |config| {
        config.approval.lifetime = Duration::from_millis(300);
    });
    let url = format!("{}/order", client.base);
    client.call("navigate", json!({ "url": url }));
    let asked = client.call("click", json!({"target": "button \"Place order\""}));
    assert!(asked.starts_with("needs_confirmation c1"), "{asked}");
    std::thread::sleep(Duration::from_millis(400));
    let late = client.decide(&asked, "approve");
    assert!(late.contains("\"changed\":false"), "{late}");
    let repeated = client.call(
        "click",
        json!({"target": "button \"Place order\"", "confirmation": "c1"}),
    );
    assert_eq!(repeated, "blocked expired: c1 was not approved in time");
    assert!(posts(&client.log).is_empty());
}

#[test]
fn a_redirect_out_of_the_allowed_domains_is_not_followed() {
    let mut client = Client::new("redirect", |config| {
        config.policy = Policy {
            preset: Preset::Open,
            allowed_domains: vec!["127.0.0.1".into()],
            ..Policy::default()
        };
    });
    let url = format!("{}/to-elsewhere", client.base);
    let blocked = client.call("navigate", json!({ "url": url }));
    assert!(blocked.starts_with("blocked policy: "), "{blocked}");
    assert!(
        blocked.contains("localhost is not an allowed domain"),
        "{blocked}"
    );
    assert_eq!(
        *client.log.lock().unwrap(),
        vec!["GET /to-elsewhere".to_string()],
        "the hop elsewhere is never fetched"
    );
}

#[test]
fn a_handoff_leaves_the_agents_holds_alone() {
    let mut client = Client::new("handoff-holds", |_| {});
    let url = format!("{}/order", client.base);
    client.call("navigate", json!({ "url": url }));
    let asked = client.call("click", json!({"target": "button \"Place order\""}));
    assert!(asked.starts_with("needs_confirmation c1"), "{asked}");
    let started = client.call("handoff", json!({"reason": "Have a look"}));
    let link = started
        .split_whitespace()
        .find(|w| w.contains("/handoff/h1?t="))
        .unwrap()
        .to_string();
    let key = client.key();
    // The user clicks where nothing is and gives the tab back.
    let click = json!({"kind": "click", "x": 2, "y": 2}).to_string();
    handoff_request(&link, Some(&key), "POST", "/input", &click);
    handoff_request(&link, Some(&key), "POST", "/done", "");
    let back = client.call("wait", json!({"for": "handoff"}));
    assert!(back.starts_with("ok wait handoff h1: given back"), "{back}");
    assert!(
        posts(&client.log).is_empty(),
        "what the agent's click held is still the user's to approve"
    );
    client.decide(&asked, "approve");
    let done = client.call(
        "click",
        json!({"target": "button \"Place order\"", "confirmation": "c1"}),
    );
    assert!(done.contains("(confirmed c1) → "), "{done}");
    assert_eq!(posts(&client.log).len(), 1);
}

#[test]
fn a_damaged_checkpoint_leaves_the_session_alone() {
    let profile = temp_dir("damaged-profile");
    let dir = profile.clone();
    let mut client = Client::new("damaged", move |config| {
        config.profile = Some(dir);
        config.tools = vec!["session".into()];
    });
    let url = format!("{}/order", client.base);
    client.call("navigate", json!({ "url": url }));
    let saved = client.call("session", json!({"op": "save", "name": "two words"}));
    assert!(
        saved.starts_with("ok session save \"two words\" (1 tab)"),
        "{saved}"
    );
    assert!(profile.join("checkpoints/two%20words.json").exists());
    std::fs::write(profile.join("checkpoints/broken.json"), "{not json").unwrap();
    let listed = client.call("session", json!({"op": "list"}));
    assert_eq!(listed, "ok session list\n\"broken\"\n\"two words\"");
    let broken = client.call("session", json!({"op": "restore", "name": "broken"}));
    assert!(
        broken.starts_with("error Unsupported checkpoint \"broken\" is damaged"),
        "{broken}"
    );
    let missing = client.call("session", json!({"op": "restore", "name": "nope"}));
    assert!(
        missing.starts_with("error NotFound nothing is saved as \"nope\""),
        "{missing}"
    );
    // The session is as it was.
    let still = client.call("snapshot", json!({}));
    assert!(still.contains("/order title=\"Order\""), "{still}");
}

#[test]
fn the_approval_key_is_never_uploaded() {
    let mut client = Client::new("key-upload", |config| {
        config.policy = Policy {
            preset: Preset::Open,
            ..Policy::default()
        };
    });
    let url = format!("{}/order", client.base);
    client.call("navigate", json!({ "url": url }));
    std::fs::write(&client.key_file, format!("{}\n", "7".repeat(64))).unwrap();
    // An input without `multiple` takes one file, as its picker would.
    let dir = client.key_file.parent().unwrap().to_path_buf();
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    std::fs::write(dir.join("b.txt"), "b").unwrap();
    let two = client.call(
        "act",
        json!({"kind": "upload", "target": "css:#doc", "files": [
            dir.join("a.txt").to_str().unwrap(), dir.join("b.txt").to_str().unwrap()
        ]}),
    );
    assert!(
        two.starts_with("error BadArgument e") && two.contains("takes one file"),
        "{two}"
    );
    let link = client.key_file.parent().unwrap().join("innocent.txt");
    std::fs::hard_link(&client.key_file, &link).unwrap();
    for file in [client.key_file.clone(), link] {
        let refused = client.call(
            "act",
            json!({"kind": "upload", "target": "css:#doc", "files": [file.to_str().unwrap()]}),
        );
        assert!(
            refused.starts_with("error BadArgument that file is CatPaw's approval key"),
            "{refused}"
        );
    }
}
