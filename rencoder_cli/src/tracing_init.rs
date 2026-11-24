//! Tracing initialization.
//!
//! Two output sinks:
//! - stdout via `fmt` (RUST_LOG / default info)
//! - rotating file under `<app_dir>/log/` (U18; `$RENENCODER_LOG_DIR` overrides)
//!   default 10 MB, 7 days
//!
//! v1.2 follow-up #4: lib emits `tracing` events directly (was
//! `log` bridged via `tracing-log`). The bridge is dropped here.
//!
//! Span events are enabled so each `info!` / `warn!` carries the span
//! context (function name + entered spans). `#[instrument]` annotations
//! on `encode_cmd` / `analyze_cmd` propagate the span hierarchy.

use std::path::Path;

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

use crate::file_sink::FileSink;

/// Initialize the global tracing subscriber with stdout + file sinks.
///
/// Idempotent: subsequent calls are no-ops (the registry init returns
/// `Err` and we swallow it). Filesystem errors (log dir creation /
/// mode-bit set) propagate via `Result` so the binary exits with a
/// clean error message instead of panicking.
pub fn init(log_dir: &Path) -> anyhow::Result<()> {
    use anyhow::Context;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let stdout_layer = fmt::layer()
        .with_target(false)
        .with_thread_ids(false)
        .with_span_events(fmt::format::FmtSpan::CLOSE)
        .with_writer(std::io::stderr);

    std::fs::create_dir_all(log_dir).context("create log dir")?;
    rencoder_lib::platform::perms::set_app_dir_mode(log_dir).context("set log dir mode bits")?;

    let file_sink =
        FileSink::new(log_dir, 10 * 1024 * 1024, 7).context("failed to open log file sink")?;
    // File layer is for post-mortem analysis: keep it scannable.
    // - `.compact()` produces Format::Compact, which renders span
    //   fields inline without the Pretty field formatter's ANSI
    //   styling. (`with_ansi(false)` alone is not enough — the
    //   default `Format::Full` uses `Pretty` for span-field display,
    //   which emits escape codes regardless of the layer-level
    //   ANSI flag. Confirmed via `xxd` on a sample run.)
    // - Keep `target` (module path) so post-mortem analysis can grep
    //   by module.
    // - No thread ids (process is single-threaded; rare rayon threads
    //   would just clutter).
    let file_layer = fmt::layer()
        .compact()
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(false)
        .with_writer(file_sink)
        .with_filter(EnvFilter::new("info"));

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer)
        .with(file_layer)
        .try_init();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("rencoder_log_init_{}", std::process::id()));
        init(&dir).expect("first init");
        init(&dir).expect("second init (idempotent)");
    }
}
