//! Tracing initialization.
//!
//! Two output sinks:
//! - stdout via `fmt` (RUST_LOG / default info)
//! - rotating file under `./log/` (default 10 MB, 7 days)
//!
//! `tracing-log` bridges the library's `log` events into tracing.
//!
//! Span events are enabled so each `info!` / `warn!` carries the span
//! context (function name + entered spans). `#[instrument]` annotations
//! on `encode_cmd` / `analyze_cmd` propagate the span hierarchy.

use std::path::PathBuf;

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

use crate::file_sink::FileSink;

/// Initialize the global tracing subscriber with stdout + file sinks.
///
/// Idempotent: subsequent calls are no-ops (the registry init returns
/// `Err` and we swallow it).
pub fn init() {
    // Bridge log -> tracing so library events flow through our subscriber.
    let _ = tracing_log::LogTracer::init();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let stdout_layer = fmt::layer()
        .with_target(false)
        .with_thread_ids(false)
        .with_span_events(fmt::format::FmtSpan::CLOSE)
        .with_writer(std::io::stderr);

    let file_sink = FileSink::new(log_dir(), 10 * 1024 * 1024, 7)
        .expect("failed to open log file sink");
    let file_layer = fmt::layer()
        .with_target(true)
        .with_thread_ids(false)
        .with_span_events(fmt::format::FmtSpan::CLOSE)
        .with_writer(file_sink)
        .with_filter(EnvFilter::new("info"));

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer)
        .with(file_layer)
        .try_init();
}

fn log_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent() {
        init();
        init();
    }

    #[test]
    fn log_dir_is_under_cwd() {
        let dir = log_dir();
        assert!(dir.ends_with("log"));
    }
}
