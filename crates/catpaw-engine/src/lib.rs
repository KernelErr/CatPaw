//! CatPaw engine: ties the pieces into a page that loads and runs.
//!
//! [`Page`] fetches a document (`catpaw-net`, `catpaw-fetch`), parses it
//! while running its scripts (`catpaw-web` on `catpaw-bindings-boa`), and
//! drives the event loop until the page settles. A page lives on one thread;
//! [`with_page`] gives it a dedicated one.

pub mod group;
pub mod net;
pub mod page;

pub use catpaw_js::Value;
pub use catpaw_web::event_loop::{LoopLimits, LoopReport, StopReason};
pub use catpaw_web::frames::FrameId;
pub use catpaw_web::input::InputError;
pub use catpaw_web::settle::{
    Initiator, PendingReport, PendingRequest, PendingTimer, RequestClass, SettlePolicy, TimerClass,
};
pub use catpaw_web::workers::WorkerId;
pub use catpaw_web::{ConsoleLevel, ConsoleMessage, DialogAnswer, DialogPolicy, PageConfig};
pub use group::GroupHandle;
pub use net::{EngineNet, RequestRecord, SharedNet};
pub use page::{
    ActionError, DocumentInfo, EngineError, FrameInfo, MAX_FRAME_DEPTH, MAX_FRAMES, MAX_WORKERS,
    PAGE_STACK_SIZE, Page, PageEvent, PageOptions, ScopeId, WorkerInfo, with_html, with_page,
};
