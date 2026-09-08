//! rencoder_lib -- core library for Rencoder.
//!
//! All business logic lives here. The CLI frontend in `rencoder_cli`
//! consumes this crate and adds clap/dialoguer/signal-handling on top.
//!
//! See `docs/smart_code.md` for the full design.

pub mod analyzer;
pub mod collect;
pub mod config;
pub mod encoding;
pub mod error;
pub mod ffmpeg;
pub mod fps;
pub mod path_util;
pub mod processor;
pub mod types;

/// Optional C FFI bridge to libavformat for high-performance probing.
/// Compiled only when the `c-probe` feature is enabled.
#[cfg(feature = "c-probe")]
pub mod ffi;
