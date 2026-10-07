//! The page's network: `catpaw_web::net::NetHost` on top of `catpaw-net`.
//!
//! Requests run on a small tokio runtime owned by the engine; the page
//! thread only ever blocks on it (for parser-blocking scripts) or collects
//! finished requests from a channel.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use bytes::Bytes;
use catpaw_fetch::{FetchedDocument, fetch_document_with};
use catpaw_net::WsMessage;
use catpaw_net::{NetClient, NetConfig, NetError, RequestOptions};
use catpaw_web::net::{
    NetHost, NetRequest, NetResponse, NetResult, RequestKind, WsEvent, WsOutbound,
};
use http::Method;
use http::header::{ACCEPT, HeaderName, HeaderValue, REFERER};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::AbortHandle;
use url::Url;

/// One request the page made, for diagnostics.
#[derive(Clone, Debug)]
pub struct RequestRecord {
    pub method: String,
    pub url: Url,
    pub kind: RequestKind,
    /// The response status, or `None` if the request failed or is pending.
    pub status: Option<u16>,
    /// The start of the request body, when there is one.
    pub body_preview: Option<String>,
    /// Whether an answer (or a failure) came.
    pub finished: bool,
    /// Why the request failed, when it did.
    pub error: Option<String>,
}

/// How much of a request body a record keeps.
const BODY_PREVIEW_BYTES: usize = 4096;

/// What the network tasks report back to the page thread.
enum HostEvent {
    Response(u64, NetResult),
    Socket(u64, WsEvent),
}

/// The network a browsing context's pages share: one runtime and one
/// client, so one cookie jar. `Send` and `Sync`: page threads each build
/// their own [`EngineNet`] over it.
#[derive(Clone)]
pub struct SharedNet {
    runtime: Arc<Runtime>,
    client: Arc<NetClient>,
}

impl SharedNet {
    pub fn new(config: NetConfig) -> Result<Self, NetError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("catpaw-net")
            .enable_all()
            .build()
            .map_err(|e| NetError::Tls(format!("starting the network runtime: {e}")))?;
        // The client spawns connection tasks; create it inside the runtime.
        let client = {
            let _guard = runtime.enter();
            NetClient::new(config)?
        };
        Ok(Self {
            runtime: Arc::new(runtime),
            client: Arc::new(client),
        })
    }

    pub fn client(&self) -> &NetClient {
        &self.client
    }

    /// Runs a future on the network runtime and waits for it. Must not be
    /// called from inside an async context.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }
}

/// The referrer a navigation from `from` to `to` sends, under the default
/// policy (`strict-origin-when-cross-origin`): the whole URL to the same
/// origin, the origin elsewhere, nothing from HTTPS to HTTP or from a
/// document that is not on the web.
pub fn navigation_referrer(from: &Url, to: &Url) -> Option<Url> {
    if !matches!(from.scheme(), "http" | "https") {
        return None;
    }
    if from.scheme() == "https" && to.scheme() == "http" {
        return None;
    }
    let mut referrer = from.clone();
    referrer.set_fragment(None);
    let _ = referrer.set_username("");
    let _ = referrer.set_password(None);
    if from.origin() == to.origin() {
        return Some(referrer);
    }
    referrer.set_path("/");
    referrer.set_query(None);
    Some(referrer)
}

pub struct EngineNet {
    runtime: Arc<Runtime>,
    client: Arc<NetClient>,
    tx: Sender<HostEvent>,
    rx: Receiver<HostEvent>,
    next_token: Cell<u64>,
    inflight: RefCell<HashMap<u64, (AbortHandle, usize)>>,
    /// Open sockets: what to send them, and whether the handshake is still
    /// pending (then the socket counts as in flight).
    sockets: RefCell<HashMap<u64, (UnboundedSender<WsOutbound>, bool)>>,
    /// Socket events taken from the channel while polling for responses.
    socket_events: RefCell<Vec<(u64, WsEvent)>>,
    /// Shared with the hosts of the page's frames: one log per page.
    log: Rc<RefCell<Vec<RequestRecord>>>,
}

async fn perform(client: &NetClient, request: NetRequest) -> NetResult {
    let method = Method::from_bytes(request.method.as_bytes())
        .map_err(|_| format!("invalid method `{}`", request.method))?;
    let mut options = RequestOptions::default();
    // Subresource requests accept anything unless the page says otherwise.
    options
        .headers
        .insert(ACCEPT, HeaderValue::from_static("*/*"));
    // The page decided the referrer (its policy applied): it goes as is.
    if let Some(referrer) = &request.referrer
        && let Ok(value) = HeaderValue::from_str(referrer.as_str())
    {
        options.headers.insert(REFERER, value);
    }
    for (name, value) in &request.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            options.headers.insert(name, value);
        }
    }
    options.body = request.body.map(Bytes::from);
    options.credentials = request.credentials;
    options.follow_redirects = request.follow_redirects;

    let response = client
        .request(method, &request.url, options)
        .await
        .map_err(|e| e.to_string())?;
    Ok(NetResponse {
        status: response.status.as_u16(),
        status_text: response
            .status
            .canonical_reason()
            .unwrap_or_default()
            .to_string(),
        headers: response
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect(),
        redirected: !response.redirect_chain.is_empty(),
        body: response.body.to_vec(),
        url: response.url,
    })
}

impl EngineNet {
    pub fn new(config: NetConfig) -> Result<Self, NetError> {
        Ok(Self::from_shared(&SharedNet::new(config)?))
    }

    /// A host over a context's shared network, with its own request log.
    pub fn from_shared(net: &SharedNet) -> Self {
        let (tx, rx) = channel();
        Self {
            runtime: net.runtime.clone(),
            client: net.client.clone(),
            tx,
            rx,
            next_token: Cell::new(1),
            inflight: RefCell::new(HashMap::new()),
            sockets: RefCell::new(HashMap::new()),
            socket_events: RefCell::new(Vec::new()),
            log: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// The shared network this host runs on.
    pub fn shared(&self) -> SharedNet {
        SharedNet {
            runtime: self.runtime.clone(),
            client: self.client.clone(),
        }
    }

    /// Fetches a document (a navigation or a frame's) and logs it with the
    /// page's other requests.
    pub fn fetch_document(
        &self,
        method: &str,
        url: &Url,
        body: Option<(String, Vec<u8>)>,
        referrer: Option<&Url>,
    ) -> Result<FetchedDocument, NetError> {
        let referrer = referrer.and_then(|from| navigation_referrer(from, url));
        let result = self.block_on(fetch_document_with(
            &self.client,
            method,
            url,
            body,
            referrer.as_ref(),
        ));
        self.record_document(
            method,
            url,
            result.as_ref().ok().map(|d| d.response.status.as_u16()),
        );
        result
    }

    /// A host for another frame of the same page: the same runtime, client
    /// (cookies included) and request log, with requests of its own.
    pub fn child(&self) -> Self {
        let (tx, rx) = channel();
        Self {
            runtime: self.runtime.clone(),
            client: self.client.clone(),
            tx,
            rx,
            next_token: Cell::new(1),
            inflight: RefCell::new(HashMap::new()),
            sockets: RefCell::new(HashMap::new()),
            socket_events: RefCell::new(Vec::new()),
            log: self.log.clone(),
        }
    }

    pub fn client(&self) -> &NetClient {
        &self.client
    }

    /// Runs a future on the network runtime and waits for it. Must not be
    /// called from inside an async context.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    /// Every request made through this host so far.
    pub fn requests(&self) -> Vec<RequestRecord> {
        self.log.borrow().clone()
    }

    /// How many requests the log holds.
    pub fn requests_len(&self) -> usize {
        self.log.borrow().len()
    }

    /// The requests from index `from` on.
    pub fn requests_since(&self, from: usize) -> Vec<RequestRecord> {
        self.log.borrow().iter().skip(from).cloned().collect()
    }

    /// Logs a document fetch made outside the host (a frame's document).
    pub(crate) fn record_document(&self, method: &str, url: &Url, status: Option<u16>) {
        self.log.borrow_mut().push(RequestRecord {
            method: method.to_string(),
            url: url.clone(),
            kind: RequestKind::Document,
            status,
            body_preview: None,
            finished: true,
            error: None,
        });
    }

    fn record(&self, request: &NetRequest) -> usize {
        let mut log = self.log.borrow_mut();
        log.push(RequestRecord {
            method: request.method.clone(),
            url: request.url.clone(),
            kind: request.kind,
            status: None,
            body_preview: request.body.as_ref().map(|body| {
                let end = body.len().min(BODY_PREVIEW_BYTES);
                String::from_utf8_lossy(&body[..end]).into_owned()
            }),
            finished: false,
            error: None,
        });
        log.len() - 1
    }

    fn finish(&self, index: usize, result: &NetResult) {
        if let Some(record) = self.log.borrow_mut().get_mut(index) {
            record.finished = true;
            match result {
                Ok(response) => record.status = Some(response.status),
                Err(e) => record.error = Some(e.clone()),
            }
        }
    }

    /// Sorts an event from the channel: responses go to `out`, socket
    /// events wait for [`NetHost::poll_sockets`].
    fn take(&self, out: &mut Vec<(u64, NetResult)>, event: HostEvent) {
        match event {
            HostEvent::Response(token, result) => self.accept(out, token, result),
            HostEvent::Socket(token, event) => {
                let mut sockets = self.sockets.borrow_mut();
                match &event {
                    WsEvent::Open { .. } => {
                        if let Some(entry) = sockets.get_mut(&token) {
                            entry.1 = false;
                        }
                    }
                    WsEvent::Close { .. } | WsEvent::Error(_) => {
                        sockets.remove(&token);
                    }
                    _ => {}
                }
                self.socket_events.borrow_mut().push((token, event));
            }
        }
    }

    fn accept(&self, out: &mut Vec<(u64, NetResult)>, token: u64, result: NetResult) {
        // A result for an aborted request is dropped.
        if let Some((_, index)) = self.inflight.borrow_mut().remove(&token) {
            self.finish(index, &result);
            out.push((token, result));
        }
    }
}

impl NetHost for EngineNet {
    fn fetch_blocking(&self, request: NetRequest) -> NetResult {
        let index = self.record(&request);
        let result = self.runtime.block_on(perform(&self.client, request));
        self.finish(index, &result);
        result
    }

    fn start(&self, request: NetRequest) -> u64 {
        let token = self.next_token.get();
        self.next_token.set(token + 1);
        let index = self.record(&request);
        let client = self.client.clone();
        let tx = self.tx.clone();
        let task = self.runtime.spawn(async move {
            let result = perform(&client, request).await;
            let _ = tx.send(HostEvent::Response(token, result));
        });
        self.inflight
            .borrow_mut()
            .insert(token, (task.abort_handle(), index));
        token
    }

    fn poll(&self, wait: Option<Duration>) -> Vec<(u64, NetResult)> {
        let mut out = Vec::new();
        if let Some(wait) = wait
            && (self.inflight() > 0 || !self.sockets.borrow().is_empty())
        {
            match self.rx.recv_timeout(wait) {
                Ok(event) => self.take(&mut out, event),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
            }
        }
        while let Ok(event) = self.rx.try_recv() {
            self.take(&mut out, event);
        }
        out
    }

    fn poll_sockets(&self) -> Vec<(u64, WsEvent)> {
        std::mem::take(&mut *self.socket_events.borrow_mut())
    }

    fn abort(&self, token: u64) {
        if let Some((task, _)) = self.inflight.borrow_mut().remove(&token) {
            task.abort();
        }
    }

    fn inflight(&self) -> usize {
        self.inflight.borrow().len()
            + self
                .sockets
                .borrow()
                .values()
                .filter(|(_, connecting)| *connecting)
                .count()
    }

    fn ws_connect(&self, url: Url, protocols: Vec<String>, origin: String) -> Option<u64> {
        let token = self.next_token.get();
        self.next_token.set(token + 1);
        let (out_tx, mut out_rx) = unbounded_channel::<WsOutbound>();
        self.sockets.borrow_mut().insert(token, (out_tx, true));
        let client = self.client.clone();
        let tx = self.tx.clone();
        self.record(&NetRequest::get(url.clone(), RequestKind::Other));
        self.runtime.spawn(async move {
            let mut connection = match client.websocket(&url, &protocols, Some(&origin)).await {
                Ok(connection) => connection,
                Err(e) => {
                    let _ = tx.send(HostEvent::Socket(token, WsEvent::Error(e.to_string())));
                    return;
                }
            };
            let _ = tx.send(HostEvent::Socket(
                token,
                WsEvent::Open {
                    protocol: connection.protocol.clone(),
                    extensions: connection.extensions.clone(),
                },
            ));
            // The close we sent, answered by the peer or by the stream
            // ending.
            let mut we_closed: Option<(u16, String)> = None;
            loop {
                tokio::select! {
                    incoming = connection.next() => match incoming {
                        Some(Ok(WsMessage::Text(text))) => {
                            let _ = tx.send(HostEvent::Socket(token, WsEvent::Text(text)));
                        }
                        Some(Ok(WsMessage::Binary(bytes))) => {
                            let _ = tx.send(HostEvent::Socket(token, WsEvent::Binary(bytes)));
                        }
                        Some(Ok(WsMessage::Close { code, reason })) => {
                            // The stream answers the close frame itself.
                            let _ = tx.send(HostEvent::Socket(token, WsEvent::Close { code, reason, clean: true }));
                            return;
                        }
                        Some(Err(e)) => {
                            let event = match &we_closed {
                                Some((code, reason)) => WsEvent::Close { code: *code, reason: reason.clone(), clean: true },
                                None => WsEvent::Error(e),
                            };
                            let _ = tx.send(HostEvent::Socket(token, event));
                            return;
                        }
                        None => {
                            let event = match &we_closed {
                                Some((code, reason)) => WsEvent::Close { code: *code, reason: reason.clone(), clean: true },
                                None => WsEvent::Close { code: 1006, reason: String::new(), clean: false },
                            };
                            let _ = tx.send(HostEvent::Socket(token, event));
                            return;
                        }
                    },
                    outgoing = out_rx.recv() => match outgoing {
                        Some(WsOutbound::Text(text)) => {
                            if let Err(e) = connection.send(WsMessage::Text(text)).await {
                                let _ = tx.send(HostEvent::Socket(token, WsEvent::Error(e)));
                                return;
                            }
                        }
                        Some(WsOutbound::Binary(bytes)) => {
                            if let Err(e) = connection.send(WsMessage::Binary(bytes)).await {
                                let _ = tx.send(HostEvent::Socket(token, WsEvent::Error(e)));
                                return;
                            }
                        }
                        Some(WsOutbound::Close { code, reason }) => {
                            let code = code.unwrap_or(1000);
                            we_closed = Some((code, reason.clone()));
                            let _ = connection
                                .send(WsMessage::Close { code, reason })
                                .await;
                        }
                        // The page let go of the socket: it is dropped.
                        None => return,
                    },
                }
            }
        });
        Some(token)
    }

    fn ws_send(&self, token: u64, message: WsOutbound) {
        if let Some((sender, _)) = self.sockets.borrow().get(&token) {
            let _ = sender.send(message);
        }
    }

    fn cookies_for(&self, url: &Url) -> String {
        self.client.cookies().script_header(url)
    }

    fn set_cookie(&self, url: &Url, cookie: &str) {
        self.client.cookies().store_from_script(url, cookie);
    }
}
