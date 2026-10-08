//! The page's network: `catpaw_web::net::NetHost` on top of `catpaw-net`.
//!
//! Requests run on a small tokio runtime owned by the engine; the page
//! thread only ever blocks on it (for parser-blocking scripts) or collects
//! finished requests from a channel.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use bytes::Bytes;
use catpaw_fetch::{FetchedDocument, fetch_document_hop};
use catpaw_net::WsMessage;
use catpaw_net::{NetClient, NetConfig, NetError, RequestOptions};
use catpaw_web::net::{
    NetHost, NetRequest, NetResponse, NetResult, RequestKind, WsEvent, WsOutbound,
};
use http::Method;
use http::header::{ACCEPT, HeaderName, HeaderValue, REFERER};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::AbortHandle;
use url::Url;

use crate::page::Gate;

/// One request the page made, for diagnostics.
#[derive(Clone, Debug)]
pub struct RequestRecord {
    pub method: String,
    pub url: Url,
    pub kind: RequestKind,
    /// The response status, or `None` if the request failed or is pending.
    pub status: Option<u16>,
    /// The start of the request body, when there is one.
    pub body_preview: Option<String>,
    /// Whether an answer (or a failure) came.
    pub finished: bool,
    /// Why the request failed, when it did.
    pub error: Option<String>,
    /// Started by a timer that keeps setting itself again (polling).
    pub polling: bool,
}

/// How much of a request body a record keeps.
const BODY_PREVIEW_BYTES: usize = 4096;

/// What the network tasks report back to the page thread.
enum HostEvent {
    Response(u64, NetResult),
    Socket(u64, WsEvent),
}

/// The network a browsing context's pages share: one runtime and one
/// client, so one cookie jar. `Send` and `Sync`: page threads each build
/// their own [`EngineNet`] over it.
#[derive(Clone)]
pub struct SharedNet {
    runtime: Arc<Runtime>,
    client: Arc<NetClient>,
}

impl SharedNet {
    pub fn new(config: NetConfig) -> Result<Self, NetError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("catpaw-net")
            .enable_all()
            .build()
            .map_err(|e| NetError::Tls(format!("starting the network runtime: {e}")))?;
        // The client spawns connection tasks; create it inside the runtime.
        let client = {
            let _guard = runtime.enter();
            NetClient::new(config)?
        };
        Ok(Self {
            runtime: Arc::new(runtime),
            client: Arc::new(client),
        })
    }

    pub fn client(&self) -> &NetClient {
        &self.client
    }

    /// Runs a future on the network runtime and waits for it. Must not be
    /// called from inside an async context.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }
}

/// The referrer a navigation from `from` to `to` sends, under the default
/// policy (`strict-origin-when-cross-origin`): the whole URL to the same
/// origin, the origin elsewhere, nothing from HTTPS to HTTP or from a
/// document that is not on the web.
pub fn navigation_referrer(from: &Url, to: &Url) -> Option<Url> {
    if !matches!(from.scheme(), "http" | "https") {
        return None;
    }
    if from.scheme() == "https" && to.scheme() == "http" {
        return None;
    }
    let mut referrer = from.clone();
    referrer.set_fragment(None);
    let _ = referrer.set_username("");
    let _ = referrer.set_password(None);
    if from.origin() == to.origin() {
        return Some(referrer);
    }
    referrer.set_path("/");
    referrer.set_query(None);
    Some(referrer)
}

pub struct EngineNet {
    runtime: Arc<Runtime>,
    client: Arc<NetClient>,
    tx: Sender<HostEvent>,
    rx: Receiver<HostEvent>,
    next_token: Cell<u64>,
    /// Requests on their way: the task, the log entry and the kind.
    inflight: RefCell<HashMap<u64, (AbortHandle, usize, RequestKind)>>,
    /// Open sockets: what to send them, and whether the handshake is still
    /// pending (then the socket counts as in flight).
    sockets: RefCell<HashMap<u64, (UnboundedSender<WsOutbound>, bool)>>,
    /// Socket events taken from the channel while polling for responses.
    socket_events: RefCell<Vec<(u64, WsEvent)>>,
    /// Shared with the hosts of the page's frames: one log per page.
    log: Rc<RefCell<Vec<RequestRecord>>>,
    /// Answers already there (from a recording), delivered at the next
    /// poll in the order the requests were made: a replay interleaves
    /// the same way every time.
    ready: RefCell<std::collections::VecDeque<(u64, NetResult)>>,
    /// Decides about the requests script makes; shared with the hosts of
    /// the page's frames and workers.
    gate: Rc<RefCell<Option<Rc<RequestGate>>>>,
    /// Requests the gate held: started for the page, not sent.
    held: RefCell<Vec<HeldRequest>>,
    /// Numbers what the gates hold, navigations and requests alike, across
    /// the page's frames and workers.
    hold_ids: Rc<Cell<u64>>,
    /// Fetch chains (`NetRequest::chain`) let through after approval:
    /// their later hops and the request after a preflight go ungated.
    approved: Rc<RefCell<HashSet<u64>>>,
    /// Requests the gate refused, or that could not wait for approval, for
    /// the page to report.
    refused: RefCell<Vec<RefusedRequest>>,
}

/// The most redirects a document request follows.
const MAX_REDIRECTS: usize = 20;

/// How a document request ended.
pub enum DocumentFetch {
    Loaded(FetchedDocument),
    /// Stopped before requesting a redirect hop the check did not let
    /// through (it said `Hold` or `Deny`): the hop as it would have gone.
    Stopped {
        method: String,
        url: Url,
        body: Option<(String, Vec<u8>)>,
        gate: Gate,
    },
}

/// Decides about a request script makes (`fetch`, `XMLHttpRequest`,
/// `sendBeacon`, a WebSocket) before it is sent.
pub type RequestGate = dyn Fn(&NetRequest) -> Gate;

struct HeldRequest {
    id: u64,
    token: u64,
    request: NetRequest,
    index: usize,
}

/// Judges a redirect hop of a document fetch: its method, URL and body.
pub type HopCheck<'a> = dyn FnMut(&str, &Url, Option<&(String, Vec<u8>)>) -> Gate + 'a;

/// A request the gate holds (see [`EngineNet::held_requests`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldRequestInfo {
    /// Its hold number: what a release or a drop names.
    pub id: u64,
    pub method: String,
    pub url: Url,
}

/// A request that was not sent: the gate refused it, or it needed an
/// approval it could not wait for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusedRequest {
    pub method: String,
    pub url: Url,
    pub reason: String,
}

/// A header of a request, by name.
fn header<'a>(request: &'a NetRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

async fn perform(client: &NetClient, request: NetRequest) -> NetResult {
    let method = Method::from_bytes(request.method.as_bytes())
        .map_err(|_| format!("invalid method `{}`", request.method))?;
    let mut options = RequestOptions::default();
    // Subresource requests accept anything unless the page says otherwise.
    options
        .headers
        .insert(ACCEPT, HeaderValue::from_static("*/*"));
    // The page decided the referrer (its policy applied): it goes as is.
    if let Some(referrer) = &request.referrer
        && let Ok(value) = HeaderValue::from_str(referrer.as_str())
    {
        options.headers.insert(REFERER, value);
    }
    for (name, value) in &request.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            options.headers.insert(name, value);
        }
    }
    options.body = request.body.map(Bytes::from);
    options.credentials = request.credentials;
    options.follow_redirects = request.follow_redirects;

    let response = client
        .request(method, &request.url, options)
        .await
        .map_err(|e| e.to_string())?;
    Ok(NetResponse {
        status: response.status.as_u16(),
        status_text: response
            .status
            .canonical_reason()
            .unwrap_or_default()
            .to_string(),
        headers: response
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect(),
        redirected: !response.redirect_chain.is_empty(),
        body: response.body.to_vec(),
        url: response.url,
    })
}

impl EngineNet {
    pub fn new(config: NetConfig) -> Result<Self, NetError> {
        Ok(Self::from_shared(&SharedNet::new(config)?))
    }

    /// A host over a context's shared network, with its own request log.
    pub fn from_shared(net: &SharedNet) -> Self {
        let (tx, rx) = channel();
        Self {
            runtime: net.runtime.clone(),
            client: net.client.clone(),
            tx,
            rx,
            next_token: Cell::new(1),
            inflight: RefCell::new(HashMap::new()),
            sockets: RefCell::new(HashMap::new()),
            socket_events: RefCell::new(Vec::new()),
            log: Rc::new(RefCell::new(Vec::new())),
            ready: RefCell::new(std::collections::VecDeque::new()),
            gate: Rc::new(RefCell::new(None)),
            held: RefCell::new(Vec::new()),
            hold_ids: Rc::new(Cell::new(0)),
            approved: Rc::new(RefCell::new(HashSet::new())),
            refused: RefCell::new(Vec::new()),
        }
    }

    /// The shared network this host runs on.
    pub fn shared(&self) -> SharedNet {
        SharedNet {
            runtime: self.runtime.clone(),
            client: self.client.clone(),
        }
    }

    /// Fetches a document (a navigation or a frame's), following its
    /// redirects, and logs each hop with the page's other requests.
    pub fn fetch_document(
        &self,
        method: &str,
        url: &Url,
        body: Option<(String, Vec<u8>)>,
        referrer: Option<&Url>,
    ) -> Result<FetchedDocument, NetError> {
        match self
            .fetch_document_checked(method, url, body, referrer, &mut |_, _, _| Gate::Allow)?
        {
            DocumentFetch::Loaded(fetched) => Ok(fetched),
            DocumentFetch::Stopped { .. } => unreachable!("every hop is allowed"),
        }
    }

    /// Fetches a document one hop at a time: before a redirect is
    /// followed, `check` judges the next hop (method, URL, body) as it
    /// would a navigation — the first hop is the caller's to judge. Each
    /// hop is logged with the page's other requests.
    pub fn fetch_document_checked(
        &self,
        method: &str,
        url: &Url,
        body: Option<(String, Vec<u8>)>,
        referrer: Option<&Url>,
        check: &mut HopCheck<'_>,
    ) -> Result<DocumentFetch, NetError> {
        let mut method = method.to_string();
        let mut url = url.clone();
        let mut body = body;
        let mut chain = Vec::new();
        for _ in 0..=MAX_REDIRECTS {
            let hop_referrer = referrer.and_then(|from| navigation_referrer(from, &url));
            let result = self.block_on(fetch_document_hop(
                &self.client,
                &method,
                &url,
                body.clone(),
                hop_referrer.as_ref(),
            ));
            self.record_document(
                &method,
                &url,
                result.as_ref().ok().map(|d| d.response.status.as_u16()),
            );
            let mut fetched = result?;
            let status = fetched.response.status.as_u16();
            let next = fetched
                .response
                .headers
                .get("location")
                .and_then(|v| v.to_str().ok())
                .and_then(|location| url.join(location).ok())
                .filter(|_| matches!(status, 301 | 302 | 303 | 307 | 308));
            if let Some(next) = next {
                // 303, and 301/302 after a POST, go on as GET without a body.
                let to_get = (status == 303 && method != "GET" && method != "HEAD")
                    || (matches!(status, 301 | 302) && method == "POST");
                let (next_method, next_body) = if to_get {
                    ("GET".to_string(), None)
                } else {
                    (method.clone(), body.clone())
                };
                match check(&next_method, &next, next_body.as_ref()) {
                    Gate::Allow => {}
                    gate => {
                        return Ok(DocumentFetch::Stopped {
                            method: next_method,
                            url: next,
                            body: next_body,
                            gate,
                        });
                    }
                }
                chain.push(url);
                url = next;
                method = next_method;
                body = next_body;
                continue;
            }
            fetched.response.redirect_chain = chain;
            return Ok(DocumentFetch::Loaded(fetched));
        }
        Err(NetError::TooManyRedirects(MAX_REDIRECTS))
    }

    /// A host for another frame of the same page: the same runtime, client
    /// (cookies included) and request log, with requests of its own.
    pub fn child(&self) -> Self {
        let (tx, rx) = channel();
        Self {
            runtime: self.runtime.clone(),
            client: self.client.clone(),
            tx,
            rx,
            next_token: Cell::new(1),
            inflight: RefCell::new(HashMap::new()),
            sockets: RefCell::new(HashMap::new()),
            socket_events: RefCell::new(Vec::new()),
            log: self.log.clone(),
            ready: RefCell::new(std::collections::VecDeque::new()),
            gate: self.gate.clone(),
            held: RefCell::new(Vec::new()),
            hold_ids: self.hold_ids.clone(),
            approved: self.approved.clone(),
            refused: RefCell::new(Vec::new()),
        }
    }

    pub fn client(&self) -> &NetClient {
        &self.client
    }

    /// Lets `gate` decide about the requests script makes, here and in
    /// the hosts of the page's frames and workers (`None`: all go).
    pub fn set_request_gate(&self, gate: Option<Rc<RequestGate>>) {
        *self.gate.borrow_mut() = gate;
    }

    /// A new hold number, page-wide.
    pub(crate) fn next_hold_id(&self) -> u64 {
        let id = self.hold_ids.get() + 1;
        self.hold_ids.set(id);
        id
    }

    /// The number the next hold gets: holds numbered from here on are
    /// newer than now.
    pub fn hold_watermark(&self) -> u64 {
        self.hold_ids.get() + 1
    }

    /// The requests the gate holds here.
    pub fn held_requests(&self) -> Vec<HeldRequestInfo> {
        self.held
            .borrow()
            .iter()
            .map(|h| HeldRequestInfo {
                id: h.id,
                method: h.request.method.clone(),
                url: h.request.url.clone(),
            })
            .collect()
    }

    fn take_held(&self, ids: &[u64]) -> Vec<HeldRequest> {
        let mut held = self.held.borrow_mut();
        let (taken, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut *held)
            .into_iter()
            .partition(|h| ids.contains(&h.id));
        *held = kept;
        taken
    }

    /// Sends the held requests named, as though the gate had let them
    /// through, with the rest of their fetch (redirect hops, the request
    /// after a preflight). Returns how many were held here.
    pub fn release_held(&self, ids: &[u64]) -> usize {
        let taken = self.take_held(ids);
        let count = taken.len();
        for held in taken {
            if held.request.chain != 0 {
                self.approved.borrow_mut().insert(held.request.chain);
            }
            self.dispatch(held.token, held.request, held.index);
        }
        count
    }

    /// Fails the held requests named, as a network that refused them
    /// would. Returns how many were held here.
    pub fn drop_held(&self, ids: &[u64]) -> usize {
        let taken = self.take_held(ids);
        let count = taken.len();
        for held in taken {
            let result: NetResult = Err("the request was not allowed".to_string());
            self.finish(held.index, &result);
            self.ready.borrow_mut().push_back((held.token, result));
        }
        count
    }

    /// The requests refused since the last call.
    pub fn take_refused(&self) -> Vec<RefusedRequest> {
        std::mem::take(&mut *self.refused.borrow_mut())
    }

    fn refuse(&self, request: &NetRequest, reason: &str) {
        self.refused.borrow_mut().push(RefusedRequest {
            method: request.method.clone(),
            url: request.url.clone(),
            reason: reason.to_string(),
        });
    }

    /// What the gate says about a request script makes; `None` when it
    /// need not be asked. A preflight is judged as the request it
    /// prepares, and the later hops of an approved fetch go ungated.
    fn judge(&self, request: &NetRequest) -> Option<Gate> {
        if request.chain != 0 && self.approved.borrow().contains(&request.chain) {
            return None;
        }
        let gate = self.gate.borrow().clone()?;
        if request.method.eq_ignore_ascii_case("OPTIONS")
            && let Some(method) = header(request, "access-control-request-method")
        {
            let prepared = NetRequest {
                method: method.to_ascii_uppercase(),
                ..request.clone()
            };
            return Some(gate(&prepared));
        }
        Some(gate(request))
    }

    /// The document that made the requests is gone: what it holds fails
    /// unsent, what it has on the way is abandoned (beacons excepted, as
    /// browsers let them finish), and its sockets close. Returns the hold
    /// numbers it dropped.
    pub fn leave_document(&self) -> Vec<u64> {
        let held = std::mem::take(&mut *self.held.borrow_mut());
        let dropped = held.iter().map(|h| h.id).collect();
        for held in held {
            self.finish(
                held.index,
                &Err("the page moved on before the request was allowed".to_string()),
            );
        }
        let aborted: Vec<(u64, usize)> = {
            let mut inflight = self.inflight.borrow_mut();
            let gone: Vec<u64> = inflight
                .iter()
                .filter(|(_, (_, _, kind))| *kind != RequestKind::Beacon)
                .map(|(token, _)| *token)
                .collect();
            gone.into_iter()
                .filter_map(|token| {
                    inflight.remove(&token).map(|(task, index, _)| {
                        task.abort();
                        (token, index)
                    })
                })
                .collect()
        };
        for (_, index) in aborted {
            self.finish(index, &Err("aborted: the page moved on".to_string()));
        }
        self.ready.borrow_mut().clear();
        for (_, (out, _)) in self.sockets.borrow_mut().drain() {
            let _ = out.send(WsOutbound::Close {
                code: Some(1001),
                reason: "going away".to_string(),
            });
        }
        self.socket_events.borrow_mut().clear();
        dropped
    }

    /// Sends a request started for the page.
    fn dispatch(&self, token: u64, request: NetRequest, index: usize) {
        if self.client.is_replaying() {
            let result = self.runtime.block_on(perform(&self.client, request));
            self.finish(index, &result);
            self.ready.borrow_mut().push_back((token, result));
            return;
        }
        let client = self.client.clone();
        let tx = self.tx.clone();
        let kind = request.kind;
        let task = self.runtime.spawn(async move {
            let result = perform(&client, request).await;
            let _ = tx.send(HostEvent::Response(token, result));
        });
        self.inflight
            .borrow_mut()
            .insert(token, (task.abort_handle(), index, kind));
    }

    /// Runs a future on the network runtime and waits for it. Must not be
    /// called from inside an async context.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    /// Every request made through this host so far.
    pub fn requests(&self) -> Vec<RequestRecord> {
        self.log.borrow().clone()
    }

    /// How many requests the log holds.
    pub fn requests_len(&self) -> usize {
        self.log.borrow().len()
    }

    /// The requests from index `from` on.
    pub fn requests_since(&self, from: usize) -> Vec<RequestRecord> {
        self.log.borrow().iter().skip(from).cloned().collect()
    }

    /// Logs a document fetch made outside the host (a frame's document).
    pub(crate) fn record_document(&self, method: &str, url: &Url, status: Option<u16>) {
        self.log.borrow_mut().push(RequestRecord {
            method: method.to_string(),
            url: url.clone(),
            kind: RequestKind::Document,
            status,
            body_preview: None,
            finished: true,
            error: None,
            polling: false,
        });
    }

    fn record(&self, request: &NetRequest) -> usize {
        let mut log = self.log.borrow_mut();
        log.push(RequestRecord {
            method: request.method.clone(),
            url: request.url.clone(),
            kind: request.kind,
            status: None,
            body_preview: request.body.as_ref().map(|body| {
                let end = body.len().min(BODY_PREVIEW_BYTES);
                String::from_utf8_lossy(&body[..end]).into_owned()
            }),
            finished: false,
            error: None,
            polling: request.polling,
        });
        log.len() - 1
    }

    fn finish(&self, index: usize, result: &NetResult) {
        if let Some(record) = self.log.borrow_mut().get_mut(index) {
            record.finished = true;
            match result {
                Ok(response) => record.status = Some(response.status),
                Err(e) => record.error = Some(e.clone()),
            }
        }
    }

    /// Sorts an event from the channel: responses go to `out`, socket
    /// events wait for [`NetHost::poll_sockets`].
    fn take(&self, out: &mut Vec<(u64, NetResult)>, event: HostEvent) {
        match event {
            HostEvent::Response(token, result) => self.accept(out, token, result),
            HostEvent::Socket(token, event) => {
                let mut sockets = self.sockets.borrow_mut();
                match &event {
                    WsEvent::Open { .. } => {
                        if let Some(entry) = sockets.get_mut(&token) {
                            entry.1 = false;
                        }
                    }
                    WsEvent::Close { .. } | WsEvent::Error(_) => {
                        sockets.remove(&token);
                    }
                    _ => {}
                }
                self.socket_events.borrow_mut().push((token, event));
            }
        }
    }

    fn accept(&self, out: &mut Vec<(u64, NetResult)>, token: u64, result: NetResult) {
        // A result for an aborted request is dropped.
        if let Some((_, index, _)) = self.inflight.borrow_mut().remove(&token) {
            self.finish(index, &result);
            out.push((token, result));
        }
    }
}

/// Whether script made the request (what the request gate judges).
fn scripted(request: &NetRequest) -> bool {
    matches!(
        request.kind,
        RequestKind::Fetch | RequestKind::Xhr | RequestKind::Beacon
    )
}

impl NetHost for EngineNet {
    fn fetch_blocking(&self, request: NetRequest) -> NetResult {
        // A synchronous request cannot be held: one the gate would hold is
        // refused instead.
        let decision = scripted(&request).then(|| self.judge(&request)).flatten();
        let refusal = match decision {
            Some(Gate::Hold) => Some(
                "it needs the user's approval, which a synchronous request cannot wait for"
                    .to_string(),
            ),
            Some(Gate::Deny(reason)) => Some(reason),
            Some(Gate::Allow) | None => None,
        };
        if let Some(reason) = refusal {
            self.refuse(&request, &reason);
            let index = self.record(&request);
            let result: NetResult = Err(format!("blocked: {reason}"));
            self.finish(index, &result);
            return result;
        }
        let index = self.record(&request);
        let result = self.runtime.block_on(perform(&self.client, request));
        self.finish(index, &result);
        result
    }

    fn start(&self, request: NetRequest) -> u64 {
        let token = self.next_token.get();
        self.next_token.set(token + 1);
        let index = self.record(&request);
        let decision = scripted(&request).then(|| self.judge(&request)).flatten();
        match decision {
            Some(Gate::Hold) => {
                let id = self.next_hold_id();
                self.held.borrow_mut().push(HeldRequest {
                    id,
                    token,
                    request,
                    index,
                });
            }
            Some(Gate::Deny(reason)) => {
                self.refuse(&request, &reason);
                let result: NetResult = Err(format!("blocked: {reason}"));
                self.finish(index, &result);
                self.ready.borrow_mut().push_back((token, result));
            }
            Some(Gate::Allow) | None => self.dispatch(token, request, index),
        }
        token
    }

    fn poll(&self, wait: Option<Duration>) -> Vec<(u64, NetResult)> {
        let mut out: Vec<(u64, NetResult)> = self.ready.borrow_mut().drain(..).collect();
        if !out.is_empty() {
            return out;
        }
        if let Some(wait) = wait
            && (self.inflight() > 0 || !self.sockets.borrow().is_empty())
        {
            match self.rx.recv_timeout(wait) {
                Ok(event) => self.take(&mut out, event),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
            }
        }
        while let Ok(event) = self.rx.try_recv() {
            self.take(&mut out, event);
        }
        out
    }

    fn poll_sockets(&self) -> Vec<(u64, WsEvent)> {
        std::mem::take(&mut *self.socket_events.borrow_mut())
    }

    fn abort(&self, token: u64) {
        self.ready.borrow_mut().retain(|(t, _)| *t != token);
        self.held.borrow_mut().retain(|h| h.token != token);
        if let Some((task, _, _)) = self.inflight.borrow_mut().remove(&token) {
            task.abort();
        }
    }

    fn is_held(&self, token: u64) -> bool {
        self.held.borrow().iter().any(|h| h.token == token)
    }

    fn inflight(&self) -> usize {
        self.inflight.borrow().len()
            + self
                .sockets
                .borrow()
                .values()
                .filter(|(_, connecting)| *connecting)
                .count()
    }

    fn ws_connect(&self, url: Url, protocols: Vec<String>, origin: String) -> Option<u64> {
        // Recordings hold no WebSocket traffic: a replay refuses sockets at
        // once rather than leave one connecting on the real clock.
        if self.client.is_replaying() {
            let index = self.record(&NetRequest::get(url, RequestKind::Other));
            self.finish(
                index,
                &Err("WebSockets are refused while replaying a recording".to_string()),
            );
            return None;
        }
        // A socket cannot be held either: one the gate would hold is
        // refused.
        let gate = self.gate.borrow().clone();
        if let Some(gate) = gate {
            let mut probe = NetRequest::get(url.clone(), RequestKind::Other);
            probe.headers.push(("origin".to_string(), origin.clone()));
            let refusal = match gate(&probe) {
                Gate::Hold => {
                    Some("it needs the user's approval, which a socket cannot wait for".to_string())
                }
                Gate::Deny(reason) => Some(reason),
                Gate::Allow => None,
            };
            if let Some(reason) = refusal {
                self.refuse(&probe, &reason);
                return None;
            }
        }
        let token = self.next_token.get();
        self.next_token.set(token + 1);
        let (out_tx, mut out_rx) = unbounded_channel::<WsOutbound>();
        self.sockets.borrow_mut().insert(token, (out_tx, true));
        let client = self.client.clone();
        let tx = self.tx.clone();
        self.record(&NetRequest::get(url.clone(), RequestKind::Other));
        self.runtime.spawn(async move {
            let mut connection = match client.websocket(&url, &protocols, Some(&origin)).await {
                Ok(connection) => connection,
                Err(e) => {
                    let _ = tx.send(HostEvent::Socket(token, WsEvent::Error(e.to_string())));
                    return;
                }
            };
            let _ = tx.send(HostEvent::Socket(
                token,
                WsEvent::Open {
                    protocol: connection.protocol.clone(),
                    extensions: connection.extensions.clone(),
                },
            ));
            // The close we sent, answered by the peer or by the stream
            // ending.
            let mut we_closed: Option<(u16, String)> = None;
            loop {
                tokio::select! {
                    incoming = connection.next() => match incoming {
                        Some(Ok(WsMessage::Text(text))) => {
                            let _ = tx.send(HostEvent::Socket(token, WsEvent::Text(text)));
                        }
                        Some(Ok(WsMessage::Binary(bytes))) => {
                            let _ = tx.send(HostEvent::Socket(token, WsEvent::Binary(bytes)));
                        }
                        Some(Ok(WsMessage::Close { code, reason })) => {
                            // The stream answers the close frame itself.
                            let _ = tx.send(HostEvent::Socket(token, WsEvent::Close { code, reason, clean: true }));
                            return;
                        }
                        Some(Err(e)) => {
                            let event = match &we_closed {
                                Some((code, reason)) => WsEvent::Close { code: *code, reason: reason.clone(), clean: true },
                                None => WsEvent::Error(e),
                            };
                            let _ = tx.send(HostEvent::Socket(token, event));
                            return;
                        }
                        None => {
                            let event = match &we_closed {
                                Some((code, reason)) => WsEvent::Close { code: *code, reason: reason.clone(), clean: true },
                                None => WsEvent::Close { code: 1006, reason: String::new(), clean: false },
                            };
                            let _ = tx.send(HostEvent::Socket(token, event));
                            return;
                        }
                    },
                    outgoing = out_rx.recv() => match outgoing {
                        Some(WsOutbound::Text(text)) => {
                            if let Err(e) = connection.send(WsMessage::Text(text)).await {
                                let _ = tx.send(HostEvent::Socket(token, WsEvent::Error(e)));
                                return;
                            }
                        }
                        Some(WsOutbound::Binary(bytes)) => {
                            if let Err(e) = connection.send(WsMessage::Binary(bytes)).await {
                                let _ = tx.send(HostEvent::Socket(token, WsEvent::Error(e)));
                                return;
                            }
                        }
                        Some(WsOutbound::Close { code, reason }) => {
                            let code = code.unwrap_or(1000);
                            we_closed = Some((code, reason.clone()));
                            let _ = connection
                                .send(WsMessage::Close { code, reason })
                                .await;
                        }
                        // The page let go of the socket: it is dropped.
                        None => return,
                    },
                }
            }
        });
        Some(token)
    }

    fn ws_send(&self, token: u64, message: WsOutbound) {
        if let Some((sender, _)) = self.sockets.borrow().get(&token) {
            let _ = sender.send(message);
        }
    }

    fn cookies_for(&self, url: &Url) -> String {
        self.client.cookies().script_header(url)
    }

    fn set_cookie(&self, url: &Url, cookie: &str) {
        self.client.cookies().store_from_script(url, cookie);
    }
}
