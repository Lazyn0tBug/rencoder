//! ffmpeg-specific work: build argv, select encoder.
//!
//! The two sub-modules are pure functions — no I/O, no state. They
//! take pre-built `EncodeParams` and an `EncoderCache` and return the
//! pieces the pipeline layer needs to actually run ffmpeg.
//!
//! [`nvenc`] is the exception: it does I/O (it probes ffmpeg and keeps
//! the result on disk), because the NVENC capability is a property of
//! the machine, not of a single encode.

pub mod command;
pub mod ffmpeg_arg;
pub mod nvenc;
pub mod select;
