//! Signal handling: clean up partial output on SIGINT/SIGTERM/SIGHUP.
//!
//! The handler is global (one per process). `main.rs` calls
//! `install_handler()` once at process start, then passes the returned
//! `CurrentOutput` handle to `transcode_batch` (U18 §3.4.5). Each
//! per-file iteration registers the current output path before
//! ffmpeg runs and clears after, so a signal mid-encode removes the
//! half-written file.
//!
//! `signal-hook::iterator::Signals` runs a single background thread
//! listening for `SIGINT` (Ctrl-C), `SIGTERM` (`kill <pid>` default,
//! container orchestrators), and `SIGHUP` (SSH session disconnect) —
//! matching encode.zsh's `trap _cleanup INT TERM HUP` exactly. We
//! chose `signal-hook` over `ctrlc` because the latter only handles
//! SIGINT; for SIGTERM/SIGHUP we'd need a second crate or `libc::signal`
//! C-style handler (which can't access the Arc<Mutex>).
//!
//! The `CurrentOutput` type is re-exported from
//! `rencoder_lib::pipeline::partial_output` so the lib's `transcode_batch`
//! can accept it as a parameter without depending on this module.

use std::sync::Arc;

use parking_lot::Mutex;
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

/// Re-export of the lib type for callers that don't want to depend on
/// `rencoder_lib::pipeline::partial_output` directly.
pub type CurrentOutput = rencoder_lib::pipeline::partial_output::PartialOutputHandle;

/// Install signal handlers (SIGINT + SIGTERM + SIGHUP) that remove the
/// registered partial output and exit with status 1. Spawns one
/// background thread to listen on the signals; the returned
/// `CurrentOutput` is the cell main.rs passes to `transcode_batch`.
pub fn install_handler() -> CurrentOutput {
    let current: CurrentOutput = Arc::new(Mutex::new(None));
    let signal_output = current.clone();

    let mut signals =
        Signals::new([SIGINT, SIGTERM, SIGHUP]).expect("failed to register signal handlers");

    std::thread::spawn(move || {
        // `signals.forever()` blocks until a signal arrives; each
        // iteration consumes one signal. The body always calls
        // `std::process::exit(1)` on signal so the loop runs once per
        // process -- clippy::never_loop is a false positive here.
        //
        // We log via `tracing::warn!` (not `eprintln!`) so the event
        // flows through the global subscriber and lands in both stderr
        // AND the rolling log file -- which is the only way an operator
        // who isn't tailing the terminal sees the cleanup happened.
        #[allow(clippy::never_loop)]
        for sig in signals.forever() {
            let sig_name = match sig {
                SIGINT => "SIGINT",
                SIGTERM => "SIGTERM",
                SIGHUP => "SIGHUP",
                _ => "unknown",
            };
            tracing::warn!("received {sig_name}, interrupting encode");

            if let Some(ctx) = signal_output.lock().take() {
                // TaskContext.output is Option: None for read-only
                // tasks (analyze), where there's nothing to clean up.
                // Some for encode -- the file we may have half-written.
                if let Some(output) = ctx.output.as_ref() {
                    match std::fs::remove_file(output) {
                        Ok(()) => tracing::warn!(
                            "task {}/{} interrupted ({}): removed partial output {}",
                            ctx.index,
                            ctx.total,
                            ctx.source.display(),
                            output.display()
                        ),
                        Err(e) => tracing::warn!(
                            "task {}/{} interrupted ({}): failed to remove partial output {}: {e}",
                            ctx.index,
                            ctx.total,
                            ctx.source.display(),
                            output.display()
                        ),
                    }
                } else {
                    tracing::warn!(
                        "task {}/{} interrupted ({}): no partial output to remove",
                        ctx.index,
                        ctx.total,
                        ctx.source.display()
                    );
                }
            }
            // `process::exit(1)` skips destructors but the tracing
            // subscriber's `fmt::Layer` writes synchronously per event
            // via our FileSink (no internal buffer), and the kernel
            // flushes its page cache during process teardown -- so the
            // log line lands on disk before we exit.
            std::process::exit(1);
        }
    });

    current
}
