//! FFmpeg interaction: probing (probe).
//!
//! Other ffmeg-related modules moved to `encoder/` (build_argv,
//! select_encoder) and `pipeline/cache` (init_encoder_cache) in the
//! U16 refactor. This module now only contains the probe implementation;
//! callers should import the canonical paths directly:
//!   - `crate::encoder::command::{build_ffmpeg_command, should_skip_encoding}`
//!   - `crate::encoder::select::select_encoder`
//!   - `crate::pipeline::cache::init_encoder_cache`

pub mod probe;
