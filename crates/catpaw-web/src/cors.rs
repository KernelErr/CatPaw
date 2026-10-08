//! Request preparation, redirects and response checks shared by `fetch()`,
//! `XMLHttpRequest` and `sendBeacon()`: which requests carry credentials,
//! which need a CORS preflight, how a redirect is followed, and which
//! responses script is allowed to read.
//!
//! This is the part of the Fetch standard that keeps a page from reading
//! another origin's data with the user's cookies. It is deliberately strict
//! where it simplifies: anything it cannot vouch for is a network error.

use std::cell::Cell;
use std::rc::Rc;

use url::{Origin, Url};

use crate::generated::ReferrerPolicy;
use crate::net::{NetRequest, NetResponse, RequestKind, start_request};
use crate::page::{Cx, PageState};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    SameOrigin,
    Cors,
    NoCors,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Credentials {
    Omit,
    SameOrigin,
    Include,
}

/// What to do with a redirect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Redirect {
    Follow,
    Error,
    Manual,
}

/// A request as script described it.
pub struct Outgoing {
    pub method: String,
    pub url: Url,
    /// Author headers, forbidden ones already dropped. Names keep the
    /// case they were given.
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub mode: Mode,
    pub credentials: Credentials,
    pub redirect: Redirect,
    /// The URL the referrer is derived from, or `None` for no referrer.
    pub referrer: Option<Url>,
    pub referrer_policy: ReferrerPolicy,
    pub kind: RequestKind,
    /// Where script made the request.
    pub site: Option<catpaw_js::SourceSite>,
}

/// How much of a response script may see.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Exposure {
    /// Same origin: everything but cookies.
    Basic,
    /// Cross origin, allowed by CORS: safelisted and exposed headers only.
    Cors,
    /// Cross origin, `no-cors`: nothing.
    Opaque,
    /// A redirect the request was told not to follow: nothing but the URL.
    OpaqueRedirect,
}

/// A response that passed the checks, filtered down to what is readable.
pub struct Readable {
    pub response: NetResponse,
    pub exposure: Exposure,
    /// Whether a redirect was followed on the way to it.
    pub redirected: bool,
}

// ---- header rules ---------------------------------------------------------

/// <https://fetch.spec.whatwg.org/#forbidden-request-header>
pub fn is_forbidden_request_header(name: &str, value: &str) -> bool {
    let name = name.to_ascii_lowercase();
    // A method override header may not smuggle a forbidden method.
    if matches!(
        name.as_str(),
        "x-http-method" | "x-http-method-override" | "x-method-override"
    ) && split_header_value(value)
        .iter()
        .any(|method| is_forbidden_method(method))
    {
        return true;
    }
    matches!(
        name.as_str(),
        "accept-charset"
            | "accept-encoding"
            | "access-control-request-headers"
            | "access-control-request-method"
            | "connection"
            | "content-length"
            | "cookie"
            | "cookie2"
            | "date"
            | "dnt"
            | "expect"
            | "host"
            | "keep-alive"
            | "origin"
            | "referer"
            | "set-cookie"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "via"
    ) || name.starts_with("proxy-")
        || name.starts_with("sec-")
}

/// <https://fetch.spec.whatwg.org/#forbidden-method>
pub fn is_forbidden_method(method: &str) -> bool {
    ["CONNECT", "TRACE", "TRACK"]
        .iter()
        .any(|m| method.eq_ignore_ascii_case(m))
}

/// <https://fetch.spec.whatwg.org/#forbidden-response-header-name>
pub fn is_forbidden_response_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("set-cookie") || name.eq_ignore_ascii_case("set-cookie2")
}

/// Splits a header value at commas, keeping quoted strings whole
/// (<https://fetch.spec.whatwg.org/#header-value-get-decode-and-split>).
pub fn split_header_value(value: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars().peekable();
    while chars.peek().is_some() {
        while let Some(&c) = chars.peek() {
            if c == '"' || c == ',' {
                break;
            }
            current.push(c);
            chars.next();
        }
        if chars.next() == Some('"') {
            // An HTTP quoted string, kept with its quotes and escapes.
            current.push('"');
            while let Some(c) = chars.next() {
                current.push(c);
                match c {
                    '\\' => current.extend(chars.next()),
                    '"' => break,
                    _ => {}
                }
            }
            if chars.peek().is_some() {
                continue;
            }
        }
        values.push(current.trim_matches([' ', '\t']).to_string());
        current.clear();
    }
    values
}

/// Ports no browser connects to
/// (<https://fetch.spec.whatwg.org/#port-blocking>), in order.
const BAD_PORTS: &[u16] = &[
    1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101, 102,
    103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389, 427, 465,
    512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990, 993,
    995, 1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566, 6665, 6666, 6667, 6668,
    6669, 6679, 6697, 10080,
];

/// Whether `url` names a bad port: a request to it is a network error.
pub fn is_bad_port(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https" | "ws" | "wss")
        && url
            .port()
            .is_some_and(|port| port == 0 || BAD_PORTS.binary_search(&port).is_ok())
}

/// Whether `name` is a valid header name (an HTTP token).
pub fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Strips the whitespace a header value may not start or end with; `None`
/// if what is left is not a valid value.
pub fn normalize_header_value(value: &str) -> Option<String> {
    let value = value.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r'));
    (!value.contains(['\0', '\n', '\r'])).then(|| value.to_string())
}

pub(crate) fn is_safelisted_method(method: &str) -> bool {
    matches!(method, "GET" | "HEAD" | "POST")
}

fn mime_essence(value: &str) -> String {
    value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// <https://fetch.spec.whatwg.org/#no-cors-safelisted-request-header>
pub(crate) fn is_no_cors_safelisted_request_header(name: &str, value: &str) -> bool {
    matches!(
        name,
        "accept" | "accept-language" | "content-language" | "content-type"
    ) && is_safelisted_request_header(name, value)
}

/// <https://fetch.spec.whatwg.org/#cors-safelisted-request-header>
pub(crate) fn is_safelisted_request_header(name: &str, value: &str) -> bool {
    if value.len() > 128 {
        return false;
    }
    match name {
        "accept" | "accept-language" | "content-language" => true,
        "content-type" => matches!(
            mime_essence(value).as_str(),
            "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain"
        ),
        _ => false,
    }
}

const SAFELISTED_RESPONSE_HEADERS: &[&str] = &[
    "cache-control",
    "content-language",
    "content-length",
    "content-type",
    "expires",
    "last-modified",
    "pragma",
];

fn header_list(response: &NetResponse, name: &str) -> Vec<String> {
    response
        .headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .flat_map(|(_, v)| v.split(','))
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn without_fragment(url: &Url) -> Url {
    let mut url = url.clone();
    url.set_fragment(None);
    url
}

// ---- the transfer ---------------------------------------------------------

/// <https://fetch.spec.whatwg.org/#concept-request-response-tainting>
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tainting {
    Basic,
    Cors,
    Opaque,
}

/// A request on its way, through any redirects: what every hop shares.
/// Numbers fetches, so that a preflight, the request and its redirect
/// hops can be told to belong together (see `NetRequest::chain`).
static NEXT_CHAIN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct Transfer {
    chain: u64,
    page_origin: Origin,
    page_url: Url,
    mode: Mode,
    credentials: Credentials,
    redirect: Redirect,
    referrer: Option<Url>,
    referrer_policy: ReferrerPolicy,
    kind: RequestKind,
    method: String,
    /// Author headers, names as given.
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    tainting: Tainting,
    /// The URLs requested so far; the last one is current.
    urls: Vec<Url>,
    /// Where script made the request.
    site: Option<catpaw_js::SourceSite>,
}

/// What to do once a hop answered.
enum Step {
    /// Request the current URL, which just changed.
    Next,
    /// Hand this to script.
    Deliver(Result<Readable, String>),
}

impl Transfer {
    /// The checks of main fetch that the request's own description decides.
    fn new(page: &PageState, out: Outgoing) -> Result<Self, String> {
        if out.mode == Mode::NoCors && !is_safelisted_method(&out.method) {
            return Err(format!(
                "method {} is not allowed in \"no-cors\" mode",
                out.method
            ));
        }
        let page_url = page.url.borrow().clone();
        let mut headers = out.headers;
        if out.mode == Mode::NoCors {
            headers.retain(|(name, value)| {
                is_safelisted_request_header(&name.to_ascii_lowercase(), value)
            });
        }
        let mut transfer = Self {
            chain: NEXT_CHAIN.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            page_origin: page_url.origin(),
            page_url,
            mode: out.mode,
            credentials: out.credentials,
            redirect: out.redirect,
            referrer: out.referrer,
            referrer_policy: out.referrer_policy,
            kind: out.kind,
            method: out.method,
            headers,
            body: out.body,
            tainting: Tainting::Basic,
            urls: vec![out.url],
            site: out.site,
        };
        transfer.enter_url()?;
        Ok(transfer)
    }

    fn current_url(&self) -> &Url {
        self.urls.last().expect("a transfer has a URL")
    }

    /// Main fetch for the current URL: whether it may be requested at all,
    /// and how its response will be tainted.
    fn enter_url(&mut self) -> Result<(), String> {
        let url = self.current_url().clone();
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("URL scheme \"{}\" is not supported", url.scheme()));
        }
        if is_bad_port(&url) {
            return Err(format!(
                "port {} is blocked",
                url.port().unwrap_or_default()
            ));
        }
        if url.origin() == self.page_origin && self.tainting == Tainting::Basic {
            return Ok(());
        }
        match self.mode {
            Mode::SameOrigin => Err(format!(
                "the request mode is \"same-origin\" but {url} is cross-origin"
            )),
            Mode::NoCors => {
                if self.redirect != Redirect::Follow {
                    return Err("a \"no-cors\" request must follow redirects".to_string());
                }
                self.tainting = Tainting::Opaque;
                Ok(())
            }
            Mode::Cors => {
                self.tainting = Tainting::Cors;
                Ok(())
            }
        }
    }

    /// <https://fetch.spec.whatwg.org/#concept-request-tainted-origin>
    fn redirect_tainted(&self) -> bool {
        let mut last: Option<&Url> = None;
        for url in &self.urls {
            if let Some(previous) = last
                && url.origin() != previous.origin()
                && previous.origin() != self.page_origin
            {
                return true;
            }
            last = Some(url);
        }
        false
    }

    /// <https://fetch.spec.whatwg.org/#byte-serializing-a-request-origin>
    fn serialized_origin(&self) -> String {
        if self.redirect_tainted() {
            "null".to_string()
        } else {
            self.page_origin.ascii_serialization()
        }
    }

    /// <https://fetch.spec.whatwg.org/#append-a-request-origin-header>
    fn origin_header(&self) -> Option<String> {
        if self.tainting == Tainting::Cors {
            return Some(self.serialized_origin());
        }
        if matches!(self.method.as_str(), "GET" | "HEAD") {
            return None;
        }
        // Outside CORS the referrer policy decides whether the origin is
        // told; a `null` origin says nothing.
        let downgrade = self.page_url.scheme() == "https" && self.current_url().scheme() != "https";
        let cross_origin = self.current_url().origin() != self.page_origin;
        let withheld = match self.referrer_policy {
            ReferrerPolicy::NoReferrer => true,
            ReferrerPolicy::SameOrigin => cross_origin,
            ReferrerPolicy::Empty
            | ReferrerPolicy::NoReferrerWhenDowngrade
            | ReferrerPolicy::StrictOrigin
            | ReferrerPolicy::StrictOriginWhenCrossOrigin => downgrade,
            ReferrerPolicy::Origin
            | ReferrerPolicy::OriginWhenCrossOrigin
            | ReferrerPolicy::UnsafeUrl => false,
        };
        Some(if withheld {
            "null".to_string()
        } else {
            self.serialized_origin()
        })
    }

    fn with_credentials(&self) -> bool {
        match self.credentials {
            Credentials::Omit => false,
            Credentials::SameOrigin => self.tainting == Tainting::Basic,
            Credentials::Include => true,
        }
    }

    /// Author header names that are not CORS-safelisted, sorted.
    fn unsafe_headers(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .headers
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value))
            .filter(|(name, value)| !is_safelisted_request_header(name, value))
            .map(|(name, _)| name)
            .collect();
        names.sort();
        names.dedup();
        names
    }

    fn needs_preflight(&self) -> bool {
        self.tainting == Tainting::Cors
            && (!is_safelisted_method(&self.method) || !self.unsafe_headers().is_empty())
    }

    /// The request for the current URL, and the preflight that must
    /// succeed first, if one is needed.
    fn requests(&mut self) -> (NetRequest, Option<NetRequest>) {
        let url = without_fragment(self.current_url());
        // A referrer a policy cut down stays cut down: the policy of a
        // later hop works on what is left.
        let referrer = self.referrer(&url);
        self.referrer = referrer.clone();
        let mut headers = self.headers.clone();
        if let Some(origin) = self.origin_header() {
            headers.push(("origin".to_string(), origin));
        }
        let request = NetRequest {
            method: self.method.clone(),
            url: url.clone(),
            headers,
            body: self.body.clone(),
            kind: self.kind,
            referrer: referrer.clone(),
            credentials: self.with_credentials(),
            follow_redirects: false,
            site: self.site.clone(),
            chain: self.chain,
        };
        let preflight = self.needs_preflight().then(|| {
            let mut headers = vec![
                ("origin".to_string(), self.serialized_origin()),
                (
                    "access-control-request-method".to_string(),
                    self.method.clone(),
                ),
            ];
            let unsafe_headers = self.unsafe_headers();
            if !unsafe_headers.is_empty() {
                headers.push((
                    "access-control-request-headers".to_string(),
                    unsafe_headers.join(","),
                ));
            }
            NetRequest {
                method: "OPTIONS".to_string(),
                url,
                headers,
                body: None,
                kind: self.kind,
                referrer,
                credentials: false,
                follow_redirects: false,
                site: self.site.clone(),
                chain: self.chain,
            }
        });
        (request, preflight)
    }

    /// The referrer sent with a request to `url`, under the policy.
    fn referrer(&self, url: &Url) -> Option<Url> {
        self.referrer
            .as_ref()
            .and_then(|source| crate::referrer::determine(self.referrer_policy, source, url))
    }

    /// <https://fetch.spec.whatwg.org/#concept-cors-check>
    fn cors_check(&self, response: &NetResponse) -> Result<(), String> {
        let allowed = response
            .header("access-control-allow-origin")
            .map(str::trim)
            .ok_or("the response has no Access-Control-Allow-Origin header")?;
        let with_credentials = self.with_credentials();
        if allowed == "*" {
            return if with_credentials {
                Err(
                    "Access-Control-Allow-Origin must not be \"*\" for a request with credentials"
                        .to_string(),
                )
            } else {
                Ok(())
            };
        }
        let origin = self.serialized_origin();
        if allowed != origin {
            return Err(format!(
                "Access-Control-Allow-Origin is \"{allowed}\", which does not match the origin {origin}"
            ));
        }
        if with_credentials
            && response
                .header("access-control-allow-credentials")
                .map(str::trim)
                != Some("true")
        {
            return Err(
                "Access-Control-Allow-Credentials must be \"true\" for a request with credentials"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// The checks on a preflight response
    /// (<https://fetch.spec.whatwg.org/#cors-preflight-fetch-0>).
    fn check_preflight(&self, response: &NetResponse) -> Result<(), String> {
        if !response.is_success() {
            return Err(format!(
                "the preflight response has status {}",
                response.status
            ));
        }
        self.cors_check(response)?;
        let wildcard_ok = !self.with_credentials();
        let methods = header_list(response, "access-control-allow-methods");
        let method_allowed = is_safelisted_method(&self.method)
            || methods.contains(&self.method)
            || (wildcard_ok && methods.iter().any(|m| m == "*"));
        if !method_allowed {
            return Err(format!(
                "method {} is not allowed by Access-Control-Allow-Methods",
                self.method
            ));
        }
        let allowed: Vec<String> = header_list(response, "access-control-allow-headers")
            .into_iter()
            .map(|h| h.to_ascii_lowercase())
            .collect();
        for name in &self.unsafe_headers() {
            let ok = allowed.iter().any(|h| h == name)
                || (wildcard_ok && name != "authorization" && allowed.iter().any(|h| h == "*"));
            if !ok {
                return Err(format!(
                    "request header {name} is not allowed by Access-Control-Allow-Headers"
                ));
            }
        }
        Ok(())
    }

    /// What a response to the current URL leads to; for a redirect,
    /// <https://fetch.spec.whatwg.org/#concept-http-redirect-fetch>.
    fn on_response(&mut self, response: NetResponse) -> Step {
        if !is_redirect_status(response.status) {
            return Step::Deliver(self.finish(response));
        }
        match self.redirect {
            Redirect::Error => {
                return Step::Deliver(Err(
                    "the request was redirected, and its redirect mode is \"error\"".to_string(),
                ));
            }
            Redirect::Manual => return Step::Deliver(Ok(self.opaque_redirect(response))),
            Redirect::Follow => {}
        }
        let Some(location) = response.header("location") else {
            return Step::Deliver(self.finish(response));
        };
        let Ok(mut location) = self.current_url().join(location) else {
            return Step::Deliver(Err(format!(
                "the Location header {location:?} is not a valid URL"
            )));
        };
        if location.fragment().is_none() {
            location.set_fragment(self.current_url().fragment());
        }
        if !matches!(location.scheme(), "http" | "https") {
            return Step::Deliver(Err(format!(
                "redirected to the unsupported scheme {}",
                location.scheme()
            )));
        }
        if self.urls.len() > 20 {
            return Step::Deliver(Err("too many redirects".to_string()));
        }
        let has_credentials = !location.username().is_empty() || location.password().is_some();
        if has_credentials
            && (self.tainting == Tainting::Cors
                || (self.mode == Mode::Cors && location.origin() != self.page_origin))
        {
            return Step::Deliver(Err("redirected to a URL with credentials".to_string()));
        }
        let to_get = (matches!(response.status, 301 | 302) && self.method == "POST")
            || (response.status == 303 && !matches!(self.method.as_str(), "GET" | "HEAD"));
        if to_get {
            self.method = "GET".to_string();
            self.body = None;
            self.headers.retain(|(name, _)| {
                !matches!(
                    name.to_ascii_lowercase().as_str(),
                    "content-encoding" | "content-language" | "content-location" | "content-type"
                )
            });
        }
        if location.origin() != self.current_url().origin() {
            self.headers
                .retain(|(name, _)| !name.eq_ignore_ascii_case("authorization"));
        }
        // A redirect may tighten (or loosen) the referrer policy.
        if let Some(policy) = response
            .header("referrer-policy")
            .and_then(crate::referrer::from_header)
        {
            self.referrer_policy = policy;
        }
        self.urls.push(location);
        match self.enter_url() {
            Ok(()) => Step::Next,
            Err(reason) => Step::Deliver(Err(reason)),
        }
    }

    /// <https://fetch.spec.whatwg.org/#concept-filtered-response-opaque-redirect>
    fn opaque_redirect(&self, mut response: NetResponse) -> Readable {
        response.status = 0;
        response.status_text.clear();
        response.headers.clear();
        response.body.clear();
        response.url = without_fragment(&self.urls[0]);
        Readable {
            response,
            exposure: Exposure::OpaqueRedirect,
            redirected: false,
        }
    }

    /// Decides what of `response` script may read.
    fn finish(&self, mut response: NetResponse) -> Result<Readable, String> {
        response.url = without_fragment(self.current_url());
        response
            .headers
            .retain(|(name, _)| !is_forbidden_response_header(name));
        let exposure = match self.tainting {
            Tainting::Basic => Exposure::Basic,
            Tainting::Opaque => {
                response.status = 0;
                response.status_text.clear();
                response.headers.clear();
                response.body.clear();
                Exposure::Opaque
            }
            Tainting::Cors => {
                self.cors_check(&response)?;
                let exposed: Vec<String> = header_list(&response, "access-control-expose-headers")
                    .into_iter()
                    .map(|h| h.to_ascii_lowercase())
                    .collect();
                let expose_all = !self.with_credentials() && exposed.iter().any(|h| h == "*");
                response.headers.retain(|(name, _)| {
                    let name = name.to_ascii_lowercase();
                    expose_all
                        || SAFELISTED_RESPONSE_HEADERS.contains(&name.as_str())
                        || exposed.contains(&name)
                });
                Exposure::Cors
            }
        };
        Ok(Readable {
            response,
            exposure,
            redirected: self.urls.len() > 1,
        })
    }
}

// ---- sending --------------------------------------------------------------

/// A started request. Hold on to it to be able to abort.
#[derive(Clone, Default)]
pub struct Pending {
    token: Rc<Cell<Option<u64>>>,
}

impl Pending {
    /// Abandons the request; its callback is never called.
    pub fn abort(&self, page: &PageState) {
        if let Some(token) = self.token.take() {
            crate::net::abort_request(page, token);
        }
    }
}

type Done = Box<dyn FnOnce(&mut Cx<'_>, Result<Readable, String>)>;

fn local_readable(response: NetResponse) -> Readable {
    Readable {
        response,
        exposure: Exposure::Basic,
        redirected: false,
    }
}

/// Sends `out` and passes what script may read of the response (or the
/// reason it failed) to `done`, from a later task.
pub fn send(
    page: &PageState,
    out: Outgoing,
    done: impl FnOnce(&mut Cx<'_>, Result<Readable, String>) + 'static,
) -> Pending {
    let pending = Pending::default();
    let done: Done = Box::new(done);
    // A data URL is its own response, readable by anyone.
    if let Some(result) = crate::net::local_response(page, &out.url) {
        crate::event_loop::queue_task(page, "data URL", move |cx| {
            done(cx, result.map(local_readable));
        });
        return pending;
    }
    match Transfer::new(page, out) {
        Ok(transfer) => start_hop(page, transfer, &pending, done),
        Err(reason) => {
            crate::event_loop::queue_task(page, "request rejected", move |cx| {
                done(cx, Err(reason));
            });
        }
    }
    pending
}

/// Requests the transfer's current URL, after a preflight if one is
/// needed, then goes on to the next hop or delivers the outcome.
fn start_hop(page: &PageState, mut transfer: Transfer, pending: &Pending, done: Done) {
    let (request, preflight) = transfer.requests();
    match preflight {
        None => start_actual(page, transfer, request, pending, done),
        Some(preflight) => {
            // The actual request only goes out once the server has agreed.
            let after = pending.clone();
            let token = start_request(page, preflight, move |cx, result| {
                match result.and_then(|r| transfer.check_preflight(&r)) {
                    Ok(()) => start_actual(cx.page, transfer, request, &after, done),
                    Err(reason) => done(cx, Err(format!("CORS preflight failed: {reason}"))),
                }
            });
            pending.token.set(token);
        }
    }
}

fn start_actual(
    page: &PageState,
    mut transfer: Transfer,
    request: NetRequest,
    pending: &Pending,
    done: Done,
) {
    let after = pending.clone();
    let token = start_request(page, request, move |cx, result| match result {
        Err(reason) => done(cx, Err(reason)),
        Ok(response) => match transfer.on_response(response) {
            Step::Deliver(outcome) => done(cx, outcome),
            Step::Next => start_hop(cx.page, transfer, &after, done),
        },
    });
    pending.token.set(token);
}

/// Sends `out` on the calling thread (synchronous XHR).
pub fn send_blocking(page: &PageState, out: Outgoing) -> Result<Readable, String> {
    if let Some(result) = crate::net::local_response(page, &out.url) {
        return result.map(local_readable);
    }
    let mut transfer = Transfer::new(page, out)?;
    let net = page.net().ok_or("no network available")?;
    loop {
        let (request, preflight) = transfer.requests();
        if let Some(preflight) = preflight {
            let response = net.fetch_blocking(preflight)?;
            transfer
                .check_preflight(&response)
                .map_err(|reason| format!("CORS preflight failed: {reason}"))?;
        }
        let response = net.fetch_blocking(request)?;
        match transfer.on_response(response) {
            Step::Deliver(outcome) => return outcome,
            Step::Next => {}
        }
    }
}
