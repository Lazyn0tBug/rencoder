//! Signal handling: clean up partial output on SIGINT.
//!
//! The handler is global (one per process). `main.rs` calls
//! `install_handler()` once at process start, then passes the returned
//! `CurrentOutput` handle to `transcode_batch` (U18 §3.4.5). Each
//! per-file iteration registers the current output path before
//! ffmpeg runs and clears after, so a signal mid-encode removes the
//! half-written file.
//!
//! `ctrlc::set_handler` installs a SIGINT handler that crosses to
//! Windows console events internally — that's the only signal we
//! cover. `encode.zsh`'s `trap _cleanup INT TERM HUP` covers more,
//! but `ctrlc` can't reach SIGTERM/SIGHUP without an additional
//! crate or `libc::signal` (C-style handler can't access the
//! `Arc<Mutex>`). See §3.4.5 of `docs/design.md` for the tradeoff
//! rationale (Windows extension preserved over TERM/HUP coverage).
//!
//! The `CurrentOutput` type is re-exported from
//! `rencoder_lib::pipeline::partial_output` so the lib's `transcode_batch`
//! can accept it as a parameter without depending on this module.

use std::sync::Arc;

use parking_lot::Mutex;

/// Re-export of the lib type for callers that don't want to depend on
/// `rencoder_lib::pipeline::partial_output` directly.
pub type CurrentOutput = rencoder_lib::pipeline::partial_output::PartialOutputHandle;

/// Install a SIGINT handler that removes the registered partial
/// output and exits with status 1. The returned `CurrentOutput` is
/// the cell main.rs passes to `transcode_batch`.
pub fn install_handler() -> CurrentOutput {
    let current: CurrentOutput = Arc::new(Mutex::new(None));
    let signal_output = current.clone();

    ctrlc::set_handler(move || {
        // `ctrlc::set_handler` invokes this closure on SIGINT (cross-
        // platform: Unix signal + Windows console event). We always
        // call `std::process::exit(1)` so the handler runs once per
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
        } else {
            // Signal arrived outside any batch (e.g. before any
            // task started). Still log for the operator.
            tracing::warn!("received SIGINT (no active batch)");
        }

        // `process::exit(1)` skips destructors but the tracing
        // subscriber's `fmt::Layer` writes synchronously per event
        // via our FileSink (no internal buffer), and the kernel
        // flushes its page cache during process teardown -- so the
        // log line lands on disk before we exit.
        std::process::exit(1);
    })
    .expect("failed to register ctrlc handler");

    current
}
