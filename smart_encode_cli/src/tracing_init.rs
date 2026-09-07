//! Tracing initialization.
//!
//! `init` configures the global `tracing` subscriber with:
//! - `tracing-log` bridge so events from the library's `log` calls are
//!   captured (the library still uses `log`; we don't migrate it).
//! - `RUST_LOG` env filter, default `info`.
//! - Plain `fmt` layer without target/timestamp prefixes (matches the
//!   previous `env_logger` config).

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Initialize the global tracing subscriber. Called once from main().
/// Subsequent calls are no-ops (the registry init returns Err but we
/// swallow it -- the typical "already initialized" case in tests).
pub fn init() {
    // Bridge log -> tracing so library events show up alongside ours.
    let _ = tracing_log::LogTracer::init();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false).without_time())
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent() {
        // Calling init twice in the same process is the documented
        // "global state, no second init" case. We swallow the error.
        init();
        init();
    }
}
