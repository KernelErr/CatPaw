//! Hand-off: the user takes over a tab in their own browser and gives it
//! back (ADR 0006, decision 15). A page on 127.0.0.1 (see
//! [`crate::local`]) shows the tab and passes the user's clicks, typing
//! and scrolling to it. The page's address carries a one-time token, and
//! driving the tab needs the approval key too, so the agent, which sees
//! the address, cannot drive it itself. The agent learns that the user is
//! done and then sees the page as it is; what the user typed into fields
//! shows masked. For logins, checks that want a person, and anything else
//! the agent should not do or see.

use std::collections::BTreeMap;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use catpaw_engine::GroupCaller;
use serde_json::{Value, json};

use crate::http::{Request, escape, respond, respond_bytes};
use crate::local::Pages;
use crate::tab::{GroupState, HandState};

/// How long a hand-off nobody waits for keeps its page, and how long one
/// given back waits for the agent.
pub(crate) const LIFETIME: Duration = Duration::from_secs(1800);

/// What the user does to the tab.
#[derive(Debug, Clone)]
pub(crate) enum Input {
    /// A click at a point of the viewport, in CSS pixels.
    Click { x: f32, y: f32 },
    /// Text typed into the focused element.
    Text(String),
    /// A key: Enter, Tab, Backspace, an arrow.
    Key(String),
    /// A scroll of the page by `dy` pixels.
    Scroll(f32),
}

/// A tab the page drives, through its group's thread.
#[derive(Clone)]
struct Driver {
    group: GroupCaller<GroupState>,
    tab: u32,
}

impl Driver {
    fn screen(&self) -> Option<Vec<u8>> {
        let tab = self.tab;
        self.group.call(move |g| g.screen(tab)).ok().flatten()
    }

    fn input(&self, input: Input) -> Result<HandState, String> {
        let tab = self.tab;
        self.group
            .call(move |g| g.hand_input(tab, input))
            .map_err(|_| "the tab is gone".to_string())?
    }

    fn state(&self) -> Result<HandState, String> {
        let tab = self.tab;
        self.group
            .call(move |g| g.hand_view(tab))
            .map_err(|_| "the tab is gone".to_string())?
    }

    fn decide(&self, ids: Vec<u64>, allow: bool) -> Result<HandState, String> {
        let tab = self.tab;
        self.group
            .call(move |g| g.hand_decide(tab, &ids, allow))
            .map_err(|_| "the tab is gone".to_string())?
    }
}

/// A handed-over tab's state as the hand-off page reads it.
fn state_json(state: Result<HandState, String>) -> Value {
    match state {
        Ok(state) => json!({
            "url": state.url,
            "title": state.title,
            "held": state
                .held
                .iter()
                .map(|(id, what)| json!({"id": id, "what": what}))
                .collect::<Vec<_>>(),
        }),
        Err(error) => json!({"error": error}),
    }
}

struct Handoff {
    tab: u32,
    reason: String,
    token: String,
    /// When the user gave the tab back.
    done: Option<Instant>,
    started: Instant,
    driver: Driver,
}

impl Handoff {
    /// Over without the agent: the user kept the tab past the lifetime,
    /// or gave it back that long ago.
    fn lapsed(&self) -> bool {
        self.done.unwrap_or(self.started).elapsed() >= LIFETIME
    }
}

/// Where a hand-off stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Standing {
    WithUser(u32),
    GivenBack(u32),
    /// The user did not give the tab back within the lifetime.
    Lapsed(u32),
}

#[derive(Default)]
pub(crate) struct Store {
    next: u32,
    items: BTreeMap<u32, Handoff>,
}

/// The hand-offs, shared with the viewer.
pub(crate) type SharedStore = Arc<Mutex<Store>>;

fn lock(store: &SharedStore) -> std::sync::MutexGuard<'_, Store> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
pub struct Handoffs {
    store: SharedStore,
}

impl Store {
    /// Forgets the hand-offs that are over without the agent.
    fn prune(&mut self) {
        self.items.retain(|_, h| !h.lapsed());
    }
}

impl Handoffs {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn shared(&self) -> SharedStore {
        self.store.clone()
    }

    /// Opens hand-off `hN` of `tab` (in place of one the tab had); its id
    /// and the path of its page (with the token).
    pub(crate) fn start(
        &mut self,
        tab: u32,
        reason: &str,
        group: GroupCaller<GroupState>,
    ) -> std::io::Result<(u32, String)> {
        self.close_for_tab(tab);
        let token = crate::local::random_hex(16)?;
        let mut store = lock(&self.store);
        store.prune();
        store.next += 1;
        let id = store.next;
        store.items.insert(
            id,
            Handoff {
                tab,
                reason: reason.to_string(),
                token: token.clone(),
                done: None,
                started: Instant::now(),
                driver: Driver { group, tab },
            },
        );
        Ok((id, format!("/handoff/h{id}?t={token}")))
    }

    /// Where hand-off `hN` stands; one that lapsed is forgotten once said.
    pub(crate) fn state(&self, id: u32) -> Option<Standing> {
        let mut store = lock(&self.store);
        let handoff = store.items.get(&id)?;
        let standing = match handoff.done {
            Some(_) => Standing::GivenBack(handoff.tab),
            None if handoff.lapsed() => Standing::Lapsed(handoff.tab),
            None => Standing::WithUser(handoff.tab),
        };
        if handoff.lapsed() {
            store.items.remove(&id);
        }
        Some(standing)
    }

    /// The latest hand-off not waited for yet (given back or not), of
    /// `tab` when there is one.
    pub fn open(&self, tab: Option<u32>) -> Option<u32> {
        let store = lock(&self.store);
        let store = &*store;
        tab.and_then(|tab| {
            store
                .items
                .iter()
                .rev()
                .find(|(_, h)| h.tab == tab)
                .map(|(id, _)| *id)
        })
        .or_else(|| store.items.keys().next_back().copied())
    }

    /// The hand-off `tab` is with (not given back yet), if any.
    pub fn with_user(&self, tab: u32) -> Option<u32> {
        lock(&self.store)
            .items
            .iter()
            .find(|(_, h)| h.tab == tab && h.done.is_none() && !h.lapsed())
            .map(|(id, _)| *id)
    }

    /// Forgets a hand-off: its page stops answering.
    pub fn close(&self, id: u32) {
        lock(&self.store).items.remove(&id);
    }

    /// Forgets the hand-offs of a tab (it closed, or gets a new one).
    pub fn close_for_tab(&self, tab: u32) {
        lock(&self.store).items.retain(|_, h| h.tab != tab);
    }
}

/// What the viewer's script may do: load its own screenshots and post
/// input to its own address.
const VIEWER: &str = "default-src 'none'; img-src blob:; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; form-action 'none'";

/// `/handoff/hN[/screen|/input|/done]?t=<token>`.
pub(crate) fn handle(
    stream: TcpStream,
    request: &Request,
    rest: &str,
    pages: &Pages,
) -> std::io::Result<()> {
    let query = request.path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let token = url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == "t")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    let Some(rest) = rest.strip_prefix('h') else {
        return respond(stream, "404 Not Found", "text/plain", &[], "not found\n");
    };
    let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
    let entry = id.parse::<u32>().ok().and_then(|id| {
        lock(&pages.handoffs)
            .items
            .get(&id)
            .filter(|h| !h.lapsed())
            .map(|h| {
                (
                    id,
                    h.token.clone(),
                    h.reason.clone(),
                    h.tab,
                    h.done.is_some(),
                    h.driver.clone(),
                )
            })
    });
    let Some((id, expected, reason, tab, done, driver)) = entry else {
        return respond(
            stream,
            "404 Not Found",
            "text/plain",
            &[],
            "this hand-off is over\n",
        );
    };
    if token.len() != expected.len() || token != expected {
        return respond(stream, "403 Forbidden", "text/plain", &[], "wrong link\n");
    }
    // The page itself is only a shell; what drives or shows the tab needs
    // the approval key.
    if !action.is_empty() && !pages.key_given(request) {
        return respond(
            stream,
            "403 Forbidden",
            "application/json",
            &[],
            "{\"error\":\"wrong key\"}\n",
        );
    }
    match (request.method.as_str(), action) {
        ("GET", "") => {
            let body = viewer(
                id,
                tab,
                &reason,
                done,
                &pages.key_file.display().to_string(),
            );
            respond_bytes(
                stream,
                "200 OK",
                "text/html; charset=utf-8",
                VIEWER,
                &[],
                body.as_bytes(),
            )
        }
        ("GET", "screen") => match driver.screen() {
            Some(png) => respond_bytes(stream, "200 OK", "image/png", VIEWER, &[], &png),
            None => respond(stream, "410 Gone", "text/plain", &[], "the tab is closed\n"),
        },
        ("POST", "input") if !done => {
            let input: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let number = |key: &str| input[key].as_f64().unwrap_or(0.0) as f32;
            let input = match input["kind"].as_str() {
                Some("click") => Input::Click {
                    x: number("x"),
                    y: number("y"),
                },
                Some("text") => Input::Text(input["text"].as_str().unwrap_or("").to_string()),
                Some("key") => Input::Key(input["key"].as_str().unwrap_or("").to_string()),
                Some("scroll") => Input::Scroll(number("dy")),
                _ => {
                    return respond(
                        stream,
                        "400 Bad Request",
                        "text/plain",
                        &[],
                        "unknown input\n",
                    );
                }
            };
            let body = state_json(driver.input(input));
            respond(stream, "200 OK", "application/json", &[], &body.to_string())
        }
        ("GET", "state") => {
            let body = state_json(driver.state());
            respond(stream, "200 OK", "application/json", &[], &body.to_string())
        }
        // The user lets go, or drops, what the page holds.
        ("POST", "decide") if !done => {
            let decision: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let ids: Vec<u64> = decision["ids"]
                .as_array()
                .map(|ids| ids.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default();
            let allow = decision["allow"] == json!(true);
            pages.journal(
                "handoff-decision",
                json!({"id": format!("h{id}"), "holds": ids, "allowed": allow}),
            );
            let body = state_json(driver.decide(ids, allow));
            respond(stream, "200 OK", "application/json", &[], &body.to_string())
        }
        ("POST", "done") => {
            if let Some(handoff) = lock(&pages.handoffs).items.get_mut(&id) {
                handoff.done.get_or_insert_with(Instant::now);
            }
            pages.journal("handoff-given-back", json!({"id": format!("h{id}")}));
            respond(stream, "200 OK", "application/json", &[], "{\"done\":true}")
        }
        _ => respond(stream, "404 Not Found", "text/plain", &[], "not found\n"),
    }
}

/// The page the user drives the tab from.
fn viewer(id: u32, tab: u32, reason: &str, done: bool, key_file: &str) -> String {
    let status = if done {
        "<p class=note>This tab was given back to the agent.</p>".to_string()
    } else {
        "<p class=note>Click the page to click it, type while it is selected, scroll over it. The agent does not see what you type: fields you typed into show to it masked. Anything the page would send waits above for you to allow it.</p>".to_string()
    };
    format!(
        r#"<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">
<title>CatPaw: take over t{tab}</title>
<style>
:root{{color-scheme:light dark;--line:#8886;--accent:#2f6fde}}
body{{font:15px/1.5 system-ui,sans-serif;margin:0;padding:1rem;max-width:1320px;margin-inline:auto}}
header{{display:flex;flex-wrap:wrap;gap:.5rem 1rem;align-items:center;justify-content:space-between;margin-bottom:.75rem}}
h1{{font-size:1.05rem;margin:0}} .why{{opacity:.8}}
button{{font:inherit;padding:.45rem 1.1rem;border-radius:.4rem;border:1px solid var(--accent);background:var(--accent);color:#fff;cursor:pointer}}
input{{font:inherit;padding:.35rem;width:min(28rem,100%)}}
#where{{font-size:.85rem;opacity:.75;overflow-wrap:anywhere;margin:.25rem 0 .5rem}}
#screen{{display:block;width:100%;height:auto;border:1px solid var(--line);border-radius:.4rem;cursor:pointer;outline-offset:2px}}
#screen:focus{{outline:2px solid var(--accent)}}
.note{{font-size:.85rem;opacity:.75}}
#held{{border:1px solid var(--accent);border-radius:.4rem;padding:.5rem .75rem;margin:.5rem 0}}
#held li{{overflow-wrap:anywhere}} #held .block{{background:transparent;color:inherit}}
</style>
<header><div><h1>Hand-off h{id}: tab t{tab}</h1><div class=why>{reason}</div></div><button id=done>Done, give it back</button></header>
<div id=keyrow hidden><p><label>Approval key <input id=key type=password autocomplete=off></label> <button id=use>Use</button><br><small>From <code>{key_file}</code>. This browser keeps a pass for this session only. <label><input id=remember type=checkbox> Remember the key itself here</label></small></p></div>
<div id=where></div>
<div id=held hidden><p>The page would:</p><ul id=heldlist></ul><button id=allow>Allow</button> <button id=block class=block>Block</button></div>
<img id=screen tabindex=0 alt="The tab the agent handed over">
{status}
<script>
const base = location.pathname, q = location.search;
const img = document.getElementById('screen'), where = document.getElementById('where');
let key = null;
try {{ key = localStorage.getItem('catpaw-approval-key') || localStorage.getItem('catpaw-session-token'); }} catch (e) {{}}
const keyrow = document.getElementById('keyrow');
function askKey() {{ keyrow.hidden = false; }}
function forget() {{
  try {{ localStorage.removeItem('catpaw-approval-key'); localStorage.removeItem('catpaw-session-token'); }} catch (e) {{}}
  key = null;
}}
document.getElementById('use').onclick = async () => {{
  const typed = document.getElementById('key').value.trim();
  try {{
    const r = await fetch('/session', {{method: 'POST', headers: {{'Content-Type': 'application/json'}}, body: JSON.stringify({{key: typed}})}});
    if (!r.ok) {{ askKey(); return; }}
    key = (await r.json()).token;
    localStorage.setItem('catpaw-session-token', key);
    if (document.getElementById('remember').checked) localStorage.setItem('catpaw-approval-key', typed);
  }} catch (e) {{}}
  keyrow.hidden = true;
  refresh();
  state();
}};
if (!key) askKey();
const headers = () => ({{'X-CatPaw-Key': key || '', 'Content-Type': 'application/json'}});
let chain = Promise.resolve(), shown = null, loading = false, heldIds = [];
const held = document.getElementById('held'), heldlist = document.getElementById('heldlist');
function show(s) {{
  if (s.url) where.textContent = (s.title ? s.title + ' · ' : '') + s.url;
  const items = s.held || [];
  heldIds = items.map(h => h.id);
  heldlist.replaceChildren(...items.map(h => {{ const li = document.createElement('li'); li.textContent = h.what; return li; }}));
  held.hidden = items.length === 0;
}}
async function state() {{
  if (!key) return;
  try {{ const r = await fetch(base + '/state' + q, {{cache: 'no-store', headers: headers()}}); if (r.ok) show(await r.json()); }} catch (e) {{}}
}}
function decide(allow) {{
  const ids = heldIds;
  chain = chain.then(async () => {{
    const r = await fetch(base + '/decide' + q, {{method: 'POST', headers: headers(), body: JSON.stringify({{ids, allow}})}});
    if (r.ok) show(await r.json());
    await refresh();
  }}).catch(() => {{}});
}}
document.getElementById('allow').onclick = () => decide(true);
document.getElementById('block').onclick = () => decide(false);
async function refresh() {{
  if (loading || !key) return;
  loading = true;
  try {{
    const r = await fetch(base + '/screen' + q, {{cache: 'no-store', headers: headers()}});
    if (r.status === 403) {{ forget(); askKey(); return; }}
    if (r.ok) {{
      const next = URL.createObjectURL(await r.blob());
      img.onload = () => {{ if (shown) URL.revokeObjectURL(shown); shown = next; }};
      img.src = next;
    }}
  }} catch (e) {{}} finally {{ loading = false; }}
}}
function send(input) {{
  if (!key) {{ askKey(); return; }}
  chain = chain.then(async () => {{
    const r = await fetch(base + '/input' + q, {{method: 'POST', headers: headers(), body: JSON.stringify(input)}});
    if (r.ok) show(await r.json());
    await refresh();
  }}).catch(() => {{}});
}}
img.addEventListener('click', e => {{
  const box = img.getBoundingClientRect();
  send({{kind: 'click', x: (e.clientX - box.left) * img.naturalWidth / box.width, y: (e.clientY - box.top) * img.naturalHeight / box.height}});
  img.focus();
}});
const keys = ['Enter', 'Tab', 'Backspace', 'Delete', 'Escape', 'ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Home', 'End'];
img.addEventListener('keydown', e => {{
  if (e.ctrlKey || e.metaKey || e.altKey) return;
  if (e.key.length === 1) send({{kind: 'text', text: e.key}});
  else if (keys.includes(e.key)) send({{kind: 'key', key: e.key}});
  else return;
  e.preventDefault();
}});
document.addEventListener('paste', e => {{ if (document.activeElement === img) {{ send({{kind: 'text', text: e.clipboardData.getData('text')}}); e.preventDefault(); }} }});
img.addEventListener('wheel', e => {{ send({{kind: 'scroll', dy: e.deltaY}}); e.preventDefault(); }}, {{passive: false}});
document.getElementById('done').addEventListener('click', async () => {{
  if (!key) {{ askKey(); return; }}
  await chain;
  const r = await fetch(base + '/done' + q, {{method: 'POST', headers: headers()}});
  if (r.ok) document.body.innerHTML = '<p class=note>Given back to the agent. You can close this tab.</p>';
}});
refresh();
state();
setInterval(() => {{ refresh(); state(); }}, 1000);
</script></html>"#,
        reason = escape(reason),
        key_file = escape(key_file),
    )
}
