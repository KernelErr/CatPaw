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

/// Serves `pages` by path (the query is ignored), one request per
/// connection.
fn serve(pages: HashMap<&'static str, &'static str>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let target = request
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            let path = target.split('?').next().unwrap_or("/");
            let (status, body) = match pages.get(path) {
                Some(body) => ("200 OK", body.to_string()),
                None => ("404 Not Found", "<p>not found</p>".to_string()),
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
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
        let port = serve(HashMap::from([("/", INDEX), ("/next", NEXT)]));
        let mut config = SessionConfig::default();
        config.options.net.allow_private_network = true;
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
    assert!(typed.contains("[value=\"wool socks\"]"), "{typed}");

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
        checked.contains("checkbox \"In stock\" [checked]"),
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
    assert!(bought.contains("paragraph: bought"), "{bought}");

    let asked = client.ok("click", json!({"target": "css:#ask"}));
    assert!(asked.starts_with("ok click e"), "{asked}");
    assert!(
        asked.contains("! dialog confirm \"Sure?\" → dismissed"),
        "{asked}"
    );
    assert!(asked.contains("paragraph: no"), "{asked}");
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
