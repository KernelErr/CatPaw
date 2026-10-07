//! CatPaw networking: an HTTP/1.1 + HTTP/2 client over hyper and rustls with
//! per-client cookie jars, redirect handling, content decoding, and Web Bot
//! Auth request signing (see ADR 0003).
//!
//! This crate is deliberately below the Fetch specification: it moves bytes
//! and cookies. CORS, referrer policy, CSP and caching live in `catpaw-fetch`.

pub mod bot_auth;
pub mod client;
pub mod cookies;
pub mod decode;
pub mod policy;
pub mod websocket;

pub use bot_auth::{BotAuthConfig, BotAuthError, BotAuthSigner, KeyPair, SignedHeaders};
pub use bytes::Bytes;
pub use client::{NetClient, NetConfig, NetError, RequestOptions, Response};
pub use cookies::CookieJar;
pub use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
pub use url::Url;
pub use websocket::{WsConnection, WsMessage, WsStream};
