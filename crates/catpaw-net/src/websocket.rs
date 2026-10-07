//! WebSocket connections: the opening handshake over the client's own
//! transport (proxy, TLS, address policy and cookies included), then the
//! framed stream.

use futures_util::{SinkExt, StreamExt};
use http::header::{
    CONNECTION, COOKIE, HOST, ORIGIN, SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_KEY,
    SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_VERSION, UPGRADE, USER_AGENT,
};
use http::{HeaderValue, Request, Uri};
use hyper_rustls::MaybeHttpsStream;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message};
use tower_service::Service;
use url::Url;

use crate::client::{NetClient, NetError};
use crate::policy;

/// The framed connection.
pub type WsStream = WebSocketStream<TokioIo<MaybeHttpsStream<TokioIo<TcpStream>>>>;

/// An open WebSocket.
pub struct WsConnection {
    pub stream: WsStream,
    /// The subprotocol the server chose, if any.
    pub protocol: String,
    pub extensions: String,
}

/// A message either way.
#[derive(Clone, Debug)]
pub enum WsMessage {
    Text(String),
    Binary(Vec<u8>),
    /// The connection closed; `code` is 1005 when the peer sent none.
    Close {
        code: u16,
        reason: String,
    },
}

impl NetClient {
    /// Opens a WebSocket to `url` (`ws:` or `wss:`), offering
    /// `protocols`, with `origin` as the page's origin.
    pub async fn websocket(
        &self,
        url: &Url,
        protocols: &[String],
        origin: Option<&str>,
    ) -> Result<WsConnection, NetError> {
        if self.is_replaying() {
            return Err(NetError::Replay("WebSockets are not recorded".to_string()));
        }
        let (scheme, default_port) = match url.scheme() {
            "ws" => ("http", 80),
            "wss" => ("https", 443),
            other => return Err(NetError::UnsupportedScheme(other.to_string())),
        };
        policy::check_host(url, self.config().allow_private_network)
            .map_err(NetError::PrivateAddress)?;
        let host = url
            .host_str()
            .ok_or_else(|| NetError::InvalidUrl(format!("{url} has no host")))?;
        let port = url.port().unwrap_or(default_port);
        let authority: Uri = format!("{scheme}://{host}:{port}")
            .parse()
            .map_err(|e| NetError::InvalidUrl(format!("{url}: {e}")))?;

        let mut connector = self.connector();
        let stream = connector
            .call(authority)
            .await
            .map_err(|e| NetError::Proxy(format!("connecting to {host}:{port}: {e}")))?;
        let stream = TokioIo::new(stream);

        let host_header = if url.port().is_some() {
            format!("{host}:{port}")
        } else {
            host.to_string()
        };
        let path = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_string(),
        };
        let mut request = Request::builder()
            .method("GET")
            .uri(format!("{}://{host_header}{path}", url.scheme()))
            .header(HOST, host_header)
            .header(UPGRADE, "websocket")
            .header(CONNECTION, "Upgrade")
            .header(SEC_WEBSOCKET_KEY, generate_key())
            .header(SEC_WEBSOCKET_VERSION, "13")
            .header(USER_AGENT, self.config().user_agent.as_str());
        if let Some(origin) = origin {
            request = request.header(ORIGIN, origin);
        }
        if !protocols.is_empty() {
            request = request.header(SEC_WEBSOCKET_PROTOCOL, protocols.join(", "));
        }
        if let Some(cookie) = self.cookies().request_header(url) {
            request = request.header(COOKIE, cookie);
        }
        let request = request.body(()).map_err(NetError::Http)?;

        let (stream, response) = tokio_tungstenite::client_async(request, stream)
            .await
            .map_err(|e| NetError::Proxy(format!("WebSocket handshake with {url} failed: {e}")))?;
        self.cookies().store_response(url, response.headers());
        let header = |name| {
            response
                .headers()
                .get(name)
                .and_then(|v: &HeaderValue| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        let protocol = header(SEC_WEBSOCKET_PROTOCOL);
        if !protocol.is_empty() && !protocols.iter().any(|p| p == &protocol) {
            return Err(NetError::Proxy(format!(
                "the server chose a subprotocol that was not offered: {protocol}"
            )));
        }
        Ok(WsConnection {
            stream,
            protocol,
            extensions: header(SEC_WEBSOCKET_EXTENSIONS),
        })
    }
}

impl WsConnection {
    /// The next message from the peer; `None` once the stream ended.
    pub async fn next(&mut self) -> Option<Result<WsMessage, String>> {
        loop {
            let item = self.stream.next().await?;
            match item {
                Ok(Message::Text(text)) => return Some(Ok(WsMessage::Text(text.to_string()))),
                Ok(Message::Binary(bytes)) => return Some(Ok(WsMessage::Binary(bytes.to_vec()))),
                Ok(Message::Close(frame)) => {
                    let (code, reason) = match frame {
                        Some(f) => (u16::from(f.code), f.reason.to_string()),
                        None => (1005, String::new()),
                    };
                    return Some(Ok(WsMessage::Close { code, reason }));
                }
                // Pings are answered by the stream itself; pongs and raw
                // frames carry nothing for the page.
                Ok(_) => continue,
                // The close handshake is complete: the stream is over.
                Err(
                    tokio_tungstenite::tungstenite::Error::ConnectionClosed
                    | tokio_tungstenite::tungstenite::Error::AlreadyClosed,
                ) => return None,
                Err(e) => return Some(Err(e.to_string())),
            }
        }
    }

    pub async fn send(&mut self, message: WsMessage) -> Result<(), String> {
        let message = match message {
            WsMessage::Text(text) => Message::Text(text.into()),
            WsMessage::Binary(bytes) => Message::Binary(bytes.into()),
            WsMessage::Close { code, reason } => Message::Close(Some(CloseFrame {
                code: code.into(),
                reason: reason.into(),
            })),
        };
        self.stream.send(message).await.map_err(|e| e.to_string())
    }
}
