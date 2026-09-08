//! Cross-cutting helpers: file-system utilities and time formatting.
//!
//! Two sub-modules:
//!
//! - `fs`   — path normalization + output-path generation
//! - `time` — UTC timestamp formatting
//!
//! These are the few small things every other module reuses. Config
//! and logging live elsewhere (config/ + tracing_init in the CLI)
//! because they have their own concerns.

pub mod fs;
pub mod time;
