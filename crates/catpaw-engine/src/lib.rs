//! CatPaw engine: ties the pieces into a page that loads and runs.
//!
//! [`Page`] fetches a document (`catpaw-net`, `catpaw-fetch`), parses it
//! while running its scripts (`catpaw-web` on `catpaw-bindings-boa`), and
//! drives the event loop until the page settles. A page lives on one thread;
//! [`with_page`] gives it a dedicated one.

pub mod net;
pub mod page;

pub use catpaw_web::event_loop::{LoopLimits, LoopReport, StopReason};
pub use catpaw_web::frames::FrameId;
pub use catpaw_web::input::InputError;
pub use catpaw_web::workers::WorkerId;
pub use catpaw_web::{ConsoleLevel, ConsoleMessage, PageConfig};
pub use net::{EngineNet, RequestRecord};
pub use page::{
    ActionError, DocumentInfo, EngineError, FrameInfo, MAX_FRAME_DEPTH, MAX_FRAMES, MAX_WORKERS,
    Page, PageOptions, ScopeId, WorkerInfo, with_html, with_page,
};
