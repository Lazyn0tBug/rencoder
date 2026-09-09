//! Work-record data layer.
//!
//! Three sub-modules:
//!
//! - `extract` — source metadata probe, output metadata, stderr capture
//!   (1.1-1.3 in the design doc).
//! - `record` — record construction helpers (1.4).
//! - `store` — DataFrame-based writers (1.5).
//!
//! 1.6 (data record analyze / query layer) is deferred — there's no
//! CLI query surface yet.

pub mod extract;
pub mod record;
pub mod store;

pub use crate::types::{
    EncoderSettings, EncodingOutcome, EncodingRecord, ErrorCategory, ErrorContext, SkippedReason,
};
