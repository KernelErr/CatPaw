//! `WebSocket`: the page's end of a connection the network host keeps.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use catpaw_js::{EventTargetRef, Exception, Fallible, ObjectId, Value};
use url::Url;

use crate::events::{self, Event, EventData};
use crate::frames::{self, MessageData};
use crate::generated::{self as web, InterfaceId};
use crate::net::{WsEvent, WsOutbound};
use crate::page::{Cx, PageState};
use crate::{Web, platform_object};

const CONNECTING: u16 = 0;
const OPEN: u16 = 1;
const CLOSING: u16 = 2;
const CLOSED: u16 = 3;

pub struct WebSocketObject {
    token: Option<u64>,
    url: Url,
    state: Cell<u16>,
    protocol: RefCell<String>,
    extensions: RefCell<String>,
    binary_type: Cell<web::BinaryType>,
    buffered: Cell<u64>,
    /// When it was made (wall time): a handshake that takes long stops
    /// counting as work the page waits for.
    made: std::time::Instant,
}
platform_object!(WebSocketObject, WebSocket);

/// The page's sockets by host token.
#[derive(Default)]
pub struct Sockets {
    by_token: RefCell<HashMap<u64, ObjectId>>,
}

impl Sockets {
    fn count(&self, page: &PageState, state: u16) -> usize {
        self.by_token
            .borrow()
            .values()
            .filter(|&&id| {
                page.try_with::<WebSocketObject, _>(id, |s| s.state.get()) == Some(state)
            })
            .count()
    }

    /// Sockets whose handshake is pending: work the page waits for.
    pub fn connecting(&self, page: &PageState) -> usize {
        self.count(page, CONNECTING)
    }

    /// Sockets whose handshake is pending and began less than `within`
    /// ago.
    pub fn connecting_within(&self, page: &PageState, within: std::time::Duration) -> usize {
        self.by_token
            .borrow()
            .values()
            .filter(|&&id| {
                page.try_with::<WebSocketObject, _>(id, |s| {
                    s.state.get() == CONNECTING && s.made.elapsed() < within
                }) == Some(true)
            })
            .count()
    }

    /// Sockets that are open or closing: the server may still speak.
    pub fn open(&self, page: &PageState) -> usize {
        self.count(page, OPEN) + self.count(page, CLOSING)
    }
}

fn socket<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&WebSocketObject) -> R) -> Fallible<R> {
    cx.page.with::<WebSocketObject, _>(this, |s| f(s))
}

/// The ready state, `CLOSED` for a stale object.
fn state_of(cx: &Cx<'_>, this: ObjectId) -> u16 {
    socket(cx, this, |s| s.state.get()).unwrap_or(CLOSED)
}

fn fire_close(cx: &mut Cx<'_>, this: ObjectId, code: u16, reason: String, clean: bool) {
    let mut event = Event::new("close", false, false, cx.page.clock.peek());
    event.iface = InterfaceId::CloseEvent;
    event.trusted = true;
    event.data = EventData::Close {
        was_clean: clean,
        code,
        reason,
    };
    let event = cx.page.alloc(event);
    cx.pin(event);
    events::dispatch(cx, EventTargetRef::Object(this), event);
    cx.unpin(event);
}

/// Ends a socket: `error` (unless the close was clean), then `close`, and
/// the object is let go of.
fn finish(
    cx: &mut Cx<'_>,
    this: ObjectId,
    token: Option<u64>,
    code: u16,
    reason: String,
    clean: bool,
) {
    let _ = socket(cx, this, |s| s.state.set(CLOSED));
    if let Some(token) = token {
        cx.page.sockets.by_token.borrow_mut().remove(&token);
    }
    if !clean {
        events::fire(cx, EventTargetRef::Object(this), "error", false, false);
    }
    fire_close(cx, this, code, reason, clean);
    cx.unpin(this);
}

/// A socket event from the host.
pub(crate) fn on_event(cx: &mut Cx<'_>, token: u64, event: WsEvent) {
    let Some(this) = cx.page.sockets.by_token.borrow().get(&token).copied() else {
        return;
    };
    match event {
        WsEvent::Open {
            protocol,
            extensions,
        } => {
            let was_connecting = socket(cx, this, |s| {
                *s.protocol.borrow_mut() = protocol;
                *s.extensions.borrow_mut() = extensions;
                s.state.replace(OPEN) == CONNECTING
            })
            .unwrap_or(false);
            if was_connecting {
                events::fire(cx, EventTargetRef::Object(this), "open", false, false);
            }
        }
        WsEvent::Text(text) => {
            if state_of(cx, this) == OPEN {
                let origin = socket(cx, this, |s| frames::origin_of(&s.url)).unwrap_or_default();
                frames::dispatch_message(
                    cx,
                    EventTargetRef::Object(this),
                    MessageData::Value(Value::String(text)),
                    origin,
                    None,
                );
            }
        }
        WsEvent::Binary(bytes) => {
            if state_of(cx, this) == OPEN {
                let (origin, binary_type) = socket(cx, this, |s| {
                    (frames::origin_of(&s.url), s.binary_type.get())
                })
                .unwrap_or((String::new(), web::BinaryType::Blob));
                let data = match binary_type {
                    web::BinaryType::Arraybuffer => Value::ArrayBuffer(bytes),
                    web::BinaryType::Blob => {
                        Value::Object(crate::file_api::new_blob(cx, bytes, ""))
                    }
                };
                frames::dispatch_message(
                    cx,
                    EventTargetRef::Object(this),
                    MessageData::Value(data),
                    origin,
                    None,
                );
            }
        }
        WsEvent::Close {
            code,
            reason,
            clean,
        } => {
            if state_of(cx, this) != CLOSED {
                finish(cx, this, Some(token), code, reason, clean);
            }
        }
        WsEvent::Error(message) => {
            if state_of(cx, this) != CLOSED {
                cx.page.log(
                    crate::page::ConsoleLevel::Error,
                    format!("WebSocket connection failed: {message}"),
                );
                finish(cx, this, Some(token), 1006, String::new(), false);
            }
        }
    }
}

fn valid_protocol(p: &str) -> bool {
    !p.is_empty()
        && p.bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && !b"()<>@,;:\\\"/[]?={}".contains(&b))
}

impl web::WebSocketImpl for Web {
    fn constructor(
        cx: &mut Cx<'_>,
        url: String,
        protocols: web::StringOrStringSequence,
    ) -> Fallible<ObjectId> {
        let mut parsed = cx.page.resolve_url(&url).ok_or_else(|| {
            Exception::dom(
                "SyntaxError",
                format!("Failed to construct 'WebSocket': The URL '{url}' is invalid."),
            )
        })?;
        match parsed.scheme() {
            "ws" | "wss" => {}
            "http" => parsed
                .set_scheme("ws")
                .ok()
                .ok_or_else(|| Exception::type_error("bad URL"))?,
            "https" => parsed
                .set_scheme("wss")
                .ok()
                .ok_or_else(|| Exception::type_error("bad URL"))?,
            other => {
                return Err(Exception::dom(
                    "SyntaxError",
                    format!(
                        "Failed to construct 'WebSocket': The URL's scheme must be either 'ws' or 'wss'. '{other}' is not allowed."
                    ),
                ));
            }
        }
        if parsed.fragment().is_some() {
            return Err(Exception::dom(
                "SyntaxError",
                "Failed to construct 'WebSocket': The URL contains a fragment identifier.",
            ));
        }
        let protocols = match protocols {
            web::StringOrStringSequence::String(s) => vec![s],
            web::StringOrStringSequence::StringSequence(v) => v,
        };
        for (i, p) in protocols.iter().enumerate() {
            if !valid_protocol(p) || protocols[..i].iter().any(|q| q.eq_ignore_ascii_case(p)) {
                return Err(Exception::dom(
                    "SyntaxError",
                    format!("Failed to construct 'WebSocket': The subprotocol '{p}' is invalid."),
                ));
            }
        }
        let origin = frames::origin_of(&cx.page.url.borrow());
        let token = cx
            .page
            .net()
            .and_then(|net| net.ws_connect(parsed.clone(), protocols, origin));
        let this = cx.page.alloc(WebSocketObject {
            token,
            url: parsed,
            state: Cell::new(CONNECTING),
            protocol: RefCell::new(String::new()),
            extensions: RefCell::new(String::new()),
            binary_type: Cell::new(web::BinaryType::Blob),
            buffered: Cell::new(0),
            made: std::time::Instant::now(),
        });
        cx.pin(this);
        match token {
            Some(token) => {
                cx.page.sockets.by_token.borrow_mut().insert(token, this);
            }
            None => {
                // No host to connect with: the connection fails, later.
                crate::event_loop::queue_task(cx.page, "websocket failure", move |cx| {
                    if state_of(cx, this) != CLOSED {
                        finish(cx, this, None, 1006, String::new(), false);
                    }
                });
            }
        }
        Ok(this)
    }

    fn url(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        socket(cx, this, |s| s.url.to_string())
    }

    fn ready_state(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u16> {
        socket(cx, this, |s| s.state.get())
    }

    fn buffered_amount(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u64> {
        socket(cx, this, |s| s.buffered.get())
    }

    fn extensions(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        socket(cx, this, |s| s.extensions.borrow().clone())
    }

    fn protocol(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        socket(cx, this, |s| s.protocol.borrow().clone())
    }

    fn close(
        cx: &mut Cx<'_>,
        this: ObjectId,
        code: Option<u16>,
        reason: Option<String>,
    ) -> Fallible<()> {
        if let Some(code) = code
            && code != 1000
            && !(3000..=4999).contains(&code)
        {
            return Err(Exception::dom(
                "InvalidAccessError",
                format!(
                    "Failed to execute 'close' on 'WebSocket': The close code must be either 1000, or between 3000 and 4999. {code} is neither."
                ),
            ));
        }
        let reason = reason.unwrap_or_default();
        if reason.len() > 123 {
            return Err(Exception::dom(
                "SyntaxError",
                "Failed to execute 'close' on 'WebSocket': The close reason must not be greater than 123 UTF-8 bytes.",
            ));
        }
        let (state, token) = socket(cx, this, |s| (s.state.get(), s.token))?;
        match state {
            CLOSING | CLOSED => Ok(()),
            CONNECTING => {
                // Fail the connection: the handshake, if it completes, is
                // discarded.
                let _ = socket(cx, this, |s| s.state.set(CLOSING));
                if let (Some(net), Some(token)) = (cx.page.net(), token) {
                    net.ws_send(token, WsOutbound::Close { code, reason });
                }
                crate::event_loop::queue_task(cx.page, "websocket close", move |cx| {
                    if state_of(cx, this) != CLOSED {
                        finish(cx, this, token, 1006, String::new(), false);
                    }
                });
                Ok(())
            }
            _ => {
                let _ = socket(cx, this, |s| s.state.set(CLOSING));
                if let (Some(net), Some(token)) = (cx.page.net(), token) {
                    net.ws_send(token, WsOutbound::Close { code, reason });
                }
                Ok(())
            }
        }
    }

    fn binary_type(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<web::BinaryType> {
        socket(cx, this, |s| s.binary_type.get())
    }

    fn set_binary_type(cx: &mut Cx<'_>, this: ObjectId, value: web::BinaryType) -> Fallible<()> {
        socket(cx, this, |s| s.binary_type.set(value))
    }

    fn send(
        cx: &mut Cx<'_>,
        this: ObjectId,
        data: web::BufferSourceOrBlobOrString,
    ) -> Fallible<()> {
        let (state, token) = socket(cx, this, |s| (s.state.get(), s.token))?;
        if state == CONNECTING {
            return Err(Exception::dom(
                "InvalidStateError",
                "Failed to execute 'send' on 'WebSocket': Still in CONNECTING state.",
            ));
        }
        let message = match data {
            web::BufferSourceOrBlobOrString::String(text) => WsOutbound::Text(text),
            web::BufferSourceOrBlobOrString::BufferSource(bytes) => WsOutbound::Binary(bytes),
            web::BufferSourceOrBlobOrString::Blob(blob) => {
                let (bytes, _) = crate::file_api::blob_contents(cx, blob)?;
                WsOutbound::Binary(bytes.to_vec())
            }
        };
        if state != OPEN {
            return Ok(());
        }
        if let (Some(net), Some(token)) = (cx.page.net(), token) {
            net.ws_send(token, message);
        }
        Ok(())
    }
}

fn close_event<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(bool, u16, &str) -> R,
) -> Fallible<R> {
    cx.page.with::<Event, _>(this, |e| match &e.data {
        EventData::Close {
            was_clean,
            code,
            reason,
        } => Ok(f(*was_clean, *code, reason)),
        _ => Err(Exception::type_error("not a CloseEvent")),
    })?
}

impl web::CloseEventImpl for Web {
    fn constructor(
        cx: &mut Cx<'_>,
        type_: String,
        init: web::CloseEventInit,
    ) -> Fallible<ObjectId> {
        let mut event = Event::new(type_, init.bubbles, init.cancelable, cx.page.clock.peek());
        event.iface = InterfaceId::CloseEvent;
        event.composed = init.composed;
        event.data = EventData::Close {
            was_clean: init.was_clean,
            code: init.code,
            reason: init.reason,
        };
        Ok(cx.page.alloc(event))
    }

    fn was_clean(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        close_event(cx, this, |clean, _, _| clean)
    }

    fn code(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u16> {
        close_event(cx, this, |_, code, _| code)
    }

    fn reason(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        close_event(cx, this, |_, _, reason| reason.to_string())
    }
}
