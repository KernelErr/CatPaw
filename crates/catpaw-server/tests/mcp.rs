//! The MCP server end to end: JSON-RPC lines in, tool results out, against
//! pages from a small local HTTP server.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;

use catpaw_server::{McpServer, Session, SessionConfig};
use serde_json::{Value, json};

const INDEX: &str = r#"<!doctype html><title>Shop</title>
<main><h1>Shop</h1>
<form action="/next" method="get">
  <label>Search <input name="q"></label>
  <label>Sort <select name="sort"><option value="az">Name (A to Z)</option><option value="lohi">Price (low to high)</option></select></label>
  <label><input type="checkbox" name="stock"> In stock</label>
  <button>Go</button>
</form>
<span id="buy" onclick="document.getElementById('out').textContent='bought'">Buy now</span>
<p id="out">idle</p>
<button id="help" onclick="window.open('/next', '_blank')">Help</button>
<button id="ask" onclick="document.getElementById('out').textContent = confirm('Sure?') ? 'yes' : 'no'">Ask</button>
</main>"#;

const NEXT: &str = r#"<!doctype html><title>Next</title><h1>Next page</h1>
<p id="q"></p><script>document.getElementById('q').textContent = location.search</script>
<a href="/">Home</a>"#;

/// Dynamic loading, requests, dialogs, logs and tables.
const LAB: &str = r#"<!doctype html><title>Lab</title>
<button id=start onclick="document.getElementById('status').textContent = 'Loading...'; setTimeout(() => { document.getElementById('status').textContent = 'Hello World!'; }, 3000)">Start</button>
<p id=status>idle</p>
<button id=fetch onclick="fetch('/slow/data').then(r => r.text()).then(t => { document.getElementById('status').textContent = 'got ' + t; })">Fetch</button>
<button id=hang onclick="fetch('/hang')">Hang</button>
<button id=log onclick="console.error('boom happened')">Log</button>
<button id=ask onclick="document.getElementById('status').textContent = confirm('Sure?') + ' ' + prompt('Name?', 'anon')">Ask</button>
<table><caption>Prices</caption><tr><th>Item<th>Price</tr><tr><td>Socks<td>$5</tr><tr><td>Hat<td>$12</tr></table>
<p>Shipping is free over $50.</p>"#;

/// A page with a frame.
const HOST: &str = r#"<!doctype html><title>Host</title><h1>Checkout</h1>
<iframe src="/inner" title="Payment"></iframe><p id=out>waiting</p>"#;

const INNER: &str = r#"<!doctype html><title>Inner</title>
<button onclick="this.textContent = 'Paid'">Pay now</button>"#;

/// Targets by text, re-rendering, disabled controls and an overlay.
const P2: &str = r#"<!doctype html><title>Phase two</title>
<ul id=list><li>Socks <button onclick="rerender()">Remove</button></li></ul>
<button>Save</button><button>Save draft</button>
<button disabled onclick="document.title='clicked'">Locked</button>
<p id=out>idle</p>
<button id=under onclick="document.getElementById('out').textContent='under'">Under</button>
<div id=overlay style="position:fixed;left:0;top:0;width:100%;height:100%;background:#fff">
  We use cookies <button onclick="document.getElementById('overlay').remove()">Accept all</button>
</div>
<script>
function rerender() {
  // A framework re-rendering the row: a new element, the same text.
  document.getElementById('list').innerHTML = '<li>Socks <button onclick="rerender()">Remove</button></li>';
  document.getElementById('out').textContent = 'rendered ' + (++window.renders || (window.renders = 1));
}
</script>"#;

/// A long page.
const LONG: &str = r##"<!doctype html><title>Long</title><main>
<section><h2>Prose</h2>
<p>One paragraph of prose that goes on for a while, to take some room in the snapshot.</p>
<p>Another paragraph of prose that goes on for a while, to take some room as well.</p>
<p>A third paragraph of prose that goes on for a while, to take some room still.</p>
</section>
<ul id=items></ul></main>
<script>
const ul = document.getElementById('items');
for (let i = 0; i < 60; i++) {
  const li = document.createElement('li');
  li.innerHTML = '<a href="#' + i + '">Item ' + i + '</a>';
  ul.append(li);
}
</script>"##;

/// Serves `pages` by path (the query is ignored), a thread per
/// connection; `/slow…` answers after 300 ms and `/hang…` after 3 s.
fn serve(pages: HashMap<&'static str, &'static str>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let pages = pages.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let target = request
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let path = target.split('?').next().unwrap_or("/").to_string();
                if path.starts_with("/slow") {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
                if path.starts_with("/hang") {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                }
                let (status, body) = match pages.get(path.as_str()) {
                    Some(body) => ("200 OK", body.to_string()),
                    None if path.starts_with("/slow") || path.starts_with("/hang") => {
                        ("200 OK", "data".to_string())
                    }
                    None => ("404 Not Found", "<p>not found</p>".to_string()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            });
        }
    });
    port
}

struct Client {
    server: McpServer,
    next_id: u64,
    base: String,
}

impl Client {
    fn new() -> Self {
        Self::with(|_| {})
    }

    fn with(adjust: impl FnOnce(&mut SessionConfig)) -> Self {
        let port = serve(HashMap::from([
            ("/", INDEX),
            ("/next", NEXT),
            ("/lab", LAB),
            ("/host", HOST),
            ("/inner", INNER),
            ("/p2", P2),
            ("/long", LONG),
        ]));
        let mut config = SessionConfig::default();
        config.options.net.allow_private_network = true;
        adjust(&mut config);
        let session = Session::new(config).unwrap();
        Self {
            server: McpServer::new(session),
            next_id: 1,
            base: format!("http://127.0.0.1:{port}"),
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let reply = self.server.handle_line(&line.to_string()).expect("a reply");
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["id"], id);
        reply
    }

    /// Calls a tool; returns its text and whether it is an error.
    fn call(&mut self, name: &str, args: Value) -> (String, bool) {
        let reply = self.request("tools/call", json!({"name": name, "arguments": args}));
        let result = &reply["result"];
        let text = result["content"][0]["text"].as_str().unwrap().to_string();
        (text, result["isError"] == true)
    }

    fn ok(&mut self, name: &str, args: Value) -> String {
        let (text, error) = self.call(name, args);
        assert!(!error, "{name} failed:\n{text}");
        assert!(text.starts_with("ok "), "{text}");
        text
    }

    fn error(&mut self, name: &str, args: Value) -> String {
        let (text, error) = self.call(name, args);
        assert!(error, "{name} should have failed:\n{text}");
        assert!(text.starts_with("error "), "{text}");
        text
    }
}

/// The ref of the line that contains `needle`.
fn ref_of(text: &str, needle: &str) -> String {
    let line = text
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line with {needle:?} in\n{text}"));
    line.split_whitespace().next().unwrap().to_string()
}

#[test]
fn initialize_lists_tools_and_answers_errors_as_json_rpc() {
    let mut client = Client::new();
    let init = client.request("initialize", json!({"protocolVersion": "2025-03-26"}));
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["result"]["serverInfo"]["name"], "catpaw");
    assert!(
        init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("ref")
    );
    let init = client.request("initialize", json!({"protocolVersion": "1999-01-01"}));
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");

    let notification = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    assert_eq!(client.server.handle_line(&notification.to_string()), None);

    let tools = client.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"navigate") && names.contains(&"click"),
        "{names:?}"
    );

    let missing = client.request("resources/list", json!({}));
    assert_eq!(missing["error"]["code"], -32601);
    let unknown = client.request("tools/call", json!({"name": "teleport"}));
    assert_eq!(unknown["error"]["code"], -32602);
    assert_eq!(client.request("ping", json!({}))["result"], json!({}));
}

#[test]
fn a_form_is_filled_and_submitted_by_ref() {
    let mut client = Client::new();
    let (text, error) = client.call("snapshot", json!({}));
    assert!(error && text.contains("NoTab"), "{text}");

    let url = format!("{}/", client.base);
    let page = client.ok("navigate", json!({"url": url}));
    assert!(page.contains("# s1 tab=t1 doc=d1"), "{page}");
    assert!(page.contains("settled=yes"), "{page}");
    // The label's text is the textbox's name, not a line of its own.
    assert!(!page.contains("text: Search"), "{page}");

    let search = ref_of(&page, "textbox \"Search\"");
    let typed = client.ok("type", json!({"target": search, "text": "wool socks"}));
    assert!(
        typed.contains("~ e3 textbox \"Search\" [value=- → \"wool socks\"]"),
        "{typed}"
    );

    let sort = ref_of(&page, "combobox \"Sort\"");
    let chosen = client.ok(
        "select",
        json!({"target": sort, "option": "price (LOW to high)"}),
    );
    assert!(chosen.contains("← \"Price (low to high)\""), "{chosen}");
    let missing = client.error("select", json!({"target": sort, "option": "Cheapest"}));
    assert!(
        missing.contains("NotFound") && missing.contains("\"Name (A to Z)\""),
        "{missing}"
    );

    let stock = ref_of(&page, "checkbox \"In stock\"");
    let checked = client.ok("act", json!({"kind": "check", "target": stock}));
    assert!(
        checked.contains("~ e5 checkbox \"In stock\" [- → checked]"),
        "{checked}"
    );

    let go = ref_of(&page, "button \"Go\"");
    let submitted = client.ok("click", json!({"target": go}));
    assert!(
        submitted.contains("/next?q=wool+socks&sort=lohi&stock=on (200)"),
        "{submitted}"
    );
    assert!(submitted.contains("doc=d2"), "{submitted}");

    let stale = client.error("click", json!({"target": go}));
    assert!(
        stale.contains("StaleRef") && stale.contains("(navigated away)"),
        "{stale}"
    );

    let back = client.ok("navigate", json!({"go": "back"}));
    assert!(back.starts_with("ok back → "), "{back}");
    assert!(back.contains("doc=d3"), "{back}");
}

#[test]
fn clicks_report_dialogs_and_clickable_generics() {
    let mut client = Client::new();
    let url = format!("{}/", client.base);
    let page = client.ok("navigate", json!({"url": url}));
    let buy = ref_of(&page, "[clickable]: Buy now");
    let bought = client.ok("click", json!({"target": buy}));
    assert!(bought.contains("paragraph: idle → bought"), "{bought}");

    let asked = client.ok("click", json!({"target": "css:#ask"}));
    assert!(asked.starts_with("ok click e"), "{asked}");
    assert!(
        asked.contains("! dialog confirm \"Sure?\" → dismissed"),
        "{asked}"
    );
    assert!(asked.contains("paragraph: bought → no"), "{asked}");
}

#[test]
fn popups_become_tabs() {
    let mut client = Client::new();
    let url = format!("{}/", client.base);
    let page = client.ok("navigate", json!({"url": url}));
    let help = ref_of(&page, "button \"Help\"");
    let opened = client.ok("click", json!({"target": help}));
    assert!(opened.contains("! popup t2 "), "{opened}");

    let list = client.ok("tabs", json!({"op": "list"}));
    assert!(list.contains("t1* "), "{list}");
    assert!(list.contains("\"Next\" (opened by t1)"), "{list}");

    let switched = client.ok("tabs", json!({"op": "switch", "tab": "t2"}));
    assert!(switched.contains("# s1 tab=t2"), "{switched}");
    let title = client.ok("evaluate", json!({"script": "document.title;"}));
    assert_eq!(title, "ok evaluate\nNext");

    let closed = client.ok("tabs", json!({"op": "close"}));
    assert!(closed.contains("! current tab is now t1"), "{closed}");
    let title = client.ok("evaluate", json!({"script": "document.title"}));
    assert_eq!(title, "ok evaluate\nShop");
}

#[test]
fn bad_targets_get_errors_with_advice() {
    let mut client = Client::new();
    let url = format!("{}/", client.base);
    let page = client.ok("navigate", json!({"url": url}));

    let syntax = client.error("click", json!({"target": "Sign in"}));
    assert!(syntax.contains("BadArgument"), "{syntax}");
    assert!(syntax.contains("advice: a target is"), "{syntax}");

    let unknown = client.error("click", json!({"target": "e999"}));
    assert!(
        unknown.contains("NotFound e999 was never shown"),
        "{unknown}"
    );

    let heading = ref_of(&page, "heading \"Shop\"");
    let not_checkbox = client.error("act", json!({"kind": "check", "target": heading}));
    assert!(not_checkbox.contains("NotActionable"), "{not_checkbox}");
    let not_text = client.error("type", json!({"target": heading, "text": "x"}));
    assert!(not_text.contains("does not take this input"), "{not_text}");

    let field = client.error("click", json!({"target": "e1", "button": "right"}));
    assert!(field.contains("unknown field `button`"), "{field}");
}

#[test]
fn evaluate_binds_refs_and_screenshot_returns_an_image() {
    let mut client = Client::new();
    let url = format!("{}/", client.base);
    let page = client.ok("navigate", json!({"url": url}));
    let search = ref_of(&page, "textbox \"Search\"");

    let name = client.ok("evaluate", json!({"script": "el.name", "target": search}));
    assert_eq!(name, "ok evaluate\nq");
    let script = format!("const input = $ref(\"{search}\"); return input.form.action");
    let action = client.ok("evaluate", json!({ "script": script }));
    assert!(action.ends_with("/next"), "{action}");
    let awaited = client.ok(
        "evaluate",
        json!({"script": "new Promise(r => setTimeout(() => r(6 * 7), 300))"}),
    );
    assert_eq!(awaited, "ok evaluate\n42");
    let thrown = client.error("evaluate", json!({"script": "null.x"}));
    assert!(thrown.contains("ScriptError TypeError"), "{thrown}");

    let reply = client.request("tools/call", json!({"name": "screenshot", "arguments": {}}));
    let content = &reply["result"]["content"];
    assert_eq!(content[0]["text"], "ok screenshot t1 viewport 1280x720");
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["mimeType"], "image/png");
    assert!(content[1]["data"].as_str().unwrap().len() > 100);

    let forms = client.ok("read", json!({"view": "forms"}));
    assert!(forms.contains("form e"), "{forms}");
    assert!(forms.contains("text \"Search\" name=q"), "{forms}");
    let none = client.ok("read", json!({"view": "links"}));
    assert_eq!(none, "ok read links\n(nothing to read)");

    let next = format!("{}/next", client.base);
    client.ok("navigate", json!({ "url": next }));
    let links = client.ok("read", json!({"view": "links"}));
    let home = format!("\"Home\" {}/", client.base);
    assert!(links.contains(&home), "{links}");
}

fn lab(client: &mut Client) -> String {
    let url = format!("{}/lab", client.base);
    client.ok("navigate", json!({ "url": url }))
}

#[test]
fn actions_answer_with_diffs_unless_told_otherwise() {
    let mut client = Client::new();
    let page = lab(&mut client);
    assert!(page.starts_with("ok navigate → "), "{page}");
    assert!(page.contains("# s1 tab=t1 doc=d1 url="), "{page}");

    let log = ref_of(&page, "button \"Log\"");
    let none = client.ok("click", json!({"target": log, "snapshot": "none"}));
    assert_eq!(
        none,
        format!("ok click {log} button \"Log\"\n! console error: boom happened")
    );
    let full = client.ok("click", json!({"target": log, "snapshot": "full"}));
    assert!(full.contains("\n# s2 tab=t1 doc=d1 url="), "{full}");
    let same = client.ok("snapshot", json!({"diff": true}));
    assert!(
        same.starts_with("ok snapshot\n# s3 diff-from=s2 "),
        "{same}"
    );
    assert!(same.ends_with("settled=yes no changes"), "{same}");
}

#[test]
fn wait_lets_page_time_pass_and_says_when_nothing_will_come() {
    let mut client = Client::new();
    let page = lab(&mut client);
    let start = ref_of(&page, "button \"Start\"");
    // The three-second timer is not part of the click.
    let clicked = client.ok("click", json!({"target": start}));
    assert!(clicked.contains(": idle → Loading..."), "{clicked}");
    assert!(clicked.contains("settled=yes"), "{clicked}");

    let waited = client.ok("wait", json!({"for": "text", "text": "hello world!"}));
    // The click's quiet window already took 100 ms of the three seconds.
    assert!(
        waited.starts_with("ok wait text \"hello world!\" (2.9s)"),
        "{waited}"
    );
    assert!(waited.contains(": Loading... → Hello World!"), "{waited}");

    let never = client.error("wait", json!({"for": "text", "text": "Goodbye"}));
    assert!(
        never.starts_with(
            "error Timeout text \"Goodbye\": not met, and the page has nothing left to do"
        ),
        "{never}"
    );
    let gone = client.ok("wait", json!({"for": "gone", "text": "Loading"}));
    assert!(gone.starts_with("ok wait gone text \"Loading\""), "{gone}");
    let time = client.ok("wait", json!({"for": "time", "ms": 1500}));
    assert!(time.starts_with("ok wait 1500ms (1.5s)"), "{time}");
}

#[test]
fn dialogs_follow_the_action_options() {
    let mut client = Client::new();
    let page = lab(&mut client);
    let ask = ref_of(&page, "button \"Ask\"");
    let dismissed = client.ok("click", json!({"target": ask}));
    assert!(
        dismissed.contains("! dialog confirm \"Sure?\" → dismissed"),
        "{dismissed}"
    );
    assert!(
        dismissed.contains("! dialog prompt \"Name?\" → dismissed"),
        "{dismissed}"
    );
    assert!(dismissed.contains(": idle → false null"), "{dismissed}");
    let accepted = client.ok("click", json!({"target": ask, "promptText": "catpaw"}));
    assert!(
        accepted.contains("! dialog confirm \"Sure?\" → accepted"),
        "{accepted}"
    );
    assert!(
        accepted.contains("! dialog prompt \"Name?\" → accepted \"catpaw\""),
        "{accepted}"
    );
    assert!(
        accepted.contains(": false null → true catpaw"),
        "{accepted}"
    );
}

#[test]
fn requests_are_reported_and_a_busy_page_says_why() {
    let mut client = Client::with(|config| {
        config.options.limits.wall = std::time::Duration::from_millis(800);
    });
    let page = lab(&mut client);
    let fetch = ref_of(&page, "button \"Fetch\"");
    let fetched = client.ok("click", json!({"target": fetch}));
    assert!(
        fetched.contains("! network GET /slow/data 200"),
        "{fetched}"
    );
    assert!(fetched.contains(": idle → got data"), "{fetched}");

    let hang = ref_of(&page, "button \"Hang\"");
    let busy = client.ok("click", json!({"target": hang}));
    assert!(busy.contains("! network GET /hang pending"), "{busy}");
    assert!(
        busy.contains("! not-settled still busy at the time limit"),
        "{busy}"
    );
    assert!(busy.contains("  pending fetch GET /hang ("), "{busy}");
    assert!(busy.contains("from lab:"), "{busy}");
    assert!(busy.contains("settled=no pending=requests:1"), "{busy}");
}

#[test]
fn logs_list_console_requests_and_events() {
    let mut client = Client::new();
    let page = lab(&mut client);
    let log = ref_of(&page, "button \"Log\"");
    client.ok("click", json!({"target": log}));
    let console = client.ok("logs", json!({"kind": "console", "level": "error"}));
    assert_eq!(console, "ok logs console\nerror: boom happened");
    let since = client.ok("logs", json!({"kind": "console", "since": "s2"}));
    assert_eq!(since, "ok logs console\n(none)");
    let network = client.ok("logs", json!({"kind": "network"}));
    assert!(network.contains("GET /lab 200 (document)"), "{network}");
    let events = client.ok("logs", json!({"kind": "events"}));
    assert!(
        events.contains("navigated GET http://127.0.0.1:"),
        "{events}"
    );
    let old = client.error("logs", json!({"kind": "events", "since": "s99"}));
    assert!(old.contains("s99 is not remembered"), "{old}");
}

#[test]
fn read_finds_text_and_shows_tables_and_html() {
    let mut client = Client::new();
    lab(&mut client);
    let tables = client.ok("read", json!({"view": "tables"}));
    assert!(
        tables.contains("\"Prices\" (3 rows)\n| ref | Item | Price |"),
        "{tables}"
    );
    assert!(tables.contains(" | Hat | $12 |"), "{tables}");
    let found = client.ok("read", json!({"view": "find", "query": "FREE"}));
    assert!(
        found.starts_with("ok read find \"FREE\" (1 match)\n"),
        "{found}"
    );
    assert!(
        found.contains(" paragraph: Shipping is **free** over $50."),
        "{found}"
    );
    let regex = client.ok("read", json!({"view": "find", "query": "/\\$\\d+/"}));
    assert!(regex.contains("(3 matches)"), "{regex}");
    let html = client.ok("read", json!({"view": "html"}));
    assert!(html.contains("<button id=\"start\" onclick="), "{html}");
    let bad = client.error("read", json!({"view": "find", "query": "/(/"}));
    assert!(bad.contains("is not a valid regex"), "{bad}");
}

#[test]
fn frames_show_inside_their_host_and_take_actions() {
    let mut client = Client::new();
    let url = format!("{}/host", client.base);
    let page = client.ok("navigate", json!({ "url": url }));
    let host = ref_of(&page, "iframe \"Payment\"");
    assert!(
        page.contains(&format!("{host} iframe \"Payment\" [frame=f1]\n")),
        "{page}"
    );
    let pay = ref_of(&page, "button \"Pay now\"");
    let line = page.lines().find(|l| l.contains("Pay now")).unwrap();
    assert!(
        line.starts_with("  e"),
        "the frame's lines sit under its host: {page}"
    );
    let paid = client.ok("click", json!({"target": pay}));
    assert!(paid.contains("button \"Pay now\" → \"Paid\""), "{paid}");
}

#[test]
fn targets_by_text_and_role_never_guess() {
    let mut client = Client::new();
    let url = format!("{}/p2", client.base);
    client.ok("navigate", json!({ "url": url }));
    let accepted = client.ok("click", json!({"target": "text:Accept all"}));
    assert!(accepted.starts_with("ok click e"), "{accepted}");
    assert!(accepted.contains("- e"), "the overlay went: {accepted}");
    // "Save" names one button exactly; "Sav" two in part.
    let saved = client.ok("click", json!({"target": "button \"Save\""}));
    assert!(saved.contains("button \"Save\""), "{saved}");
    // A role target takes a name in full, or as the snapshot cut it.
    let inside = client.error("click", json!({"target": "button \"draft\""}));
    assert!(
        inside.starts_with("error NotFound button \"draft\" names nothing in full; in part: e"),
        "{inside}"
    );
    assert!(inside.contains("button \"Save draft\""), "{inside}");
    let cut = client.ok("click", json!({"target": "button \"Save d…\""}));
    assert!(cut.contains("button \"Save draft\""), "{cut}");
    let partial = client.error("click", json!({"target": "text:Sav"}));
    assert!(
        partial.starts_with("error AmbiguousTarget text:Sav matches 2 elements: e"),
        "{partial}"
    );
    let missing = client.error("click", json!({"target": "text:Checkout"}));
    assert!(
        missing.contains("NotFound text:Checkout matches nothing"),
        "{missing}"
    );
}

#[test]
fn a_rerendered_ref_is_followed_and_checks_come_first() {
    let mut client = Client::new();
    let url = format!("{}/p2", client.base);
    let page = client.ok("navigate", json!({ "url": url }));
    let under = ref_of(&page, "button \"Under\"");
    let covered = client.error("click", json!({"target": under}));
    assert!(covered.contains("Occluded"), "{covered}");
    assert!(covered.contains("maybe dismiss it with e"), "{covered}");
    assert!(covered.contains("button \"Accept all\""), "{covered}");
    let forced = client.ok("click", json!({"target": under, "force": true}));
    assert!(forced.contains(": idle → under"), "{forced}");

    let remove = ref_of(&page, "button \"Remove\"");
    let first = client.ok("click", json!({"target": remove, "force": true}));
    assert!(first.contains("(replaces "), "{first}");
    // The old ref is stale, but its replacement is certain.
    let again = client.ok("click", json!({"target": remove, "force": true}));
    assert!(
        again.contains(&format!("({remove} re-rendered → e")),
        "{again}"
    );
    assert!(again.contains(": rendered 1 → rendered 2"), "{again}");

    let locked = ref_of(&page, "button \"Locked\"");
    let refused = client.error("click", json!({"target": locked}));
    assert!(refused.contains("NotActionable"), "{refused}");
    assert!(refused.contains("is disabled"), "{refused}");
    // Forced, the click goes through, and a disabled button ignores it.
    let forced = client.ok("click", json!({"target": locked, "force": true}));
    assert!(forced.ends_with("no changes"), "{forced}");
}

#[test]
fn a_page_over_budget_folds_and_opens_again() {
    let mut client = Client::new();
    let url = format!("{}/long", client.base);
    client.ok("navigate", json!({ "url": url, "snapshot": "none" }));
    let small = client.ok("snapshot", json!({"maxTokens": 200}));
    assert!(small.contains("budget=hit"), "{small}");
    assert!(small.contains("[more=50 nodes after e"), "{small}");
    let list_ref = small
        .lines()
        .find(|l| l.trim_start().contains(" list"))
        .and_then(|l| l.split_whitespace().next())
        .unwrap()
        .to_string();
    let more_line = small.lines().find(|l| l.contains("[more=")).unwrap();
    let after = more_line
        .rsplit("after ")
        .next()
        .unwrap()
        .trim_end_matches(']');
    let rest = client.ok(
        "snapshot",
        json!({"root": list_ref, "after": after, "maxTokens": 2000}),
    );
    assert!(rest.contains("link \"Item 10\""), "{rest}");
    assert!(!rest.contains("link \"Item 9\""), "{rest}");
}
