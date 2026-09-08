//! ffmpeg-specific work: build argv, select encoder.
//!
//! The two sub-modules are pure functions — no I/O, no state. They
//! take pre-built `EncodeParams` and an `EncoderCache` and return the
//! pieces the pipeline layer needs to actually run ffmpeg.

pub mod command;
pub mod select;
