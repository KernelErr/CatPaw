//! `XMLHttpRequest` (<https://xhr.spec.whatwg.org/>).

use std::rc::Rc;

use catpaw_dom::to_html;
use catpaw_js::{EventTargetRef, Exception, Fallible, ObjectId, Value};
use encoding_rs::Encoding;
use url::Url;

use crate::cors::{self, Credentials, Mode, Outgoing, Pending, Readable, Redirect};
use crate::event_loop::{self, TimerAction};
use crate::generated::ReferrerPolicy;
use crate::generated::{
    self as web, DocumentOrBlobOrBufferSourceOrFormDataOrURLSearchParamsOrString as XhrBody,
    XMLHttpRequestResponseType as ResponseType,
};
use crate::net::{NetResponse, RequestKind};
use crate::page::{ConsoleLevel, Cx};
use crate::{Web, events, platform_object};

const UNSENT: u16 = 0;
const OPENED: u16 = 1;
const HEADERS_RECEIVED: u16 = 2;
const LOADING: u16 = 3;
const DONE: u16 = 4;

pub struct XhrObject {
    state: u16,
    /// `send()` has been called and the request has not finished.
    sending: bool,
    method: String,
    url: Option<Url>,
    is_async: bool,
    request_headers: Vec<(String, String)>,
    with_credentials: bool,
    timeout_ms: u32,
    response_type: ResponseType,
    override_mime: Option<String>,
    upload: Option<ObjectId>,
    pending: Option<Pending>,
    timer: Option<i32>,
    response: Option<NetResponse>,
    /// Bumped whenever the current request is abandoned, so that a late
    /// completion can tell it is no longer wanted.
    generation: u32,
    pinned: bool,
    /// The size of the body being sent; the upload gets progress events
    /// when there is one.
    upload_total: usize,
}
platform_object!(XhrObject, XMLHttpRequest);

pub struct XhrUploadObject;
platform_object!(XhrUploadObject, XMLHttpRequestUpload);

fn xhr<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&mut XhrObject) -> R) -> Fallible<R> {
    cx.page.with::<XhrObject, _>(this, f)
}

fn invalid_state(message: &str) -> Exception {
    Exception::invalid_state(message)
}

fn fire(cx: &mut Cx<'_>, this: ObjectId, type_: &str) {
    events::fire(cx, EventTargetRef::Object(this), type_, false, false);
}

fn fire_progress(cx: &mut Cx<'_>, this: ObjectId, type_: &str, loaded: usize) {
    let event = events::progress_event(cx, type_, loaded as f64, Some(loaded as f64));
    events::dispatch(cx, EventTargetRef::Object(this), event);
}

/// Stops keeping the object alive for a request in flight.
fn release(cx: &mut Cx<'_>, this: ObjectId) {
    let (pinned, timer) = xhr(cx, this, |x| {
        x.pending = None;
        (std::mem::take(&mut x.pinned), x.timer.take())
    })
    .unwrap_or((false, None));
    if let Some(timer) = timer {
        event_loop::clear_timer(cx.page, timer);
    }
    if pinned {
        cx.unpin(this);
    }
}

/// The request finished without a usable response: `error`, `timeout` or
/// `abort`, then `loadend`.
fn fail(cx: &mut Cx<'_>, this: ObjectId, event: &str) {
    let _ = xhr(cx, this, |x| {
        x.state = DONE;
        x.sending = false;
        x.response = None;
    });
    release(cx, this);
    fire(cx, this, "readystatechange");
    if let Some((upload, _)) = upload_in_progress(cx, this) {
        fire_progress_on(cx, upload, event, 0, 0);
        fire_progress_on(cx, upload, "loadend", 0, 0);
    }
    fire_progress(cx, this, event, 0);
    fire_progress(cx, this, "loadend", 0);
}

/// The upload object and the body size, while a body is being sent and
/// script has the upload object to listen on.
fn upload_in_progress(cx: &Cx<'_>, this: ObjectId) -> Option<(ObjectId, usize)> {
    let (upload, total) = xhr(cx, this, |x| (x.upload, x.upload_total)).ok()?;
    let upload = upload.filter(|&id| cx.page.object_exists(id))?;
    (total > 0).then_some((upload, total))
}

fn fire_progress_on(cx: &mut Cx<'_>, target: ObjectId, type_: &str, loaded: usize, total: usize) {
    let event = events::progress_event(cx, type_, loaded as f64, Some(total as f64));
    events::dispatch(cx, EventTargetRef::Object(target), event);
}

fn succeed(cx: &mut Cx<'_>, this: ObjectId, response: NetResponse) {
    // The body went out in full before the response came back.
    if let Some((upload, total)) = upload_in_progress(cx, this) {
        fire_progress_on(cx, upload, "progress", total, total);
        fire_progress_on(cx, upload, "load", total, total);
        fire_progress_on(cx, upload, "loadend", total, total);
        let _ = xhr(cx, this, |x| x.upload_total = 0);
    }
    let loaded = response.body.len();
    let _ = xhr(cx, this, |x| {
        x.response = Some(response);
        x.state = HEADERS_RECEIVED;
    });
    fire(cx, this, "readystatechange");
    let _ = xhr(cx, this, |x| x.state = LOADING);
    fire(cx, this, "readystatechange");
    fire_progress(cx, this, "progress", loaded);
    let _ = xhr(cx, this, |x| {
        x.state = DONE;
        x.sending = false;
    });
    release(cx, this);
    fire(cx, this, "readystatechange");
    fire_progress(cx, this, "load", loaded);
    fire_progress(cx, this, "loadend", loaded);
}

fn finish(cx: &mut Cx<'_>, this: ObjectId, generation: u32, result: Result<Readable, String>) {
    let current = xhr(cx, this, |x| x.generation == generation && x.sending).unwrap_or(false);
    if !current {
        return;
    }
    match result {
        Ok(readable) => succeed(cx, this, readable.response),
        Err(reason) => {
            let url = xhr(cx, this, |x| x.url.as_ref().map(Url::to_string))
                .ok()
                .flatten()
                .unwrap_or_default();
            cx.page.log(
                ConsoleLevel::Error,
                format!("Failed to load {url}: {reason}"),
            );
            fail(cx, this, "error");
        }
    }
}

/// Abandons the request in flight, if any, without firing events.
fn terminate(cx: &mut Cx<'_>, this: ObjectId) {
    let pending = xhr(cx, this, |x| {
        x.generation = x.generation.wrapping_add(1);
        x.pending.take()
    })
    .ok()
    .flatten();
    if let Some(pending) = pending {
        pending.abort(cx.page);
    }
    release(cx, this);
}

fn charset_of(content_type: Option<&str>) -> Option<&'static Encoding> {
    content_type?.split(';').skip(1).find_map(|param| {
        let (name, value) = param.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| Encoding::for_label(value.trim().trim_matches('"').as_bytes()))
            .flatten()
    })
}

/// The response body as text: BOM, then the declared charset, then UTF-8.
fn response_text(x: &XhrObject) -> String {
    let Some(response) = x.response.as_ref().filter(|_| x.state >= LOADING) else {
        return String::new();
    };
    let declared = x
        .override_mime
        .as_deref()
        .or_else(|| response.header("content-type"));
    // Only the default response type looks inside an XML body for its
    // encoding; "text" takes the MIME type's word or UTF-8.
    let encoding = charset_of(declared)
        .or_else(|| {
            (x.response_type == ResponseType::Empty)
                .then(|| xml_declared_encoding(declared, &response.body))
                .flatten()
        })
        .unwrap_or(encoding_rs::UTF_8);
    encoding.decode(&response.body).0.into_owned()
}

/// The encoding an XML body declares in `<?xml ... encoding=...?>`, for
/// when the MIME type names none.
fn xml_declared_encoding(content_type: Option<&str>, body: &[u8]) -> Option<&'static Encoding> {
    let essence = content_type?.split(';').next()?.trim().to_ascii_lowercase();
    if !(essence == "text/xml" || essence == "application/xml" || essence.ends_with("+xml")) {
        return None;
    }
    let head = String::from_utf8_lossy(&body[..body.len().min(1024)]).into_owned();
    let declaration = head.strip_prefix("<?xml")?;
    let declaration = &declaration[..declaration.find("?>")?];
    let at = declaration.find("encoding")?;
    let rest = declaration[at + "encoding".len()..]
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value = rest[1..].split(quote).next()?;
    Encoding::for_label(value.as_bytes())
}

fn open(
    cx: &mut Cx<'_>,
    this: ObjectId,
    method: String,
    url: String,
    is_async: bool,
    credentials: Option<(Option<String>, Option<String>)>,
) -> Fallible<()> {
    if !cors::is_header_name(&method) {
        return Err(Exception::syntax(format!(
            "'{method}' is not a valid HTTP method"
        )));
    }
    if cors::is_forbidden_method(&method) {
        return Err(Exception::security(format!(
            "'{method}' HTTP method is unsupported"
        )));
    }
    let upper = method.to_ascii_uppercase();
    let method = if matches!(
        upper.as_str(),
        "DELETE" | "GET" | "HEAD" | "OPTIONS" | "POST" | "PUT"
    ) {
        upper
    } else {
        method
    };
    let mut parsed = cx
        .page
        .resolve_url(&url)
        .ok_or_else(|| Exception::syntax(format!("'{url}' is not a valid URL")))?;
    if let Some((username, password)) = credentials {
        if let Some(username) = username {
            let _ = parsed.set_username(&username);
        }
        if let Some(password) = password {
            let _ = parsed.set_password(Some(&password));
        }
    }

    // A synchronous request in a window cannot have a timeout or a
    // response type (XHR: open() step 10).
    if !is_async
        && cx.page.workers.role().is_none()
        && xhr(cx, this, |x| {
            x.timeout_ms != 0 || x.response_type != ResponseType::Empty
        })?
    {
        return Err(Exception::dom(
            "InvalidAccessError",
            "Synchronous requests from a document must not set a timeout or a response type",
        ));
    }
    terminate(cx, this);
    let changed = xhr(cx, this, |x| {
        x.sending = false;
        x.method = method;
        x.url = Some(parsed);
        x.is_async = is_async;
        x.request_headers.clear();
        x.response = None;
        let changed = x.state != OPENED;
        x.state = OPENED;
        changed
    })?;
    if changed {
        fire(cx, this, "readystatechange");
    }
    Ok(())
}

impl web::XMLHttpRequestImpl for Web {
    fn ready_state(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u16> {
        xhr(cx, this, |x| x.state)
    }

    fn open(cx: &mut Cx<'_>, this: ObjectId, method: String, url: String) -> Fallible<()> {
        open(cx, this, method, url, true, None)
    }

    fn open_overload2(
        cx: &mut Cx<'_>,
        this: ObjectId,
        method: String,
        url: String,
        async_: bool,
        username: Option<String>,
        password: Option<String>,
    ) -> Fallible<()> {
        open(cx, this, method, url, async_, Some((username, password)))
    }

    fn set_request_header(
        cx: &mut Cx<'_>,
        this: ObjectId,
        name: String,
        value: String,
    ) -> Fallible<()> {
        let value = cors::normalize_header_value(&value)
            .filter(|_| cors::is_header_name(&name))
            .ok_or_else(|| Exception::syntax("Invalid header name or value"))?;
        xhr(cx, this, |x| {
            if x.state != OPENED || x.sending {
                return Err(invalid_state("The object's state must be OPENED"));
            }
            if cors::is_forbidden_request_header(&name, &value) {
                return Ok(());
            }
            // The name keeps the case it was given; later values combine.
            match x
                .request_headers
                .iter_mut()
                .find(|(n, _)| n.eq_ignore_ascii_case(&name))
            {
                Some((_, existing)) => {
                    existing.push_str(", ");
                    existing.push_str(&value);
                }
                None => x.request_headers.push((name, value)),
            }
            Ok(())
        })?
    }

    fn timeout(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        xhr(cx, this, |x| x.timeout_ms)
    }

    fn set_timeout(cx: &mut Cx<'_>, this: ObjectId, value: u32) -> Fallible<()> {
        if cx.page.workers.role().is_none() && xhr(cx, this, |x| !x.is_async && x.state != UNSENT)?
        {
            return Err(Exception::dom(
                "InvalidAccessError",
                "Synchronous requests from a document must not set a timeout",
            ));
        }
        xhr(cx, this, |x| x.timeout_ms = value)
    }

    fn with_credentials(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        xhr(cx, this, |x| x.with_credentials)
    }

    fn set_with_credentials(cx: &mut Cx<'_>, this: ObjectId, value: bool) -> Fallible<()> {
        xhr(cx, this, |x| {
            if (x.state != UNSENT && x.state != OPENED) || x.sending {
                return Err(invalid_state(
                    "The value may only be set before the request is sent",
                ));
            }
            x.with_credentials = value;
            Ok(())
        })?
    }

    fn upload(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        let existing = xhr(cx, this, |x| x.upload)?;
        if let Some(upload) = existing.filter(|&id| cx.page.object_exists(id)) {
            return Ok(upload);
        }
        let upload = cx.page.alloc(XhrUploadObject);
        xhr(cx, this, |x| x.upload = Some(upload))?;
        Ok(upload)
    }

    fn send(cx: &mut Cx<'_>, this: ObjectId, body: Option<XhrBody>) -> Fallible<()> {
        let (method, url, mut headers, is_async, with_credentials, timeout_ms) =
            xhr(cx, this, |x| {
                if x.state != OPENED || x.sending {
                    return Err(invalid_state("The object's state must be OPENED"));
                }
                let url = x.url.clone().ok_or_else(|| invalid_state("No URL"))?;
                Ok((
                    x.method.clone(),
                    url,
                    x.request_headers.clone(),
                    x.is_async,
                    x.with_credentials,
                    x.timeout_ms,
                ))
            })??;

        let body = match body.filter(|_| !matches!(method.as_str(), "GET" | "HEAD")) {
            None => None,
            Some(body) => {
                let utf8_body = matches!(
                    body,
                    XhrBody::String(_) | XhrBody::Document(_) | XhrBody::URLSearchParams(_)
                );
                let (bytes, content_type) = match body {
                    XhrBody::BufferSource(bytes) => (bytes, None),
                    XhrBody::String(text) => (
                        text.into_bytes(),
                        Some("text/plain;charset=UTF-8".to_string()),
                    ),
                    XhrBody::URLSearchParams(id) => (
                        crate::url_api::serialized_params(cx, id)?.into_bytes(),
                        Some("application/x-www-form-urlencoded;charset=UTF-8".to_string()),
                    ),
                    XhrBody::Document(node) => {
                        let dom = cx.dom();
                        if crate::document::is_html_document(&dom, node) {
                            (
                                to_html(&dom, node, true).into_bytes(),
                                Some("text/html;charset=UTF-8".to_string()),
                            )
                        } else {
                            (
                                catpaw_dom::serialize::to_xml(&dom, node).into_bytes(),
                                Some("application/xml;charset=UTF-8".to_string()),
                            )
                        }
                    }
                    XhrBody::Blob(id) => {
                        let (bytes, type_) = crate::file_api::blob_contents(cx, id)?;
                        (bytes.to_vec(), (!type_.is_empty()).then_some(type_))
                    }
                    XhrBody::FormData(id) => {
                        let (bytes, content_type) = crate::file_api::multipart_body(cx, id)?;
                        (bytes, Some(content_type))
                    }
                };
                match headers
                    .iter_mut()
                    .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
                {
                    // A body that is always UTF-8 here corrects an author
                    // charset that says otherwise.
                    Some((_, author)) if utf8_body => {
                        if let Some(mut mime) = crate::mime::parse(author)
                            && mime
                                .parameter("charset")
                                .is_some_and(|c| !c.eq_ignore_ascii_case("utf-8"))
                        {
                            mime.set_parameter("charset", "UTF-8");
                            *author = mime.serialize();
                        }
                    }
                    Some(_) => {}
                    None => {
                        if let Some(content_type) = content_type {
                            headers.push(("Content-Type".to_string(), content_type));
                        }
                    }
                }
                Some(bytes)
            }
        };
        let out = Outgoing {
            method,
            url,
            headers,
            body,
            mode: Mode::Cors,
            credentials: if with_credentials {
                Credentials::Include
            } else {
                Credentials::SameOrigin
            },
            redirect: Redirect::Follow,
            referrer: Some(cx.page.url.borrow().clone()),
            referrer_policy: ReferrerPolicy::Empty,
            kind: RequestKind::Xhr,
        };

        if !is_async {
            let result = cors::send_blocking(cx.page, out);
            return match result {
                Ok(readable) => {
                    let _ = xhr(cx, this, |x| {
                        x.response = Some(readable.response);
                        x.state = DONE;
                    });
                    fire(cx, this, "readystatechange");
                    Ok(())
                }
                Err(reason) => {
                    let _ = xhr(cx, this, |x| x.state = DONE);
                    Err(Exception::network(format!("Failed to load: {reason}")))
                }
            };
        }

        // Upload events go to listeners registered before send(), as the
        // specification's upload listener flag has it.
        let upload_total = match xhr(cx, this, |x| x.upload)? {
            Some(upload) if cx.page.object_exists(upload) => {
                let target = EventTargetRef::Object(upload);
                let listening = [
                    "loadstart",
                    "progress",
                    "load",
                    "loadend",
                    "abort",
                    "error",
                    "timeout",
                ]
                .iter()
                .any(|t| events::has_listeners(cx.page, target, t));
                if listening {
                    out.body.as_ref().map_or(0, Vec::len)
                } else {
                    0
                }
            }
            _ => 0,
        };
        let generation = xhr(cx, this, |x| {
            x.sending = true;
            x.pinned = true;
            x.upload_total = upload_total;
            x.generation
        })?;
        cx.pin(this);
        fire_progress(cx, this, "loadstart", 0);
        if let Some((upload, total)) = upload_in_progress(cx, this) {
            fire_progress_on(cx, upload, "loadstart", 0, total);
        }
        // A loadstart listener may have aborted or reopened the request.
        let still_wanted = xhr(cx, this, |x| x.generation == generation && x.sending)?;
        if !still_wanted {
            return Ok(());
        }

        let pending = cors::send(cx.page, out, move |cx, result| {
            finish(cx, this, generation, result)
        });
        let timer = (timeout_ms > 0).then(|| {
            event_loop::set_timer(
                cx.page,
                TimerAction::Native(Rc::new(move |cx| {
                    let current =
                        xhr(cx, this, |x| x.generation == generation && x.sending).unwrap_or(false);
                    if current {
                        terminate(cx, this);
                        fail(cx, this, "timeout");
                    }
                })),
                i32::try_from(timeout_ms).unwrap_or(i32::MAX),
                false,
            )
        });
        xhr(cx, this, |x| {
            x.pending = Some(pending);
            x.timer = timer;
        })
    }

    fn abort(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        let was_sending = xhr(cx, this, |x| {
            (x.state == OPENED && x.sending) || x.state == HEADERS_RECEIVED || x.state == LOADING
        })?;
        terminate(cx, this);
        if was_sending {
            fail(cx, this, "abort");
        }
        // The object ends up UNSENT without a further readystatechange.
        xhr(cx, this, |x| {
            if x.state == DONE {
                x.state = UNSENT;
            }
            x.sending = false;
        })
    }

    fn response_url(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        xhr(cx, this, |x| match &x.response {
            Some(response) => {
                let mut url = response.url.clone();
                url.set_fragment(None);
                url.to_string()
            }
            None => String::new(),
        })
    }

    fn status(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u16> {
        xhr(cx, this, |x| x.response.as_ref().map_or(0, |r| r.status))
    }

    fn status_text(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        xhr(cx, this, |x| {
            x.response
                .as_ref()
                .map(|r| r.status_text.clone())
                .unwrap_or_default()
        })
    }

    fn get_response_header(
        cx: &mut Cx<'_>,
        this: ObjectId,
        name: String,
    ) -> Fallible<Option<String>> {
        xhr(cx, this, |x| {
            let response = x.response.as_ref()?;
            let values: Vec<&str> = response
                .headers
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case(&name))
                .map(|(_, v)| v.as_str())
                .collect();
            (!values.is_empty()).then(|| values.join(", "))
        })
    }

    fn get_all_response_headers(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        xhr(cx, this, |x| {
            let Some(response) = &x.response else {
                return String::new();
            };
            let mut headers: Vec<(String, &str)> = response
                .headers
                .iter()
                .map(|(n, v)| (n.to_ascii_lowercase(), v.as_str()))
                .collect();
            headers.sort_by(|a, b| a.0.cmp(&b.0));
            headers
                .into_iter()
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect()
        })
    }

    fn override_mime_type(cx: &mut Cx<'_>, this: ObjectId, mime: String) -> Fallible<()> {
        xhr(cx, this, |x| {
            if x.state >= LOADING {
                return Err(invalid_state(
                    "The MIME type cannot be overridden once loading has started",
                ));
            }
            x.override_mime = Some(mime);
            Ok(())
        })?
    }

    fn response_type(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ResponseType> {
        xhr(cx, this, |x| x.response_type)
    }

    fn set_response_type(cx: &mut Cx<'_>, this: ObjectId, value: ResponseType) -> Fallible<()> {
        xhr(cx, this, |x| {
            if x.state >= LOADING {
                return Err(invalid_state(
                    "The response type cannot be changed once loading has started",
                ));
            }
            x.response_type = value;
            Ok(())
        })?
    }

    fn response(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        let (kind, done, text, bytes) = xhr(cx, this, |x| {
            let done = x.state == DONE && x.response.is_some();
            let bytes = match (&x.response, x.response_type) {
                (Some(r), ResponseType::Arraybuffer | ResponseType::Blob) if done => {
                    Some(r.body.clone())
                }
                _ => None,
            };
            (x.response_type, done, response_text(x), bytes)
        })?;
        Ok(match kind {
            ResponseType::Empty | ResponseType::Text => Value::String(text),
            _ if !done => Value::Null,
            ResponseType::Arraybuffer => bytes.map_or(Value::Null, Value::ArrayBuffer),
            ResponseType::Json => cx.script.parse_json(&text).unwrap_or(Value::Null),
            ResponseType::Blob => match bytes {
                Some(bytes) => {
                    let mime = Self::get_response_header(cx, this, "content-type".to_string())?
                        .unwrap_or_default();
                    Value::Object(crate::file_api::new_blob(cx, bytes, &mime))
                }
                None => Value::Null,
            },
            // Document responses are not supported yet.
            ResponseType::Document => Value::Null,
        })
    }

    fn response_text(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        xhr(cx, this, |x| {
            if !matches!(x.response_type, ResponseType::Empty | ResponseType::Text) {
                return Err(invalid_state(
                    "responseText is only available if responseType is '' or 'text'",
                ));
            }
            Ok(response_text(x))
        })?
    }

    fn constructor(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(XhrObject {
            state: UNSENT,
            sending: false,
            method: "GET".to_string(),
            url: None,
            is_async: true,
            request_headers: Vec::new(),
            with_credentials: false,
            timeout_ms: 0,
            response_type: ResponseType::Empty,
            override_mime: None,
            upload: None,
            pending: None,
            timer: None,
            response: None,
            generation: 0,
            pinned: false,
            upload_total: 0,
        }))
    }
}
