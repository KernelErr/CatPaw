//! The page's network: `catpaw_web::net::NetHost` on top of `catpaw-net`.
//!
//! Requests run on a small tokio runtime owned by the engine; the page
//! thread only ever blocks on it (for parser-blocking scripts) or collects
//! finished requests from a channel.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use bytes::Bytes;
use catpaw_net::{NetClient, NetConfig, NetError, RequestOptions};
use catpaw_web::net::{NetHost, NetRequest, NetResponse, NetResult, RequestKind};
use http::Method;
use http::header::{ACCEPT, HeaderName, HeaderValue, REFERER};
use tokio::runtime::Runtime;
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
}

pub struct EngineNet {
    runtime: Runtime,
    client: Arc<NetClient>,
    tx: Sender<(u64, NetResult)>,
    rx: Receiver<(u64, NetResult)>,
    next_token: Cell<u64>,
    inflight: RefCell<HashMap<u64, (AbortHandle, usize)>>,
    log: RefCell<Vec<RequestRecord>>,
}

/// The `Referer` value for a request from `referrer` to `target`, under the
/// default policy (`strict-origin-when-cross-origin`).
fn referer_for(referrer: &Url, target: &Url) -> Option<String> {
    if !matches!(referrer.scheme(), "http" | "https") {
        return None;
    }
    if referrer.scheme() == "https" && target.scheme() != "https" {
        return None;
    }
    if referrer.origin() == target.origin() {
        let mut full = referrer.clone();
        full.set_fragment(None);
        let _ = full.set_username("");
        let _ = full.set_password(None);
        Some(full.to_string())
    } else {
        Some(format!("{}/", referrer.origin().ascii_serialization()))
    }
}

async fn perform(client: &NetClient, request: NetRequest) -> NetResult {
    let method = Method::from_bytes(request.method.as_bytes())
        .map_err(|_| format!("invalid method `{}`", request.method))?;
    let mut options = RequestOptions::default();
    // Subresource requests accept anything unless the page says otherwise.
    options
        .headers
        .insert(ACCEPT, HeaderValue::from_static("*/*"));
    if let Some(referer) = request
        .referrer
        .as_ref()
        .and_then(|r| referer_for(r, &request.url))
        && let Ok(value) = HeaderValue::from_str(&referer)
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
        let (tx, rx) = channel();
        Ok(Self {
            runtime,
            client: Arc::new(client),
            tx,
            rx,
            next_token: Cell::new(1),
            inflight: RefCell::new(HashMap::new()),
            log: RefCell::new(Vec::new()),
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

    /// Every request made through this host so far.
    pub fn requests(&self) -> Vec<RequestRecord> {
        self.log.borrow().clone()
    }

    fn record(&self, request: &NetRequest) -> usize {
        let mut log = self.log.borrow_mut();
        log.push(RequestRecord {
            method: request.method.clone(),
            url: request.url.clone(),
            kind: request.kind,
            status: None,
        });
        log.len() - 1
    }

    fn finish(&self, index: usize, result: &NetResult) {
        if let (Some(record), Ok(response)) = (self.log.borrow_mut().get_mut(index), result) {
            record.status = Some(response.status);
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
            let _ = tx.send((token, result));
        });
        self.inflight
            .borrow_mut()
            .insert(token, (task.abort_handle(), index));
        token
    }

    fn poll(&self, wait: Option<Duration>) -> Vec<(u64, NetResult)> {
        let mut out = Vec::new();
        if let Some(wait) = wait
            && !self.inflight.borrow().is_empty()
        {
            match self.rx.recv_timeout(wait) {
                Ok((token, result)) => self.accept(&mut out, token, result),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
            }
        }
        while let Ok((token, result)) = self.rx.try_recv() {
            self.accept(&mut out, token, result);
        }
        out
    }

    fn abort(&self, token: u64) {
        if let Some((task, _)) = self.inflight.borrow_mut().remove(&token) {
            task.abort();
        }
    }

    fn inflight(&self) -> usize {
        self.inflight.borrow().len()
    }

    fn cookies_for(&self, url: &Url) -> String {
        self.client.cookies().script_header(url)
    }

    fn set_cookie(&self, url: &Url, cookie: &str) {
        self.client.cookies().store_from_script(url, cookie);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn referer_follows_the_default_policy() {
        let page = url("https://user:pw@example.com/a/b?q=1#frag");
        assert_eq!(
            referer_for(&page, &url("https://example.com/x")).as_deref(),
            Some("https://example.com/a/b?q=1")
        );
        assert_eq!(
            referer_for(&page, &url("https://other.example/x")).as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(referer_for(&page, &url("http://example.com/x")), None);
        assert_eq!(
            referer_for(&url("about:blank"), &url("https://example.com/")),
            None
        );
    }
}
