//! The network, as seen by the page.
//!
//! `catpaw-web` does no I/O itself. The embedder supplies a [`NetHost`];
//! requests started through it complete later, and the event loop delivers
//! each result to the callback registered for it.

use std::time::{Duration, Instant};

use url::Url;

use crate::page::{Cx, PageState};

/// What a request is for. Hosts may use it for prioritisation and policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    /// A frame's document.
    Document,
    Script,
    Style,
    Xhr,
    Fetch,
    /// `navigator.sendBeacon()`: sent in the background, its response ignored.
    Beacon,
    Other,
}

#[derive(Clone, Debug)]
pub struct NetRequest {
    pub method: String,
    pub url: Url,
    /// Extra request headers. The host adds its own (User-Agent, cookies, ...).
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub kind: RequestKind,
    /// The referrer to send, the referrer policy already applied (`None`
    /// for no `Referer` header).
    pub referrer: Option<Url>,
    /// Whether cookies are sent with the request and stored from the
    /// response.
    pub credentials: bool,
    /// Whether the host follows redirects itself. Script-initiated
    /// requests follow them in the page, which checks each hop.
    pub follow_redirects: bool,
    /// Where script started the request (`fetch`, `XMLHttpRequest.send`).
    pub site: Option<catpaw_js::SourceSite>,
}

impl NetRequest {
    pub fn get(url: Url, kind: RequestKind) -> Self {
        Self {
            method: "GET".to_string(),
            url,
            headers: Vec::new(),
            body: None,
            kind,
            referrer: None,
            credentials: true,
            follow_redirects: true,
            site: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NetResponse {
    /// The final URL, after redirects.
    pub url: Url,
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    /// The decoded (content-encoding removed) body.
    pub body: Vec<u8>,
    pub redirected: bool,
}

impl NetResponse {
    /// The first header named `name` (ASCII case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// A network failure (no response): a human-readable reason.
pub type NetResult = Result<NetResponse, String>;

/// Receives the outcome of a request started with [`start_request`].
pub type NetCallback = Box<dyn FnOnce(&mut Cx<'_>, NetResult)>;

/// The embedder's network implementation.
/// What a WebSocket connection reports.
#[derive(Clone, Debug)]
pub enum WsEvent {
    /// The handshake succeeded.
    Open {
        protocol: String,
        extensions: String,
    },
    Text(String),
    Binary(Vec<u8>),
    /// The connection is over. `clean` when a close handshake completed;
    /// `code` 1006 when it did not.
    Close {
        code: u16,
        reason: String,
        clean: bool,
    },
    /// The connection could not be made, or broke.
    Error(String),
}

/// What a page sends on a WebSocket.
#[derive(Clone, Debug)]
pub enum WsOutbound {
    Text(String),
    Binary(Vec<u8>),
    Close { code: Option<u16>, reason: String },
}

pub trait NetHost {
    /// Performs a request and waits for it (parser-blocking scripts,
    /// synchronous XHR).
    fn fetch_blocking(&self, request: NetRequest) -> NetResult;

    /// Starts a request and returns a token identifying it. The result is
    /// later returned by [`NetHost::poll`].
    fn start(&self, request: NetRequest) -> u64;

    /// Returns the requests that have completed since the last call. With a
    /// `wait`, blocks up to that long for at least one to complete.
    fn poll(&self, wait: Option<Duration>) -> Vec<(u64, NetResult)>;

    /// Abandons a request; its result is never delivered.
    fn abort(&self, token: u64);

    /// Number of started requests that have not been delivered or aborted.
    fn inflight(&self) -> usize;

    /// The `Cookie` header value script may see for `url` (`document.cookie`).
    fn cookies_for(&self, url: &Url) -> String;

    /// Stores a cookie set through `document.cookie`.
    fn set_cookie(&self, url: &Url, cookie: &str);

    /// Opens a WebSocket and returns a token for it; its events come from
    /// [`NetHost::poll_sockets`]. A host without sockets returns `None`.
    fn ws_connect(&self, _url: Url, _protocols: Vec<String>, _origin: String) -> Option<u64> {
        None
    }

    fn ws_send(&self, _token: u64, _message: WsOutbound) {}

    /// The socket events since the last call. [`NetHost::poll`] does the
    /// waiting: a socket being opened counts as in flight.
    fn poll_sockets(&self) -> Vec<(u64, WsEvent)> {
        Vec::new()
    }
}

/// The response a `data:` URL stands for
/// (<https://fetch.spec.whatwg.org/#data-urls>), or `None` for any other
/// URL.
pub fn data_url_response(url: &Url) -> Option<NetResult> {
    use base64::Engine as _;
    if url.scheme() != "data" {
        return None;
    }
    let rest = url.path();
    let Some((media, payload)) = rest.split_once(',') else {
        return Some(Err("the data URL has no comma".to_string()));
    };
    let decoded = percent_decode(payload.as_bytes());
    let (media, base64_encoded) = match media.strip_suffix(";base64") {
        Some(media) => (media, true),
        None => (media, false),
    };
    let body = if base64_encoded {
        // Forgiving base64: ASCII whitespace is dropped and padding is optional.
        let compact: Vec<u8> = decoded
            .iter()
            .copied()
            .filter(|b| !b.is_ascii_whitespace())
            .collect();
        let engine = base64::engine::general_purpose::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::general_purpose::GeneralPurposeConfig::new()
                .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
                .with_decode_allow_trailing_bits(true),
        );
        match engine.decode(&compact) {
            Ok(bytes) => bytes,
            Err(_) => return Some(Err("the data URL is not valid base64".to_string())),
        }
    } else {
        decoded
    };
    let mut content_type = media.trim().to_string();
    if content_type.is_empty() {
        content_type = "text/plain;charset=US-ASCII".to_string();
    } else if content_type.starts_with(';') {
        content_type = format!("text/plain{content_type}");
    }
    Some(Ok(NetResponse {
        url: url.clone(),
        status: 200,
        status_text: "OK".to_string(),
        headers: vec![("content-type".to_string(), content_type)],
        body,
        redirected: false,
    }))
}

fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
        if input[i] == b'%'
            && i + 2 < input.len()
            && let (Some(hi), Some(lo)) = (hex(input[i + 1]), hex(input[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
}

/// A response the page answers itself: a `data:` URL, or a `blob:` URL
/// the page made.
pub fn local_response(page: &PageState, url: &Url) -> Option<NetResult> {
    if url.scheme() == "blob" {
        return Some(match page.blob_urls.borrow().get(url.as_str()) {
            Some((bytes, type_)) => Ok(NetResponse {
                url: url.clone(),
                status: 200,
                status_text: "OK".to_string(),
                headers: vec![
                    ("content-type".to_string(), type_),
                    ("content-length".to_string(), bytes.len().to_string()),
                ],
                body: bytes.to_vec(),
                redirected: false,
            }),
            None => Err("the blob URL is not known".to_string()),
        });
    }
    data_url_response(url)
}

/// Fetches `request` on the calling thread.
pub fn fetch_blocking(page: &PageState, request: NetRequest) -> NetResult {
    if let Some(result) = local_response(page, &request.url) {
        return result;
    }
    match page.net() {
        Some(net) => net.fetch_blocking(request),
        None => Err("no network available".to_string()),
    }
}

/// Starts a request whose result is passed to `callback` by the event loop.
/// Without a network the callback receives an error from a task, and no
/// token is returned.
pub fn start_request(
    page: &PageState,
    request: NetRequest,
    callback: impl FnOnce(&mut Cx<'_>, NetResult) + 'static,
) -> Option<u64> {
    if let Some(result) = local_response(page, &request.url) {
        crate::event_loop::queue_task(page, "data URL", move |cx| callback(cx, result));
        return None;
    }
    let Some(net) = page.net() else {
        crate::event_loop::queue_task(page, "network unavailable", move |cx| {
            callback(cx, Err("no network available".to_string()));
        });
        return None;
    };
    let info = crate::settle::RequestInfo {
        method: request.method.clone(),
        url: request.url.clone(),
        kind: request.kind,
        initiator: crate::settle::current_initiator(page),
        site: request.site.clone(),
        virtual_start: page.clock.peek(),
        real_start: Instant::now(),
    };
    let token = net.start(request);
    page.net_started.borrow_mut().insert(token, info);
    page.net_callbacks
        .borrow_mut()
        .insert(token, Box::new(callback));
    Some(token)
}

/// Abandons a request started with [`start_request`].
pub fn abort_request(page: &PageState, token: u64) {
    page.net_started.borrow_mut().remove(&token);
    if page.net_callbacks.borrow_mut().remove(&token).is_some()
        && let Some(net) = page.net()
    {
        net.abort(token);
    }
}

/// Number of requests whose results are still awaited, not counting
/// background ones such as beacons.
pub fn inflight(page: &PageState) -> usize {
    page.net_callbacks
        .borrow()
        .len()
        .saturating_sub(page.background_requests.get())
        + page.sockets.connecting(page)
}

/// How many WebSockets are open: the page may hear from them, but
/// nothing says when.
pub fn open_sockets(page: &PageState) -> usize {
    page.sockets.open(page)
}

/// How long, in real time, an awaited request could still complete before
/// a timer due at `deadline_ms` on the page clock would have fired, had
/// the page clock followed real time since the request started. A virtual
/// clock runs ahead of real time while script reads it in a loop; the
/// event loop waits this long for the network before firing the timer.
/// `None` when no request is in flight or real time has caught up. With
/// a settle policy, only requests it waits for count.
pub fn real_time_before(
    page: &PageState,
    deadline_ms: f64,
    policy: Option<&crate::settle::SettlePolicy>,
) -> Option<Duration> {
    let now = Instant::now();
    let started = page.net_started.borrow();
    started
        .iter()
        .filter(|(token, _)| page.net_callbacks.borrow().contains_key(token))
        .filter(|(_, info)| policy.is_none_or(|p| crate::settle::waits_for(page, p, info)))
        .filter_map(|(_, info)| {
            let would_fire = info.real_start
                + Duration::from_secs_f64((deadline_ms - info.virtual_start).max(0.0) / 1000.0);
            let wait = would_fire.saturating_duration_since(now);
            (!wait.is_zero()).then_some(wait)
        })
        .max()
}

/// Delivers completed requests to their callbacks. Returns how many were
/// delivered.
pub fn deliver(cx: &mut Cx<'_>, wait: Option<Duration>) -> usize {
    let Some(net) = cx.page.net() else {
        return 0;
    };
    let mut delivered = 0;
    let page = cx.page;
    for (token, result) in net.poll(wait) {
        page.net_started.borrow_mut().remove(&token);
        let callback = page.net_callbacks.borrow_mut().remove(&token);
        if let Some(callback) = callback {
            let initiator = crate::settle::Initiator::Task("network");
            crate::settle::with_initiator(page, initiator, || {
                callback(cx, result);
                cx.checkpoint();
            });
            delivered += 1;
        }
    }
    for (token, event) in net.poll_sockets() {
        let initiator = crate::settle::Initiator::Task("socket");
        crate::settle::with_initiator(page, initiator, || {
            crate::websocket::on_event(cx, token, event);
            cx.checkpoint();
        });
        delivered += 1;
    }
    delivered
}
