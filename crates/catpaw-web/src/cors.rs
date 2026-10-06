//! Request preparation and response checks shared by `fetch()` and
//! `XMLHttpRequest`: which requests carry credentials, which need a CORS
//! preflight, and which responses script is allowed to read.
//!
//! This is the part of the Fetch standard that keeps a page from reading
//! another origin's data with the user's cookies. It is deliberately strict
//! where it simplifies: anything it cannot vouch for is a network error.

use std::cell::Cell;
use std::rc::Rc;

use url::{Origin, Url};

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

/// A request as script described it.
pub struct Outgoing {
    pub method: String,
    pub url: Url,
    /// Author headers, lowercase names, forbidden ones already dropped.
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub mode: Mode,
    pub credentials: Credentials,
    pub kind: RequestKind,
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
}

/// A response that passed the checks, filtered down to what is readable.
pub struct Readable {
    pub response: NetResponse,
    pub exposure: Exposure,
}

/// What the response checks need to remember about the request.
#[derive(Clone)]
struct Policy {
    page_origin: Origin,
    /// The serialized origin, as sent in `Origin` and matched against
    /// `Access-Control-Allow-Origin`.
    origin: String,
    mode: Mode,
    with_credentials: bool,
    method: String,
    /// Author header names that are not CORS-safelisted.
    unsafe_headers: Vec<String>,
}

struct Plan {
    request: NetRequest,
    preflight: Option<NetRequest>,
    policy: Policy,
}

/// <https://fetch.spec.whatwg.org/#forbidden-request-header>
pub fn is_forbidden_request_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
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
            | "user-agent"
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

fn is_safelisted_method(method: &str) -> bool {
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

/// <https://fetch.spec.whatwg.org/#cors-safelisted-request-header>
fn is_safelisted_request_header(name: &str, value: &str) -> bool {
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

fn plan(page: &PageState, out: Outgoing) -> Result<Plan, String> {
    if !matches!(out.url.scheme(), "http" | "https") {
        return Err(format!(
            "URL scheme \"{}\" is not supported",
            out.url.scheme()
        ));
    }
    let page_url = page.url.borrow().clone();
    let page_origin = page_url.origin();
    let origin = page_origin.ascii_serialization();
    let cross = out.url.origin() != page_origin;
    if cross && out.mode == Mode::SameOrigin {
        return Err(format!(
            "the request mode is \"same-origin\" but {} is cross-origin",
            out.url
        ));
    }
    if out.mode == Mode::NoCors && !is_safelisted_method(&out.method) {
        return Err(format!(
            "method {} is not allowed in \"no-cors\" mode",
            out.method
        ));
    }
    let with_credentials = match out.credentials {
        Credentials::Omit => false,
        Credentials::SameOrigin => !cross,
        Credentials::Include => true,
    };

    let mut headers = out.headers;
    if out.mode == Mode::NoCors {
        headers.retain(|(name, value)| is_safelisted_request_header(name, value));
    }
    let mut unsafe_headers: Vec<String> = headers
        .iter()
        .filter(|(name, value)| !is_safelisted_request_header(name, value))
        .map(|(name, _)| name.clone())
        .collect();
    unsafe_headers.sort();
    unsafe_headers.dedup();

    let cors = cross && out.mode == Mode::Cors;
    if cors || !matches!(out.method.as_str(), "GET" | "HEAD") {
        headers.push(("origin".to_string(), origin.clone()));
    }

    let preflight = (cors && (!is_safelisted_method(&out.method) || !unsafe_headers.is_empty()))
        .then(|| {
            let mut headers = vec![
                ("origin".to_string(), origin.clone()),
                (
                    "access-control-request-method".to_string(),
                    out.method.clone(),
                ),
            ];
            if !unsafe_headers.is_empty() {
                headers.push((
                    "access-control-request-headers".to_string(),
                    unsafe_headers.join(","),
                ));
            }
            NetRequest {
                method: "OPTIONS".to_string(),
                url: out.url.clone(),
                headers,
                body: None,
                kind: out.kind,
                referrer: Some(page_url.clone()),
                credentials: false,
            }
        });

    Ok(Plan {
        request: NetRequest {
            method: out.method.clone(),
            url: out.url,
            headers,
            body: out.body,
            kind: out.kind,
            referrer: Some(page_url),
            credentials: with_credentials,
        },
        preflight,
        policy: Policy {
            page_origin,
            origin,
            mode: out.mode,
            with_credentials,
            method: out.method,
            unsafe_headers,
        },
    })
}

/// <https://fetch.spec.whatwg.org/#concept-cors-check>
fn cors_check(policy: &Policy, response: &NetResponse) -> Result<(), String> {
    let allowed = response
        .header("access-control-allow-origin")
        .map(str::trim)
        .ok_or("the response has no Access-Control-Allow-Origin header")?;
    if allowed == "*" {
        return if policy.with_credentials {
            Err(
                "Access-Control-Allow-Origin must not be \"*\" for a request with credentials"
                    .to_string(),
            )
        } else {
            Ok(())
        };
    }
    if allowed != policy.origin {
        return Err(format!(
            "Access-Control-Allow-Origin is \"{allowed}\", which does not match the origin {}",
            policy.origin
        ));
    }
    if policy.with_credentials
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

fn check_preflight(policy: &Policy, response: &NetResponse) -> Result<(), String> {
    if !response.is_success() {
        return Err(format!(
            "the preflight response has status {}",
            response.status
        ));
    }
    cors_check(policy, response)?;
    let wildcard_ok = !policy.with_credentials;
    let methods = header_list(response, "access-control-allow-methods");
    let method_allowed = is_safelisted_method(&policy.method)
        || methods.contains(&policy.method)
        || (wildcard_ok && methods.iter().any(|m| m == "*"));
    if !method_allowed {
        return Err(format!(
            "method {} is not allowed by Access-Control-Allow-Methods",
            policy.method
        ));
    }
    let allowed: Vec<String> = header_list(response, "access-control-allow-headers")
        .into_iter()
        .map(|h| h.to_ascii_lowercase())
        .collect();
    for name in &policy.unsafe_headers {
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

/// Decides what of `response` script may read.
fn check_response(policy: &Policy, mut response: NetResponse) -> Result<Readable, String> {
    // Redirects are followed by the network layer; the origin that matters
    // is the one that actually answered.
    let cross = response.url.origin() != policy.page_origin;
    response.headers.retain(|(name, _)| {
        !matches!(
            name.to_ascii_lowercase().as_str(),
            "set-cookie" | "set-cookie2"
        )
    });
    if !cross {
        return Ok(Readable {
            response,
            exposure: Exposure::Basic,
        });
    }
    match policy.mode {
        Mode::SameOrigin => Err(format!(
            "the request was redirected to another origin ({})",
            response.url
        )),
        Mode::NoCors => {
            response.status = 0;
            response.status_text.clear();
            response.headers.clear();
            response.body.clear();
            Ok(Readable {
                response,
                exposure: Exposure::Opaque,
            })
        }
        Mode::Cors => {
            cors_check(policy, &response)?;
            let exposed: Vec<String> = header_list(&response, "access-control-expose-headers")
                .into_iter()
                .map(|h| h.to_ascii_lowercase())
                .collect();
            let expose_all = !policy.with_credentials && exposed.iter().any(|h| h == "*");
            response.headers.retain(|(name, _)| {
                let name = name.to_ascii_lowercase();
                expose_all
                    || SAFELISTED_RESPONSE_HEADERS.contains(&name.as_str())
                    || exposed.contains(&name)
            });
            Ok(Readable {
                response,
                exposure: Exposure::Cors,
            })
        }
    }
}

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
    if let Some(result) = crate::net::data_url_response(&out.url) {
        crate::event_loop::queue_task(page, "data URL", move |cx| {
            done(
                cx,
                result.map(|response| Readable {
                    response,
                    exposure: Exposure::Basic,
                }),
            )
        });
        return pending;
    }
    let plan = match plan(page, out) {
        Ok(plan) => plan,
        Err(reason) => {
            crate::event_loop::queue_task(page, "request rejected", move |cx| {
                done(cx, Err(reason))
            });
            return pending;
        }
    };
    let Plan {
        request,
        preflight,
        policy,
    } = plan;

    match preflight {
        None => start_actual(page, &pending, request, policy, done),
        Some(preflight) => {
            // The actual request only goes out once the server has agreed.
            let after = pending.clone();
            let token = start_request(page, preflight, move |cx, result| {
                match result.and_then(|r| check_preflight(&policy, &r)) {
                    Ok(()) => start_actual(cx.page, &after, request, policy, done),
                    Err(reason) => done(cx, Err(format!("CORS preflight failed: {reason}"))),
                }
            });
            pending.token.set(token);
        }
    }
    pending
}

type Done = Box<dyn FnOnce(&mut Cx<'_>, Result<Readable, String>)>;

fn start_actual(
    page: &PageState,
    pending: &Pending,
    request: NetRequest,
    policy: Policy,
    done: Done,
) {
    let token = start_request(page, request, move |cx, result| {
        done(cx, result.and_then(|r| check_response(&policy, r)));
    });
    pending.token.set(token);
}

/// Sends `out` and waits for the response (synchronous `XMLHttpRequest`).
pub fn send_blocking(page: &PageState, out: Outgoing) -> Result<Readable, String> {
    if let Some(result) = crate::net::data_url_response(&out.url) {
        return result.map(|response| Readable {
            response,
            exposure: Exposure::Basic,
        });
    }
    let plan = plan(page, out)?;
    let net = page.net().ok_or("no network available")?;
    if let Some(preflight) = plan.preflight {
        let response = net.fetch_blocking(preflight)?;
        check_preflight(&plan.policy, &response)?;
    }
    let response = net.fetch_blocking(plan.request)?;
    check_response(&plan.policy, response)
}
