//! The HTTP client: hyper (HTTP/1.1 and HTTP/2 over ALPN) with rustls,
//! redirects, cookies, content decoding and optional Web Bot Auth signing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{
    ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE,
    COOKIE, HOST, LOCATION, USER_AGENT,
};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use url::Url;

use crate::bot_auth::{BotAuthConfig, BotAuthSigner};
use crate::cookies::CookieJar;
use crate::decode::decode_body;
use crate::policy::{self, FilteringResolver};

/// The default User-Agent. Deployers are expected to set their own, with a
/// contact URL, when they register as a signed agent.
pub const DEFAULT_USER_AGENT: &str = concat!("CatPaw/", env!("CARGO_PKG_VERSION"));

pub const DEFAULT_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
pub const DEFAULT_ACCEPT_ENCODING: &str = "gzip, deflate, br, zstd";

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("unsupported URL scheme `{0}`")]
    UnsupportedScheme(String),
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("request failed: {0}")]
    Transport(#[from] hyper_util::client::legacy::Error),
    #[error("http error: {0}")]
    Http(#[from] http::Error),
    #[error("reading body failed: {0}")]
    Body(#[from] hyper::Error),
    #[error("too many redirects (limit {0})")]
    TooManyRedirects(usize),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("content decoding failed: {0}")]
    Decode(String),
    #[error("TLS configuration failed: {0}")]
    Tls(String),
    #[error("bot auth: {0}")]
    BotAuth(#[from] crate::bot_auth::BotAuthError),
    #[error("the response is larger than the {0} byte limit")]
    TooLarge(usize),
    #[error("refused: {0} (private networks are off by default)")]
    PrivateAddress(String),
    #[error("proxy: {0}")]
    Proxy(String),
    #[error("replay: {0}")]
    Replay(String),
    #[error("replay: no recorded answer for {} {}", .0.method, .0.url)]
    Unrecorded(Box<Unrecorded>),
}

/// The hop a replay has no answer for, as it would be sent: where a
/// request made with [`RequestOptions::replay_only`] stopped.
#[derive(Debug, Clone)]
pub struct Unrecorded {
    pub method: Method,
    pub url: Url,
    pub body: Option<Bytes>,
    /// The URLs that redirected to it, in order.
    pub redirect_chain: Vec<Url>,
}

#[derive(Debug, Clone)]
pub struct NetConfig {
    pub user_agent: String,
    pub accept_language: String,
    pub max_redirects: usize,
    /// Overall budget for one hop (connect, headers, body).
    pub timeout: Duration,
    pub bot_auth: Option<BotAuthConfig>,
    /// The most bytes a response body may have on the wire.
    pub max_response_bytes: usize,
    /// The most bytes a response body may decode to.
    pub max_decoded_bytes: usize,
    /// Whether requests may go to loopback, private and link-local
    /// addresses (off by default: a page must not reach the machine or its
    /// network).
    pub allow_private_network: bool,
    /// An HTTP (`CONNECT`) or SOCKS5 proxy every connection goes through.
    pub proxy: Option<Url>,
    /// Cookies to start with, as [`CookieJar::to_json`] writes them.
    pub cookies_json: Option<String>,
    /// Record the traffic as HAR, or answer from such a recording.
    pub recording: Option<crate::har::Recording>,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            user_agent: DEFAULT_USER_AGENT.to_string(),
            accept_language: "en-US,en;q=0.9".to_string(),
            max_redirects: 20,
            timeout: Duration::from_secs(30),
            bot_auth: None,
            max_response_bytes: 32 * 1024 * 1024,
            max_decoded_bytes: 64 * 1024 * 1024,
            allow_private_network: false,
            proxy: None,
            cookies_json: None,
            recording: None,
        }
    }
}

/// Per-request options.
#[derive(Debug, Clone)]
pub struct RequestOptions {
    /// Extra headers; they override the client defaults.
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
    pub follow_redirects: bool,
    /// Whether to send cookies with the request and store the ones the
    /// response sets.
    pub credentials: bool,
    /// Where the request's entries go in a recording (see
    /// [`NetClient::reserve_place`]); without one it takes the next place
    /// when it is sent.
    pub place: Option<crate::har::Place>,
    /// While replaying, stop at the first hop the recording has no answer
    /// for, with [`NetError::Unrecorded`], rather than send it to the
    /// network ([`crate::Misses::Live`]): the caller sends the rest where
    /// it can wait for the network.
    pub replay_only: bool,
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            headers: HeaderMap::new(),
            body: None,
            follow_redirects: true,
            credentials: true,
            place: None,
            replay_only: false,
        }
    }
}

/// A fully buffered, content-decoded response.
#[derive(Debug)]
pub struct Response {
    /// The final URL after redirects.
    pub url: Url,
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    /// URLs that redirected, in order.
    pub redirect_chain: Vec<Url>,
}

impl Response {
    /// The `Content-Type` header value.
    pub fn content_type(&self) -> Option<&str> {
        self.headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok())
    }

    /// The MIME essence (`type/subtype`, lowercase) without parameters.
    pub fn mime_essence(&self) -> Option<String> {
        self.content_type().map(|ct| {
            ct.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
    }

    /// The `charset` parameter of `Content-Type`, if any.
    pub fn charset(&self) -> Option<String> {
        self.content_type()?
            .split(';')
            .skip(1)
            .map(str::trim)
            .find_map(|p| {
                let (k, v) = p.split_once('=')?;
                if k.trim().eq_ignore_ascii_case("charset") {
                    Some(v.trim().trim_matches('"').to_ascii_lowercase())
                } else {
                    None
                }
            })
    }

    pub fn is_html(&self) -> bool {
        matches!(
            self.mime_essence().as_deref(),
            Some("text/html") | Some("application/xhtml+xml")
        )
    }

    /// True if the server answered with a Cloudflare challenge page
    /// (`cf-mitigated: challenge`); see ADR 0003.
    pub fn is_cloudflare_challenge(&self) -> bool {
        self.headers
            .get("cf-mitigated")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("challenge"))
    }
}

struct Hop {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

/// An HTTP client with its own cookie jar and identity.
pub struct NetClient {
    inner: Client<Connector, Full<Bytes>>,
    /// The same transport offering HTTP/1.1 only, for hosts whose HTTP/2
    /// stalls.
    h1: Client<Connector, Full<Bytes>>,
    /// Hosts (authorities) seen answering over HTTP/2, and those moved to
    /// HTTP/1.1 after a stall.
    protocols: Protocols,
    /// The transport again, for connections hyper does not make
    /// (WebSockets).
    connector: Connector,
    config: NetConfig,
    cookies: CookieJar,
    signer: Option<BotAuthSigner>,
    recorder: Option<crate::har::Recorder>,
    replayer: Option<crate::har::Replayer>,
}

/// What the client has learnt about the HTTP versions of hosts.
#[derive(Default, Debug)]
struct HostProtocols {
    /// Hosts (authorities) seen answering over HTTP/2.
    h2: HashSet<String>,
    /// Hosts whose HTTP/2 stalls: they get HTTP/1.1 from then on.
    stalled: HashSet<String>,
    /// The requests sent to each host on the client that may speak
    /// HTTP/2, as evidence of a stall.
    traffic: HashMap<String, Traffic>,
}

/// One host's requests on the client that may speak HTTP/2.
#[derive(Default, Debug)]
struct Traffic {
    /// How many have been sent: each request's number, counting from 1.
    sent: u64,
    /// The highest number of a request answered over HTTP/2.
    answered: u64,
    /// How many have waited for their response headers longer than
    /// [`HTTP2_STALL`], and wait still.
    waiting: usize,
}

/// [`HostProtocols`], and a signal for the requests waiting long on
/// HTTP/2 whenever there may be news of their host.
#[derive(Default, Debug)]
struct Protocols {
    known: std::sync::Mutex<HostProtocols>,
    news: tokio::sync::Notify,
}

/// How long a request to a host known to speak HTTP/2 waits for its
/// response headers before the client looks for evidence that the host's
/// HTTP/2 stalls. Some servers (Heroku's router among them) at times leave
/// streams multiplexed on one connection unanswered while other streams,
/// and separate connections, are served in a second; but a request that
/// takes long is often meant to (a long poll), and sending it again would
/// repeat it.
///
/// The rule: a GET, HEAD or OPTIONS that has waited this long is sent
/// again, over an HTTP/1.1 connection of its own, only on evidence that
/// the host's HTTP/2 stalls: a request to the host sent after it has been
/// answered over HTTP/2 meanwhile (the connection serves new streams, not
/// this one), or another request to the host has waited this long at the
/// same time. The host then gets HTTP/1.1 for the client's life, so the
/// requests waiting when the evidence comes are the only ones ever sent
/// twice. A request waiting without such evidence is left to be answered
/// or to time out: a long poll is not repeated while it is its host's
/// only traffic, and at most once in a client's life when it is not.
const HTTP2_STALL: Duration = Duration::from_secs(5);

/// How a request sent over HTTP/2 ended.
#[derive(Debug, PartialEq, Eq)]
enum H2Wait<T> {
    /// It was answered (or failed) on its own.
    Done(T),
    /// The host's HTTP/2 stalls: send the request again over HTTP/1.1.
    Resend,
    /// The time allowed ran out.
    TimedOut,
}

/// A request counted as waiting long on HTTP/2, until dropped.
struct Waiting<'a> {
    protocols: &'a Protocols,
    host: String,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Some(traffic) = self.protocols.lock().traffic.get_mut(&self.host) {
            traffic.waiting = traffic.waiting.saturating_sub(1);
        }
    }
}

impl Protocols {
    fn lock(&self) -> std::sync::MutexGuard<'_, HostProtocols> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    fn is_stalled(&self, host: &str) -> bool {
        self.lock().stalled.contains(host)
    }

    /// Numbers a request to `host` as it is sent on the client that may
    /// speak HTTP/2.
    fn sent(&self, host: &str) -> u64 {
        let mut known = self.lock();
        let traffic = known.traffic.entry(host.to_string()).or_default();
        traffic.sent += 1;
        traffic.sent
    }

    /// Notes that request `number` to `host` was answered over HTTP/2.
    fn answered_h2(&self, host: &str, number: u64) {
        {
            let mut known = self.lock();
            known.h2.insert(host.to_string());
            let traffic = known.traffic.entry(host.to_string()).or_default();
            traffic.answered = traffic.answered.max(number);
        }
        self.news.notify_waiters();
    }

    /// Counts a request to `host` as waiting long.
    fn start_waiting(&self, host: &str) -> Waiting<'_> {
        self.lock()
            .traffic
            .entry(host.to_string())
            .or_default()
            .waiting += 1;
        self.news.notify_waiters();
        Waiting {
            protocols: self,
            host: host.to_string(),
        }
    }

    /// Whether there is evidence that `host`'s HTTP/2 stalls for request
    /// `number`, which waits long (see [`HTTP2_STALL`]). Evidence found
    /// marks the host, and the other requests waiting on it are told.
    fn stalls(&self, host: &str, number: u64) -> bool {
        let found = {
            let mut known = self.lock();
            if known.stalled.contains(host) {
                return true;
            }
            let found = known
                .traffic
                .get(host)
                .is_some_and(|traffic| traffic.answered > number || traffic.waiting >= 2);
            if found {
                known.stalled.insert(host.to_string());
            }
            found
        };
        if found {
            self.news.notify_waiters();
        }
        found
    }

    /// Waits up to `budget` for `response`, request `number` to `host` sent
    /// over HTTP/2, and says to send it again when, after `stall`, there
    /// is evidence that the host stalls (see [`HTTP2_STALL`]).
    async fn wait_h2<F: std::future::Future>(
        &self,
        host: &str,
        number: u64,
        response: F,
        stall: Duration,
        budget: Duration,
    ) -> H2Wait<F::Output> {
        tokio::pin!(response);
        let deadline = tokio::time::sleep(budget);
        tokio::pin!(deadline);
        tokio::select! {
            out = &mut response => return H2Wait::Done(out),
            _ = &mut deadline => return H2Wait::TimedOut,
            _ = tokio::time::sleep(stall) => {}
        }
        let _waiting = self.start_waiting(host);
        loop {
            let news = self.news.notified();
            tokio::pin!(news);
            news.as_mut().enable();
            if self.stalls(host, number) {
                return H2Wait::Resend;
            }
            tokio::select! {
                out = &mut response => return H2Wait::Done(out),
                _ = &mut deadline => return H2Wait::TimedOut,
                _ = &mut news => {}
            }
        }
    }
}

impl std::fmt::Debug for NetClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetClient")
            .field("config", &self.config)
            .field("cookies", &self.cookies)
            .field("signer", &self.signer)
            .finish()
    }
}

fn build_tls_config() -> Result<rustls::ClientConfig, NetError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| NetError::Tls(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(config)
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

/// The transport: TLS over a direct connection, or over a tunnel through
/// the proxy.
pub(crate) type Connector = HttpsConnector<ProxyOrDirect>;

/// A connection made directly (names resolved and filtered here) or
/// through a `CONNECT` or SOCKS5 proxy (which resolves the target).
#[derive(Clone)]
pub(crate) enum ProxyOrDirect {
    Direct(HttpConnector<FilteringResolver>),
    Tunnel(hyper_util::client::legacy::connect::proxy::Tunnel<HttpConnector<FilteringResolver>>),
    Socks(hyper_util::client::legacy::connect::proxy::SocksV5<HttpConnector<FilteringResolver>>),
}

impl tower_service::Service<Uri> for ProxyOrDirect {
    type Response = hyper_util::rt::TokioIo<tokio::net::TcpStream>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        match self {
            ProxyOrDirect::Direct(c) => c.poll_ready(cx).map_err(Into::into),
            ProxyOrDirect::Tunnel(c) => c.poll_ready(cx).map_err(Into::into),
            ProxyOrDirect::Socks(c) => c.poll_ready(cx).map_err(Into::into),
        }
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        match self {
            ProxyOrDirect::Direct(c) => {
                let fut = c.call(uri);
                Box::pin(async move { fut.await.map_err(Into::into) })
            }
            ProxyOrDirect::Tunnel(c) => {
                let fut = c.call(uri);
                Box::pin(async move { fut.await.map_err(Into::into) })
            }
            ProxyOrDirect::Socks(c) => {
                let fut = c.call(uri);
                Box::pin(async move { fut.await.map_err(Into::into) })
            }
        }
    }
}

impl NetClient {
    pub fn new(config: NetConfig) -> Result<Self, NetError> {
        let tls = build_tls_config()?;
        let mut http = HttpConnector::new_with_resolver(FilteringResolver {
            allow_private: config.allow_private_network,
        });
        http.enforce_http(false);
        let transport = match &config.proxy {
            None => ProxyOrDirect::Direct(http),
            Some(proxy) => {
                // The proxy itself may well be on a private network; the
                // target's address is the proxy's business.
                let (uri, auth) = policy::proxy_parts(proxy).map_err(NetError::Proxy)?;
                let mut via = HttpConnector::new_with_resolver(FilteringResolver {
                    allow_private: true,
                });
                via.enforce_http(false);
                if proxy.scheme().starts_with("socks5") {
                    let mut socks =
                        hyper_util::client::legacy::connect::proxy::SocksV5::new(uri, via);
                    if !proxy.username().is_empty() {
                        socks = socks.with_auth(
                            proxy.username().to_string(),
                            proxy.password().unwrap_or_default().to_string(),
                        );
                    }
                    ProxyOrDirect::Socks(socks)
                } else {
                    let mut tunnel =
                        hyper_util::client::legacy::connect::proxy::Tunnel::new(uri, via);
                    if let Some(auth) = auth {
                        tunnel = tunnel.with_auth(auth);
                    }
                    ProxyOrDirect::Tunnel(tunnel)
                }
            }
        };
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls.clone())
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(transport.clone());
        let inner = Client::builder(TokioExecutor::new()).build(https.clone());
        let https_h1 = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .wrap_connector(transport);
        let h1 = Client::builder(TokioExecutor::new()).build(https_h1);
        let signer = config
            .bot_auth
            .as_ref()
            .map(BotAuthSigner::new)
            .transpose()?;
        let cookies = CookieJar::new();
        if let Some(json) = &config.cookies_json {
            cookies
                .load_json(json)
                .map_err(|e| NetError::InvalidUrl(format!("cookie file: {e}")))?;
        }
        let (recorder, replayer) = match &config.recording {
            None => (None, None),
            Some(crate::har::Recording::Record(path)) => {
                (Some(crate::har::Recorder::new(path.clone())), None)
            }
            Some(crate::har::Recording::Replay { path, misses }) => (
                None,
                Some(crate::har::Replayer::open(path, *misses).map_err(NetError::Replay)?),
            ),
        };
        Ok(Self {
            inner,
            h1,
            protocols: Default::default(),
            connector: https,
            config,
            cookies,
            signer,
            recorder,
            replayer,
        })
    }

    pub(crate) fn connector(&self) -> Connector {
        self.connector.clone()
    }

    pub fn config(&self) -> &NetConfig {
        &self.config
    }

    pub fn cookies(&self) -> &CookieJar {
        &self.cookies
    }

    /// Whether answers come from a recording.
    pub fn is_replaying(&self) -> bool {
        self.replayer.is_some()
    }

    /// The next place in the recording, for a request about to be made:
    /// its entries go there ([`RequestOptions::place`]) whenever it is
    /// sent and answered, so that the recording keeps the order requests
    /// were made in. `None` when not recording.
    pub fn reserve_place(&self) -> Option<crate::har::Place> {
        self.recorder.as_ref().map(crate::har::Recorder::reserve)
    }

    /// Writes the traffic recorded so far; `None` when not recording.
    pub fn save_recording(&self) -> std::io::Result<Option<usize>> {
        match &self.recorder {
            Some(recorder) => recorder.save(&self.config.user_agent).map(Some),
            None => Ok(None),
        }
    }

    pub async fn get(&self, url: &Url) -> Result<Response, NetError> {
        self.request(Method::GET, url, RequestOptions::default())
            .await
    }

    /// Sends a request, following redirects per the Fetch specification
    /// (303 and 301/302-after-POST switch to GET and drop the body).
    pub async fn request(
        &self,
        method: Method,
        url: &Url,
        options: RequestOptions,
    ) -> Result<Response, NetError> {
        let mut method = method;
        let mut url = url.clone();
        let mut body = options.body.clone();
        let mut chain = Vec::new();
        // Taken here, before the first await, when the caller did not take
        // it: on the caller's thread when it blocks on the request.
        let place = options.place.or_else(|| self.reserve_place());

        for number in 0..=self.config.max_redirects {
            if !matches!(url.scheme(), "http" | "https") {
                return Err(NetError::UnsupportedScheme(url.scheme().to_string()));
            }
            let entry = place.map(|place| (place, number));
            let hop = match self
                .send_once(&method, &url, &options, body.clone(), entry)
                .await
            {
                Err(NetError::Unrecorded(mut unrecorded)) => {
                    unrecorded.redirect_chain = chain;
                    return Err(NetError::Unrecorded(unrecorded));
                }
                hop => hop?,
            };

            if options.follow_redirects
                && is_redirect(hop.status)
                && let Some(location) = hop
                    .headers
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|loc| url.join(loc).ok())
            {
                let switch_to_get = hop.status == StatusCode::SEE_OTHER
                    && method != Method::GET
                    && method != Method::HEAD
                    || (hop.status == StatusCode::MOVED_PERMANENTLY
                        || hop.status == StatusCode::FOUND)
                        && method == Method::POST;
                if switch_to_get {
                    method = Method::GET;
                    body = None;
                }
                chain.push(url);
                url = location;
                continue;
            }

            return Ok(Response {
                url,
                status: hop.status,
                headers: hop.headers,
                body: hop.body,
                redirect_chain: chain,
            });
        }
        Err(NetError::TooManyRedirects(self.config.max_redirects))
    }

    /// Sends one hop: `body` for `options.body` once a redirect changed it,
    /// recorded as hop `entry.1` of the request at place `entry.0`.
    async fn send_once(
        &self,
        method: &Method,
        url: &Url,
        options: &RequestOptions,
        body: Option<Bytes>,
        entry: Option<(crate::har::Place, usize)>,
    ) -> Result<Hop, NetError> {
        let credentials = options.credentials;
        if self.config.proxy.is_none() {
            policy::check_host(url, self.config.allow_private_network)
                .map_err(NetError::PrivateAddress)?;
        } else if let Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)) = url.host() {
            policy::check_host(url, self.config.allow_private_network)
                .map_err(NetError::PrivateAddress)?;
        }
        let uri: Uri = url
            .as_str()
            .parse()
            .map_err(|e| NetError::InvalidUrl(format!("{url}: {e}")))?;

        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, header_value(&self.config.user_agent)?);
        headers.insert(ACCEPT, HeaderValue::from_static(DEFAULT_ACCEPT));
        headers.insert(ACCEPT_LANGUAGE, header_value(&self.config.accept_language)?);
        headers.insert(
            ACCEPT_ENCODING,
            HeaderValue::from_static(DEFAULT_ACCEPT_ENCODING),
        );
        for (name, value) in &options.headers {
            headers.insert(name.clone(), value.clone());
        }
        if credentials && let Some(cookie) = self.cookies.request_header(url) {
            headers.insert(COOKIE, header_value(&cookie)?);
        }
        if let Some(signer) = &self.signer
            && let Some(host) = url.host_str()
            && signer.applies_to(host)
        {
            let authority = uri
                .authority()
                .map(|a| a.as_str().to_string())
                .unwrap_or_else(|| host.to_string());
            let path = url.path();
            let signed = signer.sign(method.as_str(), &authority, path)?;
            headers.insert(
                HeaderName::from_static("signature-agent"),
                header_value(&signed.signature_agent)?,
            );
            headers.insert(
                HeaderName::from_static("signature-input"),
                header_value(&signed.signature_input)?,
            );
            headers.insert(
                HeaderName::from_static("signature"),
                header_value(&signed.signature)?,
            );
        }
        // hyper sets Host from the URI; never let callers inject a stale one.
        headers.remove(HOST);

        if let Some(replayer) = &self.replayer {
            let request_body = body.clone().unwrap_or_default();
            let mime = headers
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            match replayer.answer(method, url, mime, &request_body) {
                Some(Ok(answer)) => {
                    if credentials {
                        self.cookies.store_response(url, &answer.headers);
                    }
                    return Ok(Hop {
                        status: answer.status,
                        headers: answer.headers,
                        body: answer.body,
                    });
                }
                Some(Err(reason)) => return Err(NetError::Replay(reason)),
                None if replayer.misses == crate::har::Misses::Fail => {
                    return Err(NetError::Replay(format!(
                        "no recorded answer for {method} {url}"
                    )));
                }
                None if options.replay_only => {
                    return Err(NetError::Unrecorded(Box::new(Unrecorded {
                        method: method.clone(),
                        url: url.clone(),
                        body,
                        redirect_chain: Vec::new(),
                    })));
                }
                None => {}
            }
        }
        // The entry goes to its request's place in the recording, however
        // late it comes: a replay asks in that order.
        let recording = self.recorder.as_ref().and(entry).map(|entry| {
            (
                entry,
                headers.clone(),
                body.clone().unwrap_or_default(),
                crate::har::now_ms(),
            )
        });
        let authority = uri
            .authority()
            .map(|a| a.as_str().to_string())
            .unwrap_or_default();
        let build = |uri: Uri, headers: HeaderMap, body: Option<Bytes>| {
            let mut request = Request::builder().method(method.clone()).uri(uri);
            if let Some(h) = request.headers_mut() {
                *h = headers;
            }
            request.body(Full::new(body.unwrap_or_default()))
        };
        let timeout = self.config.timeout;
        let started = Instant::now();
        let (stalled, speaks_h2) = {
            let known = self.protocols.lock();
            (
                known.stalled.contains(&authority),
                known.h2.contains(&authority),
            )
        };
        let idempotent = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
        let response = if stalled {
            tokio::time::timeout(timeout, self.h1.request(build(uri, headers, body)?))
                .await
                .map_err(|_| NetError::Timeout(timeout))??
        } else {
            let number = self.protocols.sent(&authority);
            let response = if idempotent && speaks_h2 && timeout > HTTP2_STALL {
                let first = self
                    .inner
                    .request(build(uri.clone(), headers.clone(), body.clone())?);
                match self
                    .protocols
                    .wait_h2(&authority, number, first, HTTP2_STALL, timeout)
                    .await
                {
                    H2Wait::Done(response) => response?,
                    H2Wait::TimedOut => return Err(NetError::Timeout(timeout)),
                    H2Wait::Resend => {
                        // Ask again on a connection of its own.
                        let rest = timeout.saturating_sub(started.elapsed());
                        tokio::time::timeout(rest, self.h1.request(build(uri, headers, body)?))
                            .await
                            .map_err(|_| NetError::Timeout(timeout))??
                    }
                }
            } else {
                tokio::time::timeout(timeout, self.inner.request(build(uri, headers, body)?))
                    .await
                    .map_err(|_| NetError::Timeout(timeout))??
            };
            if response.version() == http::Version::HTTP_2 {
                self.protocols.answered_h2(&authority, number);
            }
            response
        };
        let (parts, incoming) = response.into_parts();
        if credentials {
            self.cookies.store_response(url, &parts.headers);
        }

        // A body that would exceed the limit is dropped as soon as that is
        // known, by the declared length or as it arrives.
        let limit = self.config.max_response_bytes;
        if let Some(declared) = parts
            .headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<usize>().ok())
            && declared > limit
        {
            return Err(NetError::TooLarge(limit));
        }
        let raw = tokio::time::timeout(timeout, read_body_limited(incoming, limit))
            .await
            .map_err(|_| NetError::Timeout(timeout))??;
        let decoded = decode_body(&parts.headers, raw, self.config.max_decoded_bytes).map_err(
            |e| match e {
                crate::decode::DecodeError::TooLarge(limit) => NetError::TooLarge(limit),
                crate::decode::DecodeError::Failed(message) => NetError::Decode(message),
            },
        )?;

        let mut headers = parts.headers;
        // The body is now decoded; these headers would describe the wire form.
        headers.remove(CONTENT_ENCODING);
        headers.remove(CONTENT_LENGTH);
        if let (
            Some(recorder),
            Some(((place, number), request_headers, request_body, started_ms)),
        ) = (&self.recorder, recording)
        {
            recorder.record(
                place,
                number,
                crate::har::Exchange {
                    method,
                    url,
                    request_headers: &request_headers,
                    request_body: &request_body,
                    status: parts.status,
                    response_headers: &headers,
                    response_body: &decoded,
                    started_ms,
                    took_ms: crate::har::now_ms().saturating_sub(started_ms),
                },
            );
        }
        Ok(Hop {
            status: parts.status,
            headers,
            body: decoded,
        })
    }
}

/// Reads a body up to `limit` bytes; past it the connection is dropped.
async fn read_body_limited(
    mut body: hyper::body::Incoming,
    limit: usize,
) -> Result<Bytes, NetError> {
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Ok(data) = frame.into_data() {
            if out.len() + data.len() > limit {
                return Err(NetError::TooLarge(limit));
            }
            out.extend_from_slice(&data);
        }
    }
    Ok(Bytes::from(out))
}

fn header_value(s: &str) -> Result<HeaderValue, NetError> {
    HeaderValue::from_str(s)
        .map_err(|e| NetError::InvalidUrl(format!("invalid header value `{s}`: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_helpers_parse_content_type() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("Text/HTML; charset=\"ISO-8859-1\""),
        );
        let r = Response {
            url: Url::parse("https://example.com/").unwrap(),
            status: StatusCode::OK,
            headers,
            body: Bytes::new(),
            redirect_chain: vec![],
        };
        assert_eq!(r.mime_essence().as_deref(), Some("text/html"));
        assert_eq!(r.charset().as_deref(), Some("iso-8859-1"));
        assert!(r.is_html());
        assert!(!r.is_cloudflare_challenge());
    }

    #[test]
    fn client_builds_with_default_config() {
        let client = NetClient::new(NetConfig::default()).unwrap();
        assert!(client.cookies().is_empty());
        assert_eq!(client.config().user_agent, DEFAULT_USER_AGENT);
    }

    const STALL: Duration = Duration::from_millis(40);
    const BUDGET: Duration = Duration::from_secs(5);

    async fn answered_after(delay: Duration) -> &'static str {
        tokio::time::sleep(delay).await;
        "answer"
    }

    fn waiting(protocols: &Protocols, host: &str) -> usize {
        protocols
            .lock()
            .traffic
            .get(host)
            .map_or(0, |traffic| traffic.waiting)
    }

    #[tokio::test]
    async fn a_slow_request_waiting_alone_is_not_sent_again() {
        let protocols = Protocols::default();
        let number = protocols.sent("host");
        let slow = answered_after(STALL * 5);
        assert_eq!(
            protocols.wait_h2("host", number, slow, STALL, BUDGET).await,
            H2Wait::Done("answer")
        );
        assert!(!protocols.is_stalled("host"));
        assert_eq!(waiting(&protocols, "host"), 0);
        // Nor are two slow requests to different hosts, nor one whose host
        // answered only requests sent before it.
        let earlier = protocols.sent("b");
        let (a, b, ()) = tokio::join!(
            protocols.wait_h2(
                "a",
                protocols.sent("a"),
                answered_after(STALL * 3),
                STALL,
                BUDGET
            ),
            protocols.wait_h2(
                "b",
                protocols.sent("b"),
                answered_after(STALL * 3),
                STALL,
                BUDGET
            ),
            async {
                tokio::time::sleep(STALL * 2).await;
                protocols.answered_h2("b", earlier);
            },
        );
        assert_eq!((a, b), (H2Wait::Done("answer"), H2Wait::Done("answer")));
        assert!(!protocols.is_stalled("a") && !protocols.is_stalled("b"));
    }

    #[tokio::test]
    async fn requests_stuck_at_once_on_a_host_are_sent_again() {
        let protocols = Protocols::default();
        let stuck = || std::future::pending::<&str>();
        let later = async {
            tokio::time::sleep(STALL * 2).await;
            let number = protocols.sent("host");
            protocols
                .wait_h2("host", number, stuck(), STALL, BUDGET)
                .await
        };
        let first = protocols.sent("host");
        let (first, second) = tokio::join!(
            protocols.wait_h2("host", first, stuck(), STALL, BUDGET),
            later
        );
        assert_eq!((first, second), (H2Wait::Resend, H2Wait::Resend));
        assert!(protocols.is_stalled("host"));
        assert_eq!(waiting(&protocols, "host"), 0);
        // Once a host stalls, a request that waits long goes again at once.
        let number = protocols.sent("host");
        assert_eq!(
            protocols
                .wait_h2("host", number, stuck(), STALL, BUDGET)
                .await,
            H2Wait::Resend
        );
    }

    #[tokio::test]
    async fn a_request_skipped_while_later_ones_are_answered_is_sent_again() {
        let protocols = Protocols::default();
        let stuck = protocols.sent("host");
        let (outcome, ()) = tokio::join!(
            protocols.wait_h2("host", stuck, std::future::pending::<&str>(), STALL, BUDGET),
            async {
                // A request sent later is answered while the first waits,
                // before and after the first has waited long.
                tokio::time::sleep(STALL / 2).await;
                let later = protocols.sent("host");
                protocols.answered_h2("host", later);
            },
        );
        assert_eq!(outcome, H2Wait::Resend);
        assert!(protocols.is_stalled("host"));

        let protocols = Protocols::default();
        let stuck = protocols.sent("host");
        let (outcome, ()) = tokio::join!(
            protocols.wait_h2("host", stuck, std::future::pending::<&str>(), STALL, BUDGET),
            async {
                tokio::time::sleep(STALL * 3).await;
                let later = protocols.sent("host");
                protocols.answered_h2("host", later);
            },
        );
        assert_eq!(outcome, H2Wait::Resend);
    }

    #[tokio::test]
    async fn a_request_waiting_alone_times_out() {
        let protocols = Protocols::default();
        let number = protocols.sent("host");
        let stuck = std::future::pending::<&str>();
        assert_eq!(
            protocols
                .wait_h2("host", number, stuck, STALL, STALL * 3)
                .await,
            H2Wait::TimedOut
        );
        assert!(!protocols.is_stalled("host"));
    }
}
