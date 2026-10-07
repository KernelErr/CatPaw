//! The network, as seen by the page.
//!
//! `catpaw-web` does no I/O itself. The embedder supplies a [`NetHost`];
//! requests started through it complete later, and the event loop delivers
//! each result to the callback registered for it.

use std::time::Duration;

use url::Url;

use crate::page::{Cx, PageState};

/// What a request is for. Hosts may use it for prioritisation and policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
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
    let token = net.start(request);
    page.net_callbacks
        .borrow_mut()
        .insert(token, Box::new(callback));
    Some(token)
}

/// Abandons a request started with [`start_request`].
pub fn abort_request(page: &PageState, token: u64) {
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
}

/// Delivers completed requests to their callbacks. Returns how many were
/// delivered.
pub fn deliver(cx: &mut Cx<'_>, wait: Option<Duration>) -> usize {
    let Some(net) = cx.page.net() else {
        return 0;
    };
    let mut delivered = 0;
    for (token, result) in net.poll(wait) {
        let callback = cx.page.net_callbacks.borrow_mut().remove(&token);
        if let Some(callback) = callback {
            callback(cx, result);
            cx.checkpoint();
            delivered += 1;
        }
    }
    delivered
}
