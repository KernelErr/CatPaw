//! The Fetch API: `fetch()`, `Headers`, `Request`, `Response` and the
//! `Body` mixin (<https://fetch.spec.whatwg.org/#fetch-api>).
//!
//! Bodies are byte buffers: a response is delivered once it has arrived in
//! full, and streams are not exposed yet.

use std::rc::Rc;

use catpaw_js::{Exception, Fallible, ObjectId, PromiseRef, Value};
use url::Url;

use crate::abort::{self, AbortAlgorithm};
use crate::cors::{self, Credentials, Exposure, Mode, Outgoing, Readable};
use crate::generated::{
    self as web, ReadableStreamOrBufferSourceOrURLSearchParamsOrString as BodyInit, ReferrerPolicy,
    RequestCache, RequestCredentials, RequestDestination, RequestInit, RequestMode,
    RequestOrString, RequestRedirect, ResponseInit, ResponseType,
    StringSequenceSequenceOrStringStringRecord as HeadersInit,
};
use crate::net::RequestKind;
use crate::page::{ConsoleLevel, Cx, PageState};
use crate::{Web, platform_object};

type HeaderList = Vec<(String, String)>;

// ---- Headers ---------------------------------------------------------------

/// Where a `Headers` object keeps its list.
#[derive(Clone, Copy)]
enum HeaderStore {
    /// `new Headers()`: its own list (in the object).
    Own,
    /// `request.headers`.
    Request(ObjectId),
    /// `response.headers`.
    Response(ObjectId),
}

pub struct HeadersObject {
    store: HeaderStore,
    list: HeaderList,
}
platform_object!(HeadersObject, Headers);

fn invalid_header(what: &str, text: &str) -> Exception {
    Exception::type_error(format!("'{text}' is not a valid header {what}"))
}

/// Validates and normalizes one header for a header list.
fn checked_header(name: &str, value: &str) -> Fallible<(String, String)> {
    if !cors::is_header_name(name) {
        return Err(invalid_header("name", name));
    }
    let value =
        cors::normalize_header_value(value).ok_or_else(|| invalid_header("value", value))?;
    Ok((name.to_ascii_lowercase(), value))
}

fn header_list_from_init(init: Option<HeadersInit>) -> Fallible<HeaderList> {
    let mut list = Vec::new();
    match init {
        None => {}
        Some(HeadersInit::StringSequenceSequence(pairs)) => {
            for pair in pairs {
                let [name, value] = <[String; 2]>::try_from(pair)
                    .map_err(|_| Exception::type_error("Each header must be a name and a value"))?;
                list.push(checked_header(&name, &value)?);
            }
        }
        Some(HeadersInit::StringStringRecord(record)) => {
            for (name, value) in record {
                list.push(checked_header(&name, &value)?);
            }
        }
    }
    Ok(list)
}

/// The combined value of `name` in a list.
fn header_get(list: &HeaderList, name: &str) -> Option<String> {
    let name = name.to_ascii_lowercase();
    let values: Vec<&str> = list
        .iter()
        .filter(|(n, _)| *n == name)
        .map(|(_, v)| v.as_str())
        .collect();
    (!values.is_empty()).then(|| values.join(", "))
}

/// Runs `f` on the list behind a `Headers` object. `mutable` says whether
/// the list may be changed through this object.
fn with_headers<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut HeaderList, bool) -> R,
) -> Fallible<R> {
    let store = cx.page.with::<HeadersObject, _>(this, |h| h.store)?;
    match store {
        HeaderStore::Own => cx
            .page
            .with::<HeadersObject, _>(this, |h| f(&mut h.list, true)),
        HeaderStore::Request(owner) => cx
            .page
            .with::<RequestObject, _>(owner, |r| f(&mut r.headers, true)),
        HeaderStore::Response(owner) => cx.page.with::<ResponseObject, _>(owner, |r| {
            let mutable = !r.from_network;
            f(&mut r.headers, mutable)
        }),
    }
}

fn immutable() -> Exception {
    Exception::type_error("These headers are immutable")
}

impl web::HeadersImpl for Web {
    fn append(cx: &mut Cx<'_>, this: ObjectId, name: String, value: String) -> Fallible<()> {
        let header = checked_header(&name, &value)?;
        with_headers(cx, this, |list, mutable| {
            if !mutable {
                return Err(immutable());
            }
            list.push(header);
            Ok(())
        })?
    }

    fn delete(cx: &mut Cx<'_>, this: ObjectId, name: String) -> Fallible<()> {
        if !cors::is_header_name(&name) {
            return Err(invalid_header("name", &name));
        }
        let name = name.to_ascii_lowercase();
        with_headers(cx, this, |list, mutable| {
            if !mutable {
                return Err(immutable());
            }
            list.retain(|(n, _)| *n != name);
            Ok(())
        })?
    }

    fn get(cx: &mut Cx<'_>, this: ObjectId, name: String) -> Fallible<Option<String>> {
        if !cors::is_header_name(&name) {
            return Err(invalid_header("name", &name));
        }
        with_headers(cx, this, |list, _| header_get(list, &name))
    }

    fn get_set_cookie(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<String>> {
        with_headers(cx, this, |list, _| {
            list.iter()
                .filter(|(n, _)| n == "set-cookie")
                .map(|(_, v)| v.clone())
                .collect()
        })
    }

    fn has(cx: &mut Cx<'_>, this: ObjectId, name: String) -> Fallible<bool> {
        if !cors::is_header_name(&name) {
            return Err(invalid_header("name", &name));
        }
        let name = name.to_ascii_lowercase();
        with_headers(cx, this, |list, _| list.iter().any(|(n, _)| *n == name))
    }

    fn set(cx: &mut Cx<'_>, this: ObjectId, name: String, value: String) -> Fallible<()> {
        let (name, value) = checked_header(&name, &value)?;
        with_headers(cx, this, |list, mutable| {
            if !mutable {
                return Err(immutable());
            }
            match list.iter().position(|(n, _)| *n == name) {
                Some(first) => {
                    list[first].1 = value;
                    let mut index = 0;
                    list.retain(|(n, _)| {
                        let keep = index <= first || *n != name;
                        index += 1;
                        keep
                    });
                }
                None => list.push((name, value)),
            }
            Ok(())
        })?
    }

    fn constructor(cx: &mut Cx<'_>, init: Option<HeadersInit>) -> Fallible<ObjectId> {
        let list = header_list_from_init(init)?;
        Ok(cx.page.alloc(HeadersObject {
            store: HeaderStore::Own,
            list,
        }))
    }

    fn iterate(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<(String, String)>> {
        // Sorted by name, values of the same name combined.
        with_headers(cx, this, |list, _| {
            let mut names: Vec<&String> = list.iter().map(|(n, _)| n).collect();
            names.sort();
            names.dedup();
            names
                .into_iter()
                .filter_map(|name| header_get(list, name).map(|value| (name.clone(), value)))
                .collect()
        })
    }
}

// ---- bodies ----------------------------------------------------------------

/// The bytes of a body given by script, and the Content-Type it implies.
fn extract_body(cx: &Cx<'_>, body: BodyInit) -> Fallible<(Vec<u8>, Option<&'static str>)> {
    Ok(match body {
        // Bodies are kept as bytes: a stream would have to be read first.
        BodyInit::ReadableStream(_) => {
            return Err(Exception::type_error(
                "A ReadableStream body is not supported yet",
            ));
        }
        BodyInit::BufferSource(bytes) => (bytes, None),
        BodyInit::String(text) => (text.into_bytes(), Some("text/plain;charset=UTF-8")),
        BodyInit::URLSearchParams(id) => (
            crate::url_api::serialized_params(cx, id)?.into_bytes(),
            Some("application/x-www-form-urlencoded;charset=UTF-8"),
        ),
    })
}

fn decode_utf8(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.strip_prefix('\u{FEFF}').unwrap_or(&text).to_string()
}

/// The body's fields, whichever of the two objects `this` is.
fn with_body<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut Option<Vec<u8>>, &mut bool, &mut Option<ObjectId>) -> R,
) -> Fallible<R> {
    let mut f = Some(f);
    if let Some(result) = cx.page.try_with::<ResponseObject, _>(this, |r| {
        (f.take().expect("called once"))(&mut r.body, &mut r.body_used, &mut r.body_stream)
    }) {
        return Ok(result);
    }
    cx.page.with::<RequestObject, _>(this, |r| {
        (f.take().expect("called once"))(&mut r.body, &mut r.body_used, &mut r.body_stream)
    })
}

/// Whether the body was used: read here, or read or locked through its
/// stream.
fn body_is_used(cx: &Cx<'_>, this: ObjectId) -> Fallible<bool> {
    let (used, stream) = with_body(cx, this, |_, used, stream| (*used, *stream))?;
    Ok(used || stream.is_some_and(|s| crate::streams::is_disturbed_or_locked(cx, s)))
}

/// Takes the body of a `Request` or `Response`, marking it used.
fn consume_body(cx: &Cx<'_>, this: ObjectId) -> Fallible<Vec<u8>> {
    if body_is_used(cx, this)? {
        return Err(Exception::type_error("The body has already been read"));
    }
    let (bytes, stream) = with_body(cx, this, |body, used, stream| {
        if body.is_some() {
            *used = true;
        }
        (body.take().unwrap_or_default(), *stream)
    })?;
    // Reading here reads the stream handed out, as far as it is concerned.
    if let Some(stream) = stream {
        crate::streams::mark_disturbed(cx, stream);
    }
    Ok(bytes)
}

/// A promise for the consumed body, converted by `convert`.
fn body_promise(
    cx: &mut Cx<'_>,
    this: ObjectId,
    convert: impl FnOnce(&mut Cx<'_>, Vec<u8>) -> Fallible<Value>,
) -> Fallible<PromiseRef> {
    let promise = cx.script.new_promise();
    let result = consume_body(cx, this).and_then(|bytes| convert(cx, bytes));
    match result {
        Ok(value) => cx.script.resolve_promise(&promise, value),
        Err(e) => cx.script.reject_promise(&promise, e),
    }
    Ok(promise)
}

impl web::BodyImpl for Web {
    fn body(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<ObjectId>> {
        let (bytes, used, stream) = with_body(cx, this, |body, used, stream| {
            (body.clone(), *used, *stream)
        })?;
        if let Some(stream) = stream {
            return Ok(Some(stream));
        }
        // No body, or one read already: nothing to stream.
        let Some(bytes) = bytes.filter(|_| !used) else {
            return Ok(None);
        };
        let stream = crate::streams::readable_from_bytes(cx, bytes)?;
        with_body(cx, this, |_, _, slot| *slot = Some(stream))?;
        Ok(Some(stream))
    }

    fn body_used(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        body_is_used(cx, this)
    }

    fn array_buffer(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        body_promise(cx, this, |_, bytes| Ok(Value::ArrayBuffer(bytes)))
    }

    fn bytes(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        body_promise(cx, this, |_, bytes| Ok(Value::Uint8Array(bytes)))
    }

    fn json(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        body_promise(cx, this, |cx, bytes| {
            cx.script.parse_json(&decode_utf8(&bytes))
        })
    }

    fn text(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        body_promise(cx, this, |_, bytes| Ok(Value::String(decode_utf8(&bytes))))
    }
}

// ---- Request ---------------------------------------------------------------

pub struct RequestObject {
    method: String,
    url: Url,
    headers: HeaderList,
    body: Option<Vec<u8>>,
    body_used: bool,
    /// The stream `body` handed out, if it was asked for.
    body_stream: Option<ObjectId>,
    mode: RequestMode,
    credentials: RequestCredentials,
    cache: RequestCache,
    redirect: RequestRedirect,
    referrer: String,
    referrer_policy: ReferrerPolicy,
    integrity: String,
    keepalive: bool,
    /// The signal that aborts this request, if script supplied one.
    signal: Option<ObjectId>,
    headers_object: Option<ObjectId>,
}
platform_object!(RequestObject, Request);

/// <https://fetch.spec.whatwg.org/#concept-method-normalize>
fn normalize_method(method: &str) -> Fallible<String> {
    if !cors::is_header_name(method) {
        return Err(Exception::type_error(format!(
            "'{method}' is not a valid HTTP method"
        )));
    }
    if cors::is_forbidden_method(method) {
        return Err(Exception::type_error(format!(
            "'{method}' HTTP method is unsupported"
        )));
    }
    let upper = method.to_ascii_uppercase();
    Ok(
        if matches!(
            upper.as_str(),
            "DELETE" | "GET" | "HEAD" | "OPTIONS" | "POST" | "PUT"
        ) {
            upper
        } else {
            method.to_string()
        },
    )
}

/// The `Request` constructor's steps, as far as they are observable here.
fn build_request(
    cx: &mut Cx<'_>,
    input: RequestOrString,
    init: RequestInit,
) -> Fallible<RequestObject> {
    let mut request = match input {
        RequestOrString::String(text) => {
            let url = cx
                .page
                .resolve_url(&text)
                .ok_or_else(|| Exception::type_error(format!("Failed to parse URL from {text}")))?;
            if !url.username().is_empty() || url.password().is_some() {
                return Err(Exception::type_error(format!(
                    "Request cannot be constructed from a URL that includes credentials: {text}"
                )));
            }
            RequestObject {
                method: "GET".to_string(),
                url,
                headers: Vec::new(),
                body: None,
                body_used: false,
                body_stream: None,
                mode: RequestMode::Cors,
                credentials: RequestCredentials::SameOrigin,
                cache: RequestCache::Default,
                redirect: RequestRedirect::Follow,
                referrer: "about:client".to_string(),
                referrer_policy: ReferrerPolicy::Empty,
                integrity: String::new(),
                keepalive: false,
                signal: None,
                headers_object: None,
            }
        }
        RequestOrString::Request(id) => cx.page.with::<RequestObject, _>(id, |source| {
            if source.body_used {
                return Err(Exception::type_error(
                    "Cannot construct a Request with a Request object that has already been used",
                ));
            }
            let body = source.body.take();
            if body.is_some() {
                source.body_used = true;
            }
            Ok(RequestObject {
                method: source.method.clone(),
                url: source.url.clone(),
                headers: source.headers.clone(),
                body,
                body_used: false,
                body_stream: None,
                mode: source.mode,
                credentials: source.credentials,
                cache: source.cache,
                redirect: source.redirect,
                referrer: source.referrer.clone(),
                referrer_policy: source.referrer_policy,
                integrity: source.integrity.clone(),
                keepalive: source.keepalive,
                signal: source.signal,
                headers_object: None,
            })
        })??,
    };

    if let Some(method) = &init.method {
        request.method = normalize_method(method)?;
    }
    if let Some(mode) = init.mode {
        if mode == RequestMode::Navigate {
            return Err(Exception::type_error(
                "Cannot construct a Request with a RequestInit whose mode member is set as 'navigate'",
            ));
        }
        request.mode = mode;
    }
    if let Some(credentials) = init.credentials {
        request.credentials = credentials;
    }
    if let Some(cache) = init.cache {
        request.cache = cache;
    }
    if let Some(redirect) = init.redirect {
        request.redirect = redirect;
    }
    if let Some(referrer) = init.referrer {
        request.referrer = referrer;
    }
    if let Some(policy) = init.referrer_policy {
        request.referrer_policy = policy;
    }
    if let Some(integrity) = init.integrity {
        request.integrity = integrity;
    }
    if let Some(keepalive) = init.keepalive {
        request.keepalive = keepalive;
    }
    if let Some(signal) = init.signal {
        request.signal = Some(signal);
    }
    if init.headers.is_some() {
        request.headers = header_list_from_init(init.headers)?;
    }
    request
        .headers
        .retain(|(name, _)| !cors::is_forbidden_request_header(name));

    if let Some(body) = init.body {
        if matches!(request.method.as_str(), "GET" | "HEAD") {
            return Err(Exception::type_error(
                "Request with GET/HEAD method cannot have body",
            ));
        }
        let (bytes, content_type) = extract_body(cx, body)?;
        request.body = Some(bytes);
        if let Some(content_type) = content_type
            && !request.headers.iter().any(|(n, _)| n == "content-type")
        {
            request
                .headers
                .push(("content-type".to_string(), content_type.to_string()));
        }
    }
    Ok(request)
}

fn request<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&mut RequestObject) -> R) -> Fallible<R> {
    cx.page.with::<RequestObject, _>(this, f)
}

/// The `Headers` view of a request or response, created on first use.
fn headers_view(cx: &Cx<'_>, existing: Option<ObjectId>, store: HeaderStore) -> ObjectId {
    match existing.filter(|&id| cx.page.object_exists(id)) {
        Some(id) => id,
        None => cx.page.alloc(HeadersObject {
            store,
            list: Vec::new(),
        }),
    }
}

impl web::RequestImpl for Web {
    fn method(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        request(cx, this, |r| r.method.clone())
    }

    fn url(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        request(cx, this, |r| r.url.to_string())
    }

    fn headers(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        let existing = request(cx, this, |r| r.headers_object)?;
        let view = headers_view(cx, existing, HeaderStore::Request(this));
        request(cx, this, |r| r.headers_object = Some(view))?;
        Ok(view)
    }

    fn destination(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<RequestDestination> {
        Ok(RequestDestination::Empty)
    }

    fn referrer(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        request(cx, this, |r| r.referrer.clone())
    }

    fn referrer_policy(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ReferrerPolicy> {
        request(cx, this, |r| r.referrer_policy)
    }

    fn mode(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<RequestMode> {
        request(cx, this, |r| r.mode)
    }

    fn credentials(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<RequestCredentials> {
        request(cx, this, |r| r.credentials)
    }

    fn cache(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<RequestCache> {
        request(cx, this, |r| r.cache)
    }

    fn redirect(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<RequestRedirect> {
        request(cx, this, |r| r.redirect)
    }

    fn integrity(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        request(cx, this, |r| r.integrity.clone())
    }

    fn keepalive(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        request(cx, this, |r| r.keepalive)
    }

    fn signal(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        let existing = request(cx, this, |r| r.signal)?;
        if let Some(signal) = existing.filter(|&id| cx.page.object_exists(id)) {
            return Ok(signal);
        }
        let signal = abort::new_signal(cx.page);
        request(cx, this, |r| r.signal = Some(signal))?;
        Ok(signal)
    }

    fn clone(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        let copy = request(cx, this, |r| {
            if r.body_used {
                return Err(Exception::type_error("The request's body has been used"));
            }
            Ok(RequestObject {
                method: r.method.clone(),
                url: r.url.clone(),
                headers: r.headers.clone(),
                body: r.body.clone(),
                body_used: false,
                body_stream: None,
                mode: r.mode,
                credentials: r.credentials,
                cache: r.cache,
                redirect: r.redirect,
                referrer: r.referrer.clone(),
                referrer_policy: r.referrer_policy,
                integrity: r.integrity.clone(),
                keepalive: r.keepalive,
                signal: r.signal,
                headers_object: None,
            })
        })??;
        Ok(cx.page.alloc(copy))
    }

    fn constructor(
        cx: &mut Cx<'_>,
        input: RequestOrString,
        init: RequestInit,
    ) -> Fallible<ObjectId> {
        let request = build_request(cx, input, init)?;
        Ok(cx.page.alloc(request))
    }
}

// ---- Response --------------------------------------------------------------

pub struct ResponseObject {
    kind: ResponseType,
    url: Option<Url>,
    redirected: bool,
    status: u16,
    status_text: String,
    headers: HeaderList,
    body: Option<Vec<u8>>,
    body_used: bool,
    /// The stream `body` handed out, if it was asked for.
    body_stream: Option<ObjectId>,
    /// Came from the network: its headers cannot be changed.
    from_network: bool,
    headers_object: Option<ObjectId>,
}
platform_object!(ResponseObject, Response);

fn is_null_body_status(status: u16) -> bool {
    matches!(status, 101 | 103 | 204 | 205 | 304)
}

fn synthetic_response(
    cx: &Cx<'_>,
    body: Option<(Vec<u8>, Option<&'static str>)>,
    init: ResponseInit,
) -> Fallible<ResponseObject> {
    if !(200..=599).contains(&init.status) {
        return Err(Exception::range_error(format!(
            "The status provided ({}) is outside the range [200, 599]",
            init.status
        )));
    }
    if init.status_text.bytes().any(|b| b == b'\r' || b == b'\n') {
        return Err(Exception::type_error("Invalid statusText"));
    }
    let mut headers = header_list_from_init(init.headers)?;
    let body = match body {
        Some(_) if is_null_body_status(init.status) => {
            return Err(Exception::type_error(
                "Response with null body status cannot have body",
            ));
        }
        Some((bytes, content_type)) => {
            if let Some(content_type) = content_type
                && !headers.iter().any(|(n, _)| n == "content-type")
            {
                headers.push(("content-type".to_string(), content_type.to_string()));
            }
            Some(bytes)
        }
        None => None,
    };
    let _ = cx;
    Ok(ResponseObject {
        kind: ResponseType::Default,
        url: None,
        redirected: false,
        status: init.status,
        status_text: init.status_text,
        headers,
        body,
        body_used: false,
        body_stream: None,
        from_network: false,
        headers_object: None,
    })
}

fn response<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut ResponseObject) -> R,
) -> Fallible<R> {
    cx.page.with::<ResponseObject, _>(this, f)
}

impl web::ResponseImpl for Web {
    fn error(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(ResponseObject {
            kind: ResponseType::Error,
            url: None,
            redirected: false,
            status: 0,
            status_text: String::new(),
            headers: Vec::new(),
            body: None,
            body_used: false,
            body_stream: None,
            from_network: true,
            headers_object: None,
        }))
    }

    fn redirect(cx: &mut Cx<'_>, url: String, status: u16) -> Fallible<ObjectId> {
        let target = cx
            .page
            .resolve_url(&url)
            .ok_or_else(|| Exception::type_error(format!("Failed to parse URL from {url}")))?;
        if !matches!(status, 301 | 302 | 303 | 307 | 308) {
            return Err(Exception::range_error("Invalid status code"));
        }
        Ok(cx.page.alloc(ResponseObject {
            kind: ResponseType::Default,
            url: None,
            redirected: false,
            status,
            status_text: String::new(),
            headers: vec![("location".to_string(), target.to_string())],
            body: None,
            body_used: false,
            body_stream: None,
            from_network: true,
            headers_object: None,
        }))
    }

    fn json(cx: &mut Cx<'_>, data: Value, init: ResponseInit) -> Fallible<ObjectId> {
        let text = cx
            .script
            .stringify_json(&data)?
            .ok_or_else(|| Exception::type_error("The data is not JSON serializable"))?;
        let response = synthetic_response(
            cx,
            Some((text.into_bytes(), Some("application/json"))),
            init,
        )?;
        Ok(cx.page.alloc(response))
    }

    fn type_(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ResponseType> {
        response(cx, this, |r| r.kind)
    }

    fn url(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        response(cx, this, |r| match &r.url {
            Some(url) => {
                let mut url = url.clone();
                url.set_fragment(None);
                url.to_string()
            }
            None => String::new(),
        })
    }

    fn redirected(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        response(cx, this, |r| r.redirected)
    }

    fn status(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u16> {
        response(cx, this, |r| r.status)
    }

    fn ok(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        response(cx, this, |r| (200..300).contains(&r.status))
    }

    fn status_text(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        response(cx, this, |r| r.status_text.clone())
    }

    fn headers(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        let existing = response(cx, this, |r| r.headers_object)?;
        let view = headers_view(cx, existing, HeaderStore::Response(this));
        response(cx, this, |r| r.headers_object = Some(view))?;
        Ok(view)
    }

    fn clone(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        let copy = response(cx, this, |r| {
            if r.body_used {
                return Err(Exception::type_error("The response's body has been used"));
            }
            Ok(ResponseObject {
                kind: r.kind,
                url: r.url.clone(),
                redirected: r.redirected,
                status: r.status,
                status_text: r.status_text.clone(),
                headers: r.headers.clone(),
                body: r.body.clone(),
                body_used: false,
                body_stream: None,
                from_network: r.from_network,
                headers_object: None,
            })
        })??;
        Ok(cx.page.alloc(copy))
    }

    fn constructor(
        cx: &mut Cx<'_>,
        body: Option<BodyInit>,
        init: ResponseInit,
    ) -> Fallible<ObjectId> {
        let body = match body {
            Some(body) => Some(extract_body(cx, body)?),
            None => None,
        };
        let response = synthetic_response(cx, body, init)?;
        Ok(cx.page.alloc(response))
    }
}

// ---- fetch() ---------------------------------------------------------------

fn response_from_network(readable: Readable) -> ResponseObject {
    let Readable { response, exposure } = readable;
    let headers = response
        .headers
        .into_iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value))
        .collect();
    ResponseObject {
        kind: match exposure {
            Exposure::Basic => ResponseType::Basic,
            Exposure::Cors => ResponseType::Cors,
            Exposure::Opaque => ResponseType::Opaque,
        },
        url: (exposure != Exposure::Opaque).then_some(response.url),
        redirected: exposure != Exposure::Opaque && response.redirected,
        status: response.status,
        status_text: response.status_text,
        headers,
        body: (!is_null_body_status(response.status) && exposure != Exposure::Opaque)
            .then_some(response.body),
        body_used: false,
        body_stream: None,
        from_network: true,
        headers_object: None,
    }
}

/// Logs why a request failed. Script only ever sees a `TypeError`.
fn log_failure(page: &PageState, url: &Url, reason: &str) {
    page.log(
        ConsoleLevel::Error,
        format!("Failed to load {url}: {reason}"),
    );
}

impl Web {
    /// The body of `fetch()`; errors become a rejected promise.
    fn start_fetch(
        cx: &mut Cx<'_>,
        promise: &PromiseRef,
        input: RequestOrString,
        init: RequestInit,
    ) -> Fallible<()> {
        let request = build_request(cx, input, init)?;
        if let Some(reason) = request.signal.and_then(|s| abort::abort_reason(cx.page, s)) {
            return Err(Exception::Value(reason));
        }
        let url = request.url.clone();
        let out = Outgoing {
            method: request.method,
            url: url.clone(),
            headers: request.headers,
            body: request.body,
            mode: match request.mode {
                RequestMode::SameOrigin => Mode::SameOrigin,
                RequestMode::NoCors => Mode::NoCors,
                RequestMode::Cors | RequestMode::Navigate => Mode::Cors,
            },
            credentials: match request.credentials {
                RequestCredentials::Omit => Credentials::Omit,
                RequestCredentials::SameOrigin => Credentials::SameOrigin,
                RequestCredentials::Include => Credentials::Include,
            },
            kind: RequestKind::Fetch,
        };
        let fail_on_redirect = request.redirect == RequestRedirect::Error;

        let settled = promise.clone();
        let pending = cors::send(cx.page, out, move |cx, result| {
            let result = result.and_then(|readable| {
                if fail_on_redirect && readable.response.redirected {
                    Err(
                        "the request was redirected, and its redirect mode is \"error\""
                            .to_string(),
                    )
                } else {
                    Ok(readable)
                }
            });
            match result {
                Ok(readable) => {
                    let response = cx.page.alloc(response_from_network(readable));
                    cx.script.resolve_promise(&settled, Value::Object(response));
                }
                Err(reason) => {
                    log_failure(cx.page, &url, &reason);
                    cx.script
                        .reject_promise(&settled, Exception::type_error("Failed to fetch"));
                }
            }
        });

        if let Some(signal) = request.signal {
            let aborted = promise.clone();
            let algorithm: AbortAlgorithm = Rc::new(move |cx, reason| {
                pending.abort(cx.page);
                cx.script
                    .reject_promise(&aborted, Exception::Value(reason.clone()));
            });
            abort::add_algorithm(cx.page, signal, algorithm);
        }
        Ok(())
    }

    pub(crate) fn fetch_impl(
        cx: &mut Cx<'_>,
        input: RequestOrString,
        init: RequestInit,
    ) -> Fallible<PromiseRef> {
        let promise = cx.script.new_promise();
        if let Err(e) = Self::start_fetch(cx, &promise, input, init) {
            cx.script.reject_promise(&promise, e);
        }
        Ok(promise)
    }
}
