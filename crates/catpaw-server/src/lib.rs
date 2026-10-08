//! CatPaw's agent server: the tools of the agent protocol (ADR 0006) over
//! MCP on stdio, in front of a browsing session.
//!
//! A [`Session`] holds one browsing context and its tabs; [`McpServer`]
//! speaks MCP for it. Pages are not `Send`, so each group of tabs that
//! share a page runs on a thread of its own and the session sends it
//! closures (see `catpaw_engine::GroupHandle`).

pub mod confirm;
mod files;
pub mod handoff;
mod http;
pub mod journal;
pub mod jsonrpc;
mod local;
pub mod mcp;
mod oracle;
mod output;
pub mod policy;
pub mod profile;
pub mod session;
mod tab;
mod target;

pub use catpaw_protocol::OPTIONAL_TOOLS;
pub use confirm::ApprovalConfig;
pub use journal::JournalConfig;
pub use mcp::{McpServer, serve_stdio, serve_stdio_with};
pub use output::ToolOutput;
pub use policy::{Policy, Preset};
pub use session::{Approval, Host, NoHost, Session, SessionConfig, SessionError};
