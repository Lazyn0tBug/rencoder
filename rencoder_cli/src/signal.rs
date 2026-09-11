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
        //
        // U19 step 10: the signal handler emits the task-level and
        // batch-level scaffold lines for the interrupt case:
        //     task N/M interrupt
        //     {batch_label} interrupt at {batch_path}, M files successed
        // where M = ctx.index - 1 (tasks completed before this one).
        #[allow(clippy::never_loop)]
        for sig in signals.forever() {
            let sig_name = match sig {
                SIGINT => "SIGINT",
                SIGTERM => "SIGTERM",
                SIGHUP => "SIGHUP",
                _ => "unknown",
            };

            if let Some(ctx) = signal_output.lock().take() {
                // Task-level scaffold: which file was in flight when
                // the signal arrived.
                tracing::warn!("    task {}/{} interrupt", ctx.index, ctx.total);

                // Remove the partial output if there is one. analyze
                // tasks have output = None; nothing to clean up.
                // We log a separate warn on failure but keep the
                // batch-level summary line identical so post-mortem
                // tooling can pattern-match it.
                let removed = match ctx.output.as_ref() {
                    Some(output) => match std::fs::remove_file(output) {
                        Ok(()) => Ok(output.clone()),
                        Err(e) => Err(format!("failed to remove {}: {e}", output.display())),
                    },
                    None => Ok(std::path::PathBuf::new()),
                };
                if let Err(e) = &removed {
                    tracing::warn!("{}", e);
                }

                // Batch-level scaffold: summary of the interrupted
                // batch. M = tasks completed before interrupt.
                let completed = ctx.index.saturating_sub(1);
                tracing::warn!(
                    "{} interrupt at {}, {} files successed",
                    ctx.batch_label,
                    ctx.batch_path.display(),
                    completed
                );

                // Quiet the unused-variable warning from `sig_name`:
                // we keep the variable in scope for the future case
                // where the handler wants to log the signal name in
                // the task-level line.
                let _ = sig_name;
            } else {
                // Signal arrived outside any batch (e.g. before any
                // task started). Still log the signal name for the
                // operator.
                tracing::warn!("received {sig_name} (no active batch)");
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
