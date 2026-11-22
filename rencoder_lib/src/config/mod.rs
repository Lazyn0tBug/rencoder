//! Configuration: the user-facing TOML schema and the merge that turns
//! it into the effective run configuration.
//!
//! Design doc §3.10.5. Three sources, merged in one place:
//!
//! ```text
//! defaults  <-  config file  <-  command line
//! ```
//!
//! The merged result is [`crate::types::EncodeParams`] -- there is no
//! second "runtime config" type. The former `Config` struct (encoder
//! overrides + ffmpeg argv knobs, derived once at startup) is gone:
//! both fields moved into `EncodeParams`, so "what is this run
//! configured with" has exactly one answer.

pub mod app;
pub mod fs_perms;
pub mod paths;

// Re-exports for ergonomics: callers can write `rencoder_lib::config::AppConfig`
// instead of `rencoder_lib::config::app::AppConfig`.
pub use app::{
    AnalyzeConfig, AnalyzeFormatArg, AppConfig, EncodeConfig, EncodersConfig, FfmpegArgsConfig,
    FormatArg, PathsConfig,
};
pub use paths::Paths;
