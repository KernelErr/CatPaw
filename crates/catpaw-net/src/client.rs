//! The HTTP client: hyper (HTTP/1.1 and HTTP/2 over ALPN) with rustls,
//! redirects, cookies, content decoding and optional Web Bot Auth signing.

use std::sync::Arc;
use std::time::Duration;

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
}

#[derive(Debug, Clone)]
pub struct NetConfig {
    pub user_agent: String,
    pub accept_language: String,
    pub max_redirects: usize,
    /// Overall budget for one hop (connect, headers, body).
    pub timeout: Duration,
    pub bot_auth: Option<BotAuthConfig>,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            user_agent: DEFAULT_USER_AGENT.to_string(),
            accept_language: "en-US,en;q=0.9".to_string(),
            max_redirects: 20,
            timeout: Duration::from_secs(30),
            bot_auth: None,
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
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            headers: HeaderMap::new(),
            body: None,
            follow_redirects: true,
            credentials: true,
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
    inner: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    config: NetConfig,
    cookies: CookieJar,
    signer: Option<BotAuthSigner>,
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

impl NetClient {
    pub fn new(config: NetConfig) -> Result<Self, NetError> {
        let tls = build_tls_config()?;
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build();
        let inner = Client::builder(TokioExecutor::new()).build(https);
        let signer = config
            .bot_auth
            .as_ref()
            .map(BotAuthSigner::new)
            .transpose()?;
        Ok(Self {
            inner,
            config,
            cookies: CookieJar::new(),
            signer,
        })
    }

    pub fn config(&self) -> &NetConfig {
        &self.config
    }

    pub fn cookies(&self) -> &CookieJar {
        &self.cookies
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

        for _ in 0..=self.config.max_redirects {
            if !matches!(url.scheme(), "http" | "https") {
                return Err(NetError::UnsupportedScheme(url.scheme().to_string()));
            }
            let hop = self
                .send_once(
                    &method,
                    &url,
                    &options.headers,
                    body.clone(),
                    options.credentials,
                )
                .await?;

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

    async fn send_once(
        &self,
        method: &Method,
        url: &Url,
        extra_headers: &HeaderMap,
        body: Option<Bytes>,
        credentials: bool,
    ) -> Result<Hop, NetError> {
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
        for (name, value) in extra_headers {
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

        let mut request = Request::builder().method(method.clone()).uri(uri);
        if let Some(h) = request.headers_mut() {
            *h = headers;
        }
        let request = request.body(Full::new(body.unwrap_or_default()))?;

        let timeout = self.config.timeout;
        let response = tokio::time::timeout(timeout, self.inner.request(request))
            .await
            .map_err(|_| NetError::Timeout(timeout))??;
        let (parts, incoming) = response.into_parts();
        if credentials {
            self.cookies.store_response(url, &parts.headers);
        }

        let collected = tokio::time::timeout(timeout, incoming.collect())
            .await
            .map_err(|_| NetError::Timeout(timeout))??;
        let raw = collected.to_bytes();
        let decoded = decode_body(&parts.headers, raw).map_err(NetError::Decode)?;

        let mut headers = parts.headers;
        // The body is now decoded; these headers would describe the wire form.
        headers.remove(CONTENT_ENCODING);
        headers.remove(CONTENT_LENGTH);
        Ok(Hop {
            status: parts.status,
            headers,
            body: decoded,
        })
    }
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
}
