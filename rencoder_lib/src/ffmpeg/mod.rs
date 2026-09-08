//! FFmpeg interaction: probing (probe), encoder selection, command building.
//!
//! The cache helper (`init_encoder_cache`) moved to `pipeline/cache`
//! in the U16 refactor. Re-exported here for backward compatibility.

pub mod probe;

pub use crate::pipeline::cache::init_encoder_cache;
