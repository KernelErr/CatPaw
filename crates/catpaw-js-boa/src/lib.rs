//! Boa-specific pieces that do not depend on the web platform
//! implementation: the job queue behind microtask checkpoints and the
//! console's value formatter. The glue between Boa and `catpaw-web` lives
//! in `catpaw-bindings-boa`.

pub mod inspect;
pub mod jobs;

pub use jobs::Jobs;
