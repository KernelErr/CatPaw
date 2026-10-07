//! Stand-ins for the Python handlers web-platform-tests serves from
//! `resources/*.py`: the ones the fetch and XHR tests lean on, behaving as
//! the originals do (query parameters, echoed request headers, redirects,
//! the server stash), short of what needs a real socket: trickled bodies,
//! chunked encoding, HTTP authentication and cookies.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use catpaw_web::net::NetRequest;
use url::Url;

const ACAO: &str = "access-control-allow-origin";
const ACAC: &str = "access-control-allow-credentials";
const ACAM: &str = "access-control-allow-methods";
const ACAH: &str = "access-control-allow-headers";
const ACEH: &str = "access-control-expose-headers";
const ACMA: &str = "access-control-max-age";
const CORS_ALLOWED: &str = "PASS: Cross-domain access allowed.";

/// What a handler answers with.
pub struct Served {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// How long the server takes to answer.
    pub delay: Duration,
}

/// The standard reason phrase, as wptserve sends it.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "",
    }
}

impl Served {
    fn ok() -> Self {
        Self {
            status: 200,
            status_text: "OK".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
            delay: Duration::ZERO,
        }
    }

    fn status(mut self, status: u16, text: &str) -> Self {
        self.status = status;
        self.status_text = text.to_string();
        self
    }

    fn code(self, status: u16) -> Self {
        self.status(status, reason(status))
    }

    /// Adds a header; a second one of the same name is kept too.
    fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    /// Adds a header if there is a value for it.
    fn header_opt(self, name: &str, value: Option<String>) -> Self {
        match value {
            Some(value) => self.header(name, value),
            None => self,
        }
    }

    fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    fn text(self, body: &str) -> Self {
        self.body(body.as_bytes().to_vec())
    }

    fn delay_ms(mut self, ms: f64) -> Self {
        self.delay = Duration::from_secs_f64((ms / 1000.0).clamp(0.0, 5.0));
        self
    }

    /// The usual CORS allowance for the page: its origin echoed back, or
    /// `*` when it sent none.
    fn allow_origin(self, req: &Req<'_>) -> Self {
        match req.header("origin") {
            Some(origin) => self.header(ACAO, origin).header(ACAC, "true"),
            None => self.header(ACAO, "*"),
        }
    }
}

/// The server stash: a value put by one request and taken by another,
/// scoped to a path as wptserve does it (the handler's own unless it
/// names one).
#[derive(Default)]
pub struct Stash(RefCell<HashMap<(String, String), String>>);

impl Stash {
    fn put(&self, path: &str, key: &str, value: impl Into<String>) {
        self.0
            .borrow_mut()
            .insert((path.to_string(), key.to_string()), value.into());
    }

    fn take(&self, path: &str, key: &str) -> Option<String> {
        self.0
            .borrow_mut()
            .remove(&(path.to_string(), key.to_string()))
    }
}

/// A few named values, as the handlers keep in the stash.
fn encode_map(entries: &[(&str, Option<&str>)]) -> String {
    entries
        .iter()
        .filter_map(|(k, v)| v.map(|v| format!("{k}\t{v}\n")))
        .collect()
}

fn decode_map(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Percent-decodes `input`, `+` standing for a space.
fn decode_form_bytes(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len()
                && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) =>
            {
                out.push(hi * 16 + lo);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    out
}

/// The query's pairs with their values as bytes: a handler may be asked
/// for bytes that are not UTF-8.
fn query_bytes(url: &Url) -> Vec<(String, Vec<u8>)> {
    url.query()
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (
                String::from_utf8_lossy(&decode_form_bytes(key)).into_owned(),
                decode_form_bytes(value),
            )
        })
        .collect()
}

/// Decodes base64, as the `Authorization: Basic` scheme uses it.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in text.bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The request as a handler sees it.
struct Req<'a> {
    request: &'a NetRequest,
    path: String,
    query: Vec<(String, Vec<u8>)>,
    post: Vec<(String, String)>,
}

impl Req<'_> {
    fn new(request: &NetRequest) -> Req<'_> {
        let url = &request.url;
        let query = query_bytes(url);
        let post = match &request.body {
            Some(body) if request.method != "GET" => url::form_urlencoded::parse(body)
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
            _ => Vec::new(),
        };
        Req {
            request,
            path: url.path().to_string(),
            query,
            post,
        }
    }

    fn method(&self) -> &str {
        &self.request.method
    }

    fn get(&self, key: &str) -> Option<String> {
        self.get_bytes(key)
            .map(|v| String::from_utf8_lossy(v).into_owned())
    }

    fn get_bytes(&self, key: &str) -> Option<&[u8]> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_slice())
    }

    fn get_or(&self, key: &str, default: &str) -> String {
        self.get(key).unwrap_or_else(|| default.to_string())
    }

    /// The Basic credentials of an `Authorization` header.
    fn basic_auth(&self) -> Option<(String, String)> {
        let header = self.header("authorization")?;
        let encoded = header.strip_prefix("Basic ")?.trim();
        let decoded = String::from_utf8(base64_decode(encoded)?).ok()?;
        let (user, password) = decoded.split_once(':')?;
        Some((user.to_string(), password.to_string()))
    }

    fn has(&self, key: &str) -> bool {
        self.query.iter().any(|(k, _)| k == key)
    }

    fn post(&self, key: &str) -> Option<&str> {
        self.post
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// A request header as the server sees it, including the ones the
    /// network host adds on the page's behalf.
    fn header(&self, name: &str) -> Option<String> {
        let found: Vec<&str> = self
            .request
            .headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect();
        if !found.is_empty() {
            return Some(found.join(", "));
        }
        match name.to_ascii_lowercase().as_str() {
            "content-length" => self.request.body.as_ref().map(|b| b.len().to_string()),
            "user-agent" => Some("CatPaw/wpt".to_string()),
            "accept" => Some("*/*".to_string()),
            "referer" => self.request.referrer.as_ref().map(Url::to_string),
            _ => None,
        }
    }

    fn header_or(&self, name: &str, default: &str) -> String {
        self.header(name).unwrap_or_else(|| default.to_string())
    }

    /// Every header, in order, with the ones the host adds.
    fn all_headers(&self) -> Vec<(String, String)> {
        let mut all = self.request.headers.clone();
        for name in ["user-agent", "accept", "referer", "content-length"] {
            if !all.iter().any(|(n, _)| n == name)
                && let Some(value) = self.header(name)
            {
                all.push((name.to_string(), value));
            }
        }
        all
    }

    fn body(&self) -> Vec<u8> {
        self.request.body.clone().unwrap_or_default()
    }

    /// The URL's directory, with its trailing slash.
    fn dir(&self) -> String {
        match self.path.rfind('/') {
            Some(i) => self.path[..=i].to_string(),
            None => "/".to_string(),
        }
    }

    /// The query string as sent, or `NO`, as the handlers print it.
    fn raw_query(&self) -> String {
        self.request
            .url
            .query()
            .filter(|q| !q.is_empty())
            .unwrap_or("NO")
            .to_string()
    }

    fn origin_of_url(&self) -> String {
        self.request.url.origin().ascii_serialization()
    }
}

/// The response for a `.py` handler the tests use, or `None` for anything
/// else (which is served as a file; for a `.py` that is a 404).
pub fn handle(request: &NetRequest, stash: &Stash) -> Option<Served> {
    let req = Req::new(request);
    if !req.path.ends_with(".py") {
        return None;
    }
    let served = match req.path.as_str() {
        "/common/redirect.py" => common_redirect(&req),
        "/fetch/api/resources/inspect-headers.py" => inspect_headers(&req),
        "/fetch/api/resources/redirect.py" => fetch_redirect(&req, stash),
        "/fetch/api/resources/status.py" | "/xhr/resources/status.py" => status(&req),
        "/fetch/api/resources/method.py" => fetch_method(&req),
        "/fetch/api/resources/echo-content.py" => echo_content(&req),
        "/fetch/api/resources/trickle.py" => trickle(&req),
        "/fetch/api/resources/dump-authorization-header.py" => dump_authorization(&req),
        "/fetch/api/resources/stash-put.py" => stash_put(&req, stash),
        "/fetch/api/resources/stash-take.py" => stash_take(&req, stash),
        "/fetch/api/resources/clean-stash.py" => clean_stash(&req, stash),
        "/fetch/api/resources/preflight.py" => preflight(&req, stash),
        "/fetch/api/resources/authentication.py" | "/xhr/resources/authentication.py" => {
            authentication(&req)
        }
        "/fetch/api/resources/script-with-header.py" => script_with_header(&req),
        "/fetch/api/resources/cache.py" => cache(&req),
        "/fetch/api/resources/infinite-slow-response.py" => infinite_slow(&req, stash),
        "/xhr/resources/content.py" => xhr_content(&req),
        "/xhr/resources/headers.py" => xhr_headers(),
        "/xhr/resources/inspect-headers.py" => xhr_inspect_headers(&req),
        "/xhr/resources/redirect.py" => xhr_redirect(&req),
        "/xhr/resources/delay.py" => delay(&req),
        "/xhr/resources/corsenabled.py" => corsenabled(&req),
        "/xhr/resources/echo-method.py" => Served::ok()
            .header("content-type", "text/plain")
            .text(req.method()),
        "/xhr/resources/access-control-basic-allow.py" => Served::ok()
            .header("content-type", "text/plain")
            .header(ACAC, "true")
            .header_opt(ACAO, req.header("origin"))
            .text(CORS_ALLOWED),
        "/xhr/resources/access-control-basic-allow-star.py" => Served::ok()
            .header("content-type", "text/plain")
            .header(ACAO, "*")
            .text(CORS_ALLOWED),
        "/xhr/resources/access-control-basic-denied.py" => Served::ok()
            .header("cache-control", "no-store")
            .header("content-type", "text/plain")
            .text("FAIL: Cross-domain access allowed."),
        "/xhr/resources/access-control-basic-put-allow.py" => put_allow(&req),
        "/xhr/resources/access-control-basic-allow-no-credentials.py" => Served::ok()
            .header("content-type", "text/plain")
            .header_opt(ACAO, req.header("origin"))
            .text(CORS_ALLOWED),
        "/xhr/resources/access-control-origin-header.py" => Served::ok()
            .header("content-type", "text/plain")
            .header("cache-control", "no-cache, no-store")
            .header("access-control-allow-external", "true")
            .header(ACAO, "*")
            .text(&format!(
                "{CORS_ALLOWED}\nHTTP_ORIGIN: {}",
                req.header_or("origin", "")
            )),
        "/xhr/resources/access-control-basic-options-not-supported.py" => {
            options_not_supported(&req)
        }
        "/xhr/resources/access-control-allow-with-body.py" => Served::ok()
            .header("cache-control", "no-store")
            .header(ACAH, "X-Requested-With")
            .header(ACMA, "0")
            .header(ACAO, "*")
            .header(ACAM, "*")
            .header("vary", "Accept-Encoding")
            .header("content-type", "text/plain")
            .text("PASS"),
        "/xhr/resources/access-control-preflight-request-header-returns-origin.py" => {
            preflight_header(&req, req.header("origin"), "X-Test")
        }
        "/xhr/resources/access-control-preflight-request-allow-headers-returns-star.py" => {
            preflight_header(&req, Some("*".to_string()), "*")
        }
        "/xhr/resources/access-control-preflight-request-header-lowercase.py" => {
            preflight_lowercase(&req)
        }
        "/xhr/resources/access-control-preflight-request-header-sorted.py" => {
            preflight_sorted(&req)
        }
        "/xhr/resources/access-control-preflight-request-headers-origin.py" => {
            preflight_headers_origin(&req)
        }
        "/xhr/resources/access-control-preflight-request-invalid-status.py" => {
            preflight_invalid_status(&req)
        }
        "/xhr/resources/access-control-preflight-request-must-not-contain-cookie.py" => {
            Served::ok()
                .header("cache-control", "no-store")
                .header_opt(ACAO, req.header("origin"))
                .header(ACAC, "true")
                .header(ACAH, "X-Proprietary-Header")
                .header("connection", "close")
        }
        "/xhr/resources/access-control-basic-cors-safelisted-request-headers.py" => {
            safelisted_request_headers(&req)
        }
        "/xhr/resources/access-control-basic-cors-safelisted-response-headers.py" => {
            Served::ok()
                .header("content-type", "text/plain")
                .header("cache-control", "no cache")
                .header("content-language", "en")
                .header("expires", "Fri, 30 Oct 1998 14:19:41 GMT")
                .header("last-modified", "Tue, 15 Nov 1994 12:45:26 GMT")
                .header("pragma", "no-cache")
                .header("x-test", "foobar")
                .header(ACAO, "*")
                .text(CORS_ALLOWED)
        }
        "/xhr/resources/access-control-preflight-denied.py" => preflight_denied(&req, stash),
        "/xhr/resources/no-custom-header-on-preflight.py" => no_custom_header(&req, stash),
        "/xhr/resources/echo-content-cors.py" => echo_content_cors(&req),
        "/xhr/resources/echo-content-type.py" => Served::ok()
            .header("content-type", "text/plain")
            .header("connection", "close")
            .text(&req.header_or("content-type", "")),
        "/xhr/resources/form.py" => Served::ok().text(&format!(
            "id:{};value:{};",
            req.post("id").unwrap_or_default(),
            req.post("value").unwrap_or_default()
        )),
        "/xhr/resources/infinite-redirects.py" => infinite_redirects(&req),
        "/xhr/resources/parse-headers.py" => {
            Served::ok().header_opt("my-custom-header", req.get("my-custom-header"))
        }
        "/xhr/resources/invalid-utf8.py" => Served::ok()
            .header("content-type", "application/json")
            .body(b"{\"key\":\"\xff\"}".to_vec()),
        "/xhr/resources/json-with-bom.py" => Served::ok()
            .header("content-type", "application/json")
            .body(b"\xef\xbb\xbf{\"key\":\"value\"}".to_vec()),
        "/xhr/resources/header-user-agent.py" => header_user_agent(&req),
        "/xhr/resources/accept.py" => Served::ok()
            .header("content-type", "text/plain")
            .text(&req.header_or("accept", "NO")),
        "/xhr/resources/accept-language.py" => Served::ok()
            .header("content-type", "text/plain")
            .text(&req.header_or("accept-language", "NO")),
        "/xhr/resources/chunked.py" => Served::ok()
            .header("content-type", "text/plain")
            .text("First chunk\r\nSecond chunk\r\nYet another (third) chunk\r\nYet another (fourth) chunk\r\n"),
        "/xhr/resources/conditional.py" => conditional(&req),
        "/xhr/resources/img-utf8-html.py" => Served::ok()
            .header("content-type", "text/html;charset=utf-8")
            .text("<img>foo"),
        "/xhr/resources/invalid-utf8-html.py" => Served::ok()
            .header("content-type", "text/html;charset=utf-8")
            .body(vec![0xff]),
        "/xhr/resources/empty-div-utf8-html.py" => Served::ok()
            .header("content-type", "text/html;charset=utf-8")
            .text("<!DOCTYPE html><div></div>"),
        "/xhr/resources/reset-token.py" => {
            if let Some(token) = req.get("token") {
                stash.put(&req.path, &token, "");
            }
            Served::ok()
                .header_opt(ACAO, req.header("origin"))
                .text("PASS")
        }
        "/xhr/resources/redirect-cors.py" => redirect_cors(&req),
        _ => return None,
    };
    Some(served)
}

fn common_redirect(req: &Req<'_>) -> Served {
    let status = req
        .get("status")
        .and_then(|s| s.parse().ok())
        .unwrap_or(302);
    let mut served = Served::ok()
        .code(status)
        .header("location", req.get_or("location", ""));
    if req.has("enable-cors")
        && let Some(origin) = req.header("origin")
    {
        served = served
            .header("content-type", "text/plain")
            .header(ACAO, origin)
            .header(ACAC, "true");
    }
    served
}

fn inspect_headers(req: &Req<'_>) -> Served {
    let mut served = Served::ok();
    let names = req.get("headers").unwrap_or_default();
    let checked: Vec<&str> = names.split('|').filter(|h| !h.is_empty()).collect();
    for name in &checked {
        if let Some(value) = req.header(name) {
            served = served.header(&format!("x-request-{name}"), value);
        }
    }
    if req.has("cors") {
        let exposed: Vec<String> = checked.iter().map(|h| format!("x-request-{h}")).collect();
        served = served
            .header(ACAO, req.header_or("origin", "*"))
            .header(ACAC, "true")
            .header(ACAM, "GET, POST, HEAD")
            .header(ACEH, exposed.join(", "));
        served = match req.get("allow_headers") {
            Some(allow) => served.header(ACAH, allow),
            None => {
                let names: Vec<String> = req.all_headers().into_iter().map(|(n, _)| n).collect();
                served.header(ACAH, names.join(", "))
            }
        };
    }
    served.header("content-type", "text/plain")
}

fn fetch_redirect(req: &Req<'_>, stash: &Stash) -> Served {
    let mut served = Served::ok()
        .header("content-type", "text/plain")
        .header("cache-control", "no-cache")
        .header("pragma", "no-cache")
        .allow_origin(req);
    let token = req.get("token");
    let (mut count, mut preflight) = (0u32, "0".to_string());
    if let Some(token) = &token
        && let Some(data) = stash.take(&req.path, token)
    {
        let data = decode_map(&data);
        count = data.get("count").and_then(|c| c.parse().ok()).unwrap_or(0);
        preflight = data.get("preflight").cloned().unwrap_or_default();
    }
    let remember = |count: u32, preflight: &str| {
        encode_map(&[
            ("count", Some(&count.to_string())),
            ("preflight", Some(preflight)),
        ])
    };
    if req.method() == "OPTIONS" {
        if let Some(allow) = req.get("allow_headers") {
            served = served.header(ACAH, allow);
        }
        preflight = "1".to_string();
        if !req.has("redirect_preflight") {
            if let Some(token) = &token {
                stash.put(&req.path, token, remember(count, &preflight));
            }
            return served;
        }
    }
    let status = req
        .get("redirect_status")
        .or_else(|| req.post("redirect_status").map(str::to_string))
        .and_then(|s| s.parse().ok())
        .unwrap_or(302);
    count += 1;
    if let Some(location) = req.get("location") {
        let mut url = location.to_string();
        if !req.has("simple") {
            let scheme = Url::parse(&url)
                .map(|u| u.scheme().to_string())
                .unwrap_or_default();
            if matches!(scheme.as_str(), "" | "http" | "https") {
                url.push(if url.contains('?') { '&' } else { '?' });
                let mut params = url::form_urlencoded::Serializer::new(String::new());
                let mut seen: Vec<&str> = Vec::new();
                for (k, v) in &req.query {
                    if !seen.contains(&k.as_str()) {
                        seen.push(k);
                        params.append_pair(k, &String::from_utf8_lossy(v));
                    }
                }
                url.push_str(&params.finish());
                url.push_str(&format!("&count={count}"));
            }
        }
        served = served.header("location", url);
    }
    if let Some(policy) = req.get("redirect_referrerpolicy") {
        served = served.header("referrer-policy", policy);
    }
    if let Some(ms) = req.get("delay").and_then(|d| d.parse::<f64>().ok()) {
        served = served.delay_ms(ms);
    }
    if let Some(token) = &token {
        stash.put(&req.path, token, remember(count, &preflight));
        if let Some(max) = req.get("max_count").and_then(|m| m.parse::<u32>().ok())
            && count > max
        {
            return Served::ok().text(&(count - 1).to_string());
        }
    }
    served.code(status)
}

fn status(req: &Req<'_>) -> Served {
    let code = req.get("code").and_then(|c| c.parse().ok()).unwrap_or(200);
    Served::ok()
        .status(code, &req.get_or("text", "OMG"))
        .header("content-type", req.get_or("type", ""))
        .header("x-request-method", req.method())
        .body(req.get_bytes("content").unwrap_or_default().to_vec())
}

fn fetch_method(req: &Req<'_>) -> Served {
    let mut served = Served::ok();
    if req.has("cors") {
        served = served
            .header(ACAO, "*")
            .header(ACAC, "true")
            .header(ACAM, "GET, POST, PUT, FOO")
            .header(ACAH, "x-test, x-foo")
            .header(ACEH, "x-request-method");
    }
    served
        .header("x-request-method", req.method())
        .header(
            "x-request-content-type",
            req.header_or("content-type", "NO"),
        )
        .header(
            "x-request-content-length",
            req.header_or("content-length", "NO"),
        )
        .header(
            "x-request-content-encoding",
            req.header_or("content-encoding", "NO"),
        )
        .header(
            "x-request-content-language",
            req.header_or("content-language", "NO"),
        )
        .header(
            "x-request-content-location",
            req.header_or("content-location", "NO"),
        )
        .body(req.body())
}

fn echo_content(req: &Req<'_>) -> Served {
    Served::ok()
        .header("x-request-method", req.method())
        .header(
            "x-request-content-length",
            req.header_or("content-length", "NO"),
        )
        .header(
            "x-request-content-type",
            req.header_or("content-type", "NO"),
        )
        .header("content-type", "text/plain")
        .body(req.body())
}

/// The body arrives whole here, after the delay the headers would take.
fn trickle(req: &Req<'_>) -> Served {
    let ms = req
        .get("ms")
        .and_then(|m| m.parse::<f64>().ok())
        .unwrap_or(500.0);
    let count = req.get("count").and_then(|c| c.parse().ok()).unwrap_or(50);
    let mut served = Served::ok().delay_ms(ms * 2.0);
    if !req.has("notype") {
        served = served.header("content-type", "text/plain");
    }
    served.text(&"TEST_TRICKLE\n".repeat(count))
}

fn dump_authorization(req: &Req<'_>) -> Served {
    let served = Served::ok()
        .header("content-type", "text/html")
        .header("cache-control", "no-cache");
    if req.has("strip_auth_header")
        && req.method() == "OPTIONS"
        && req
            .header_or("access-control-request-headers", "")
            .to_ascii_lowercase()
            .contains("authorization")
    {
        return served.code(500).text("fail");
    }
    let served = served.allow_origin(req).header(ACAH, "Authorization");
    match req.header("authorization") {
        Some(value) => served.text(&value),
        None => served.text("none"),
    }
}

fn stash_put(req: &Req<'_>, stash: &Stash) -> Served {
    if req.method() == "OPTIONS" {
        return Served::ok()
            .header(ACAO, "*")
            .header(ACAM, "*")
            .header(ACAH, "*")
            .text("done");
    }
    let mut served = Served::ok();
    if !req.has("disallow_cross_origin") {
        served = served.header(ACAO, "*");
    } else {
        let same_origin = req.get("mode").as_deref() == Some("no-cors")
            || req.get("frame_origin") == Some(req.origin_of_url());
        if !same_origin {
            return served.text("not stashing for cors request");
        }
    }
    stash.put(&req.dir(), &req.get_or("key", ""), req.get_or("value", ""));
    served.text("done")
}

fn stash_take(req: &Req<'_>, stash: &Stash) -> Served {
    let value = stash.take(&req.dir(), &req.get_or("key", ""));
    let json = match value {
        Some(value) => serde_json::Value::String(value).to_string(),
        None => "null".to_string(),
    };
    Served::ok()
        .header(ACAO, "*")
        .header("content-type", "application/json")
        .text(&json)
}

fn clean_stash(req: &Req<'_>, stash: &Stash) -> Served {
    let found = stash.take(&req.path, &req.get_or("token", "")).is_some();
    Served::ok().text(if found { "1" } else { "0" })
}

fn preflight(req: &Req<'_>, stash: &Stash) -> Served {
    let mut served = Served::ok().header("content-type", "text/plain");
    let token = req.get("token");
    served = match req.get("origin") {
        Some(origins) => origins
            .split(", ")
            .fold(served, |served, origin| served.header(ACAO, origin)),
        None => served.header(ACAO, "*"),
    };
    if req.has("clear-stash") {
        let found = token
            .as_deref()
            .and_then(|t| stash.take(&req.path, t))
            .is_some();
        return served.text(if found { "1" } else { "0" });
    }
    if req.has("credentials") {
        served = served.header(ACAC, "true");
    }
    if req.method() == "OPTIONS" {
        if req.header("access-control-request-method").is_none() {
            return served
                .code(400)
                .text("ERROR: No access-control-request-method in preflight!");
        }
        if req.header_or("accept", "") != "*/*" {
            return served.code(400).text("ERROR: Invalid access in preflight!");
        }
        let control_request_headers = if req.has("control_request_headers") {
            req.header("access-control-request-headers")
        } else {
            Some(String::new())
        };
        if let Some(max_age) = req.get("max_age") {
            served = served.header(ACMA, max_age);
        }
        if let Some(allow) = req.get("allow_headers") {
            served = served.header(ACAH, allow);
        }
        if let Some(allow) = req.get("allow_methods") {
            served = served.header(ACAM, allow);
        }
        let status = req
            .get("preflight_status")
            .and_then(|s| s.parse().ok())
            .unwrap_or(200);
        if let Some(token) = &token {
            stash.put(
                &req.path,
                token,
                encode_map(&[
                    (
                        "control_request_headers",
                        control_request_headers.as_deref(),
                    ),
                    ("preflight", Some("1")),
                    ("preflight_referrer", Some(&req.header_or("referer", ""))),
                    (
                        "preflight_user_agent",
                        Some(&req.header_or("user-agent", "")),
                    ),
                ]),
            );
        }
        return served.code(status);
    }
    let mut data: HashMap<String, String> = decode_map(&encode_map(&[
        ("control_request_headers", Some("")),
        ("preflight", Some("0")),
        ("preflight_referrer", Some("")),
    ]));
    if let Some(token) = &token
        && let Some(stored) = stash.take(&req.path, token)
    {
        data = decode_map(&stored);
    }
    if req.has("checkUserAgentHeaderInPreflight")
        && req.header("user-agent") != data.get("preflight_user_agent").cloned()
    {
        return served
            .code(400)
            .text("ERROR: No user-agent header in preflight");
    }
    served = served
        .header(
            ACEH,
            "x-did-preflight, x-control-request-headers, x-referrer, x-preflight-referrer, x-origin",
        )
        .header("x-did-preflight", data.get("preflight").cloned().unwrap_or_default())
        .header_opt("x-control-request-headers", data.get("control_request_headers").cloned())
        .header(
            "x-preflight-referrer",
            data.get("preflight_referrer").cloned().unwrap_or_default(),
        )
        .header("x-referrer", req.header_or("referer", ""))
        .header("x-origin", req.header_or("origin", ""));
    if let Some(token) = &token {
        let entries: Vec<(&str, Option<&str>)> = data
            .iter()
            .map(|(k, v)| (k.as_str(), Some(v.as_str())))
            .collect();
        stash.put(&req.path, token, encode_map(&entries));
    }
    served
}

/// Accepts the one Basic credential the tests use; any other request gets
/// the challenge (there is no authentication dialog to answer it).
fn authentication(req: &Req<'_>) -> Served {
    if req.basic_auth() == Some(("user".to_string(), "password".to_string())) {
        return Served::ok().text("Authentication done");
    }
    let realm = req.get_or("realm", "test");
    Served::ok()
        .code(401)
        .header("www-authenticate", format!("Basic realm=\"{realm}\""))
        .text("Please login with credentials 'user' and 'password'")
}

fn script_with_header(req: &Req<'_>) -> Served {
    let served = Served::ok().header("content-type", req.get_or("mime", ""));
    if req.get("content").as_deref() == Some("empty") {
        served
    } else {
        served.text("console.log('Script loaded')")
    }
}

fn cache(req: &Req<'_>) -> Served {
    const ETAG: &str = "\"123abc\"";
    if req.header("if-none-match").as_deref() == Some(ETAG) {
        return Served::ok().code(304).header("x-http-status", "304");
    }
    Served::ok()
        .header("etag", ETAG)
        .header("content-type", "text/plain")
        .text("lorem ipsum dolor sit amet")
}

/// Never finishes: the request stays in flight until it is aborted.
fn infinite_slow(req: &Req<'_>, stash: &Stash) -> Served {
    if let Some(key) = req.get("stateKey").filter(|k| !k.is_empty()) {
        stash.put(&req.dir(), &key, "open");
    }
    let mut served = Served::ok()
        .header("content-type", "text/plain")
        .text(&".".repeat(2048));
    served.delay = Duration::from_secs(3600);
    served
}

fn xhr_content(req: &Req<'_>) -> Served {
    let charset = req
        .get("response_charset_label")
        .map(|label| format!(";charset={label}"))
        .unwrap_or_default();
    let served = Served::ok()
        .header("content-type", format!("text/plain{charset}"))
        .header("x-request-method", req.method())
        .header("x-request-query", req.raw_query())
        .header(
            "x-request-content-length",
            req.header_or("content-length", "NO"),
        )
        .header(
            "x-request-content-type",
            req.header_or("content-type", "NO"),
        );
    match req.get_bytes("content") {
        Some(content) => served.body(content.to_vec()),
        None => served.body(req.body()),
    }
}

fn xhr_headers() -> Served {
    Served::ok()
        .header("content-type", "text/plain")
        .header("x-custom-header", "test")
        .header("set-cookie", "test")
        .header("set-cookie2", "test")
        .header("x-custom-header-empty", "")
        .header("x-custom-header-comma", "1")
        .header("x-custom-header-comma", "2")
        .header("x-custom-header-bytes", "…")
        .text("TEST")
}

fn xhr_inspect_headers(req: &Req<'_>) -> Served {
    let mut served = Served::ok();
    if req.has("cors") {
        served = served
            .header(ACAO, "*")
            .header(ACAC, "true")
            .header(ACAM, "GET, POST, PUT, FOO")
            .header(ACAH, "x-test, x-foo")
            .header(
                ACEH,
                "x-request-method, x-request-content-type, x-request-query, x-request-content-length",
            );
    }
    let filter_value = req.get_or("filter_value", "");
    let filter_name = req.get_or("filter_name", "").to_ascii_lowercase();
    let mut result = String::new();
    for (name, value) in req.all_headers() {
        if !filter_value.is_empty() {
            if value == filter_value {
                result.push_str(&format!("{name},"));
            }
        } else if name.to_ascii_lowercase() == filter_name {
            result.push_str(&format!("{name}: {value}\n"));
        }
    }
    served.header("content-type", "text/plain").text(&result)
}

fn xhr_redirect(req: &Req<'_>) -> Served {
    let code = req.get("code").and_then(|c| c.parse().ok()).unwrap_or(302);
    let mut location = req
        .get("location")
        .unwrap_or_else(|| format!("{}?followed", req.path));
    // The handler runs the value through a query parser once more.
    location = url::form_urlencoded::parse(format!("location={location}").as_bytes())
        .find(|(k, _)| k == "location")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    if location.starts_with("redirect.py") {
        location.push_str(&format!("&code={code}"));
    }
    let mut served = Served::ok();
    if let Some(ms) = req.get("delay").and_then(|d| d.parse::<f64>().ok()) {
        served = served.delay_ms(ms);
    }
    if req.has("followed") {
        // The original misspells the header name; so does this.
        return served
            .header("content:type", "text/plain")
            .text("MAGIC HAPPENED");
    }
    served
        .status(code, "WEBSRT MARKETING")
        .header("location", location)
        .text("TEST")
}

fn delay(req: &Req<'_>) -> Served {
    let ms = req
        .get("ms")
        .and_then(|m| m.parse::<f64>().ok())
        .unwrap_or(500.0);
    Served::ok()
        .delay_ms(ms)
        .header(ACAO, "*")
        .header(ACAM, "YO")
        .header("content-type", "text/plain")
        .text("TEST_DELAY")
}

fn corsenabled(req: &Req<'_>) -> Served {
    let mut served = Served::ok()
        .header(ACAO, "*")
        .header(ACAC, "true")
        .header(ACAM, "GET, POST, PUT, FOO")
        .header(ACAH, "x-test, x-foo")
        .header(
            ACEH,
            "x-request-method, x-request-content-type, x-request-query, x-request-content-length, x-request-data",
        );
    if let Some(seconds) = req.get("delay").and_then(|d| d.parse::<f64>().ok()) {
        served = served.delay_ms(seconds * 1000.0);
    }
    if req.has("safelist_content_type") {
        served = served.header(ACAH, "content-type");
    }
    served
        .header("x-request-method", req.method())
        .header("x-request-query", req.raw_query())
        .header(
            "x-request-content-length",
            req.header_or("content-length", "NO"),
        )
        .header(
            "x-request-content-type",
            req.header_or("content-type", "NO"),
        )
        .header(
            "x-request-data",
            String::from_utf8_lossy(&req.body()).into_owned(),
        )
        .text("Test")
}

fn put_allow(req: &Req<'_>) -> Served {
    let served = Served::ok().header("content-type", "text/plain");
    match req.method() {
        "OPTIONS" => served
            .header(ACAC, "true")
            .header(ACAM, "PUT")
            .header_opt(ACAO, req.header("origin")),
        "PUT" => {
            let mut body = format!("{CORS_ALLOWED}\n").into_bytes();
            body.extend(req.body());
            served
                .header(ACAC, "true")
                .header_opt(ACAO, req.header("origin"))
                .body(body)
        }
        method => served.text(&format!("Wrong method: {method}")),
    }
}

fn options_not_supported(req: &Req<'_>) -> Served {
    let served = Served::ok().header("cache-control", "no-store");
    if req.method() == "OPTIONS" {
        return served.code(400);
    }
    match req.header("origin") {
        Some(origin) => served.header(ACAC, "true").header(ACAO, origin),
        None => served.code(500),
    }
}

/// A preflight that allows `allowed` from `origin`; the request then
/// passes only with an `X-Test` header.
fn preflight_header(req: &Req<'_>, origin: Option<String>, allowed: &str) -> Served {
    match req.method() {
        "OPTIONS" => Served::ok().header_opt(ACAO, origin).header(ACAH, allowed),
        "GET" => {
            let served = Served::ok().header(ACAO, "*");
            if req.header("x-test").is_some() {
                served.header("content-type", "text/plain").text("PASS")
            } else {
                served.code(400)
            }
        }
        _ => Served::ok(),
    }
}

fn preflight_lowercase(req: &Req<'_>) -> Served {
    let served = Served::ok()
        .header("cache-control", "no-store")
        .header(ACAO, "*")
        .header(ACMA, "0");
    match req.method() {
        "OPTIONS" => {
            let asked = req.header_or("access-control-request-headers", "");
            if asked.split(',').any(|h| h.trim() == "x-test") {
                served.header(ACAH, "X-Test")
            } else {
                served.code(400)
            }
        }
        "GET" if req.header("x-test").is_some() => served.text("PASS"),
        "GET" => served.code(400),
        _ => served,
    }
}

fn preflight_sorted(req: &Req<'_>) -> Served {
    const HEADERS: &str = "x-custom-s,x-custom-test,x-custom-u,x-custom-ua,x-custom-v";
    let served = Served::ok()
        .header("cache-control", "no-store")
        .header_opt(ACAO, req.header("origin"));
    if req.method() == "OPTIONS" {
        let served = served.header(ACMA, "0").header(ACAH, HEADERS);
        if req.header("access-control-request-headers").as_deref() != Some(HEADERS) {
            return served.code(400);
        }
        served
    } else if req.header("x-custom-s").is_some() {
        served.text("PASS")
    } else {
        served.code(400).text("FAIL")
    }
}

fn preflight_headers_origin(req: &Req<'_>) -> Served {
    let served = Served::ok()
        .header("cache-control", "no-store")
        .header(ACAO, "*");
    if req.method() == "OPTIONS" {
        if req
            .header_or("access-control-request-headers", "")
            .to_ascii_lowercase()
            .contains("origin")
        {
            served
                .code(400)
                .text("Error: 'origin' included in Access-Control-Request-Headers")
        } else {
            served.header(ACAH, "x-pass")
        }
    } else {
        served.text(&req.header_or("x-pass", ""))
    }
}

fn preflight_invalid_status(req: &Req<'_>) -> Served {
    let mut served = Served::ok();
    if req.method() == "OPTIONS" {
        if let Some(code) = req.get("code").and_then(|c| c.parse().ok()) {
            served = served.code(code);
        }
        served = served.header(ACMA, "1").header(ACAH, "x-pass");
    }
    served
        .header("cache-control", "no-store")
        .header_opt(ACAO, req.header("origin"))
}

fn safelisted_request_headers(req: &Req<'_>) -> Served {
    let served = Served::ok().header("cache-control", "no-store");
    if req.method() != "POST" {
        return served.code(400);
    }
    let mut body = String::new();
    for name in [
        "Accept",
        "Accept-Language",
        "Content-Language",
        "Content-Type",
    ] {
        let value = req.header(name).unwrap_or_else(|| "<None>".to_string());
        body.push_str(&format!("{name}: {value}\n"));
    }
    served
        .header(ACAC, "true")
        .header_opt(ACAO, req.header("origin"))
        .text(&body)
}

fn preflight_denied(req: &Req<'_>, stash: &Stash) -> Served {
    let served = Served::ok()
        .header("cache-control", "no-store")
        .header_opt(ACAO, req.header("origin"))
        .header(ACMA, "1");
    let token = req.get_or("token", "");
    let state = stash
        .take(&req.path, &token)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Uninitialized".to_string());
    let command = req.get("command");
    let fail = |served: Served, message: &str| served.code(400).text(&format!("FAIL: {message}"));
    match (command.as_deref(), state.as_str(), req.method()) {
        (Some("reset"), _, "GET") => {
            stash.put(&req.path, &token, "");
            served.text("Server state reset")
        }
        (Some("reset"), _, _) => fail(served, "Invalid Method."),
        (_, "Uninitialized", "OPTIONS") => {
            stash.put(&req.path, &token, "Denied");
            served.text("This request should not be displayed.")
        }
        (_, "Uninitialized", _) => fail(served, &state),
        (Some("complete"), "Denied", "GET") => {
            stash.put(&req.path, &token, "");
            served.text("Request successfully blocked.")
        }
        (_, "Denied", _) => {
            stash.put(&req.path, &token, "Deny Ignored");
            fail(served, "The request was not denied.")
        }
        (_, "Deny Ignored", _) => {
            stash.put(&req.path, &token, "");
            fail(served, &state)
        }
        _ => {
            stash.put(&req.path, &token, "");
            fail(served, "Unknown Error.")
        }
    }
}

fn no_custom_header(req: &Req<'_>, stash: &Stash) -> Served {
    let served = Served::ok()
        .header(ACAO, "*")
        .header(ACAH, "x-test")
        .header(ACMA, "0");
    let token = req.get_or("token", "");
    if req.method() == "OPTIONS" {
        if req.header("x-test").is_some() {
            served
                .code(400)
                .text("FAIL: Invalid header in preflight request.")
        } else {
            stash.put(&req.path, &token, "PASS");
            served
        }
    } else if req.header("x-test").is_some() {
        let state = stash
            .take(&req.path, &token)
            .unwrap_or_else(|| "Uninitialized".to_string());
        served.text(&state)
    } else {
        served
            .code(400)
            .text("FAIL: X-Test header missing in request")
    }
}

fn echo_content_cors(req: &Req<'_>) -> Served {
    let mut served = Served::ok()
        .header("x-request-method", req.method())
        .header(
            "x-request-content-length",
            req.header_or("content-length", "NO"),
        )
        .header(
            "x-request-content-type",
            req.header_or("content-type", "NO"),
        )
        .header(ACAC, "true")
        .header("content-type", "text/plain");
    let from_query = req.get("origin");
    if let Some(origin) = from_query.clone().or_else(|| req.header("origin")) {
        served = served.header(ACAO, origin);
    }
    if let Some(headers) = from_query
        .clone()
        .or_else(|| req.header("access-control-request-headers"))
    {
        served = served.header(ACAH, headers);
    }
    if let Some(method) = from_query.or_else(|| req.header("access-control-request-method")) {
        served = served.header(ACAM, format!("OPTIONS, {method}"));
    }
    served.body(req.body())
}

fn infinite_redirects(req: &Req<'_>) -> Served {
    let url = &req.request.url;
    let location = format!(
        "{}://{}{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.port()
            .map(|p| format!(":{p}{}", url.path()))
            .unwrap_or_else(|| url.path().to_string())
    );
    let mut page = "alternate";
    let mut kind = 302;
    let mut mix = 0;
    if req.get("page").as_deref() == Some("alternate") {
        page = "default";
    }
    if req.get("type").as_deref() == Some("301") {
        kind = 301;
    }
    if req.get("mix").as_deref() == Some("1") {
        mix = 1;
        kind = if kind == 301 { 302 } else { 301 };
    }
    let next = format!("{location}?page={page}&type={kind}&mix={mix}");
    Served::ok()
        .code(301)
        .header("cache-control", "no-cache")
        .header("pragma", "no-cache")
        .header("location", next.clone())
        .text(&format!("Hello guest. You have been redirected to {next}"))
}

fn header_user_agent(req: &Req<'_>) -> Served {
    let served = Served::ok()
        .header(ACAO, "*")
        .header(ACMA, "0")
        .header(ACAH, "x-test");
    let has_agent = req.header("user-agent").is_some_and(|ua| !ua.is_empty());
    match (req.method(), has_agent) {
        ("OPTIONS", false) => served
            .code(400)
            .text("FAIL: User-Agent header missing in preflight request."),
        ("OPTIONS", true) => served,
        (_, true) => served.text("PASS"),
        (_, false) => served
            .code(400)
            .text("FAIL: User-Agent header missing in request"),
    }
}

fn conditional(req: &Req<'_>) -> Served {
    let tag = req.get("tag");
    let date = req.get_or("date", "");
    let cors = req.has("cors");
    if req.method() == "OPTIONS" {
        return Served::ok().header(ACAO, "*").header(ACAH, "IF-NONE-MATCH");
    }
    let mut served = Served::ok();
    if let Some(tag) = &tag {
        served = served.header("etag", format!("\"{tag}\""));
    } else if !date.is_empty() {
        served = served.header("last-modified", date.clone());
    }
    if cors {
        served = served.header(ACAO, "*");
    }
    let matched = (tag.is_some() && req.header("if-none-match") == tag)
        || (!date.is_empty() && req.header("if-modified-since").as_deref() == Some(date.as_str()));
    if matched {
        return served.status(304, "SUPERCOOL");
    }
    if !cors {
        served = served.header(ACAO, "*");
    }
    served
        .header("content-type", "text/plain")
        .text("MAYBE NOT")
}

fn redirect_cors(req: &Req<'_>) -> Served {
    let location = req.get_or("location", "");
    let mut served = Served::ok();
    match req.method() {
        "OPTIONS" => {
            if req.has("redirect_preflight") {
                served = served.code(302).header("location", location);
            }
            served = served.header(ACAM, "GET").header(ACMA, "1");
        }
        "GET" => served = served.code(302).header("location", location),
        _ => {}
    }
    if req.has("allow_origin") {
        served = served.header_opt(ACAO, req.header("origin"));
    }
    if let Some(allow) = req.get("allow_header") {
        served = served.header(ACAH, allow);
    }
    served
}
