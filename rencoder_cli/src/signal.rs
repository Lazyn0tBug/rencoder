//! Signal handling: clean up partial output on interrupt.
//!
//! Unix (macOS / Linux) covers `SIGINT` / `SIGTERM` / `SIGHUP` via
//! `signal-hook`; Windows covers the console control events
//! (`CTRL_C`, `CTRL_BREAK`, `CTRL_CLOSE`, `CTRL_LOGOFF`,
//! `CTRL_SHUTDOWN`) via `SetConsoleCtrlHandler`.
//!
//! Both sides share the same cleanup logic ([`cleanup_and_exit`]); the
//! platform divergence lives here, at the definition site. Callers
//! (e.g. `main.rs`) see one stable signature:
//! `install_handler() -> CurrentOutput`.
//!
//! The handler is global (one per process). `main.rs` calls
//! `install_handler()` once at process start, then passes the returned
//! `CurrentOutput` handle to `transcode_batch` (U18 §3.4.5). Each
//! per-file iteration registers the current output path before
//! ffmpeg runs and clears after, so an interrupt mid-encode removes
//! the half-written file.
//!
//! This mirrors `encode.zsh`'s `trap _cleanup INT TERM HUP`.
//!
//! The `CurrentOutput` type is re-exported from
//! `rencoder_lib::pipeline::partial_output` so the lib's `transcode_batch`
//! can accept it as a parameter without depending on this module.

use std::sync::Arc;

use parking_lot::Mutex;

/// Re-export of the lib type for callers that don't want to depend on
/// `rencoder_lib::pipeline::partial_output` directly.
pub type CurrentOutput = rencoder_lib::pipeline::partial_output::PartialOutputHandle;

/// Install the interrupt handler that removes the registered partial
/// output and exits with status 1. The returned `CurrentOutput` is
/// the cell main.rs passes to `transcode_batch`.
pub fn install_handler() -> CurrentOutput {
    let current: CurrentOutput = Arc::new(Mutex::new(None));
    register_handler(current.clone());
    current
}

/// Remove the partial output, log the interrupt, exit(1).
///
/// Shared by both platforms: the cleanup itself uses stdlib APIs that
/// work everywhere (`Arc<Mutex<..>>`, `std::fs::remove_file`,
/// `std::process::exit`, `tracing::warn!`).
///
/// We log via `tracing::warn!` (not `eprintln!`) so the event flows
/// through the global subscriber and lands in both stderr AND the
/// rolling log file -- which is the only way an operator who isn't
/// tailing the terminal sees the cleanup happened.
///
/// U19 step 10: the handler emits the task-level and batch-level
/// scaffold lines for the interrupt case:
/// ```text
/// task N/M interrupt
/// {batch_label} interrupt at {batch_path}, M files successed
/// ```
/// where M = ctx.index - 1 (tasks completed before this one).
#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
fn cleanup_and_exit(current: &CurrentOutput, signal: &str) -> ! {
    if let Some(ctx) = current.lock().take() {
        // Task-level scaffold: which file was in flight when the
        // signal arrived.
        tracing::warn!("    task {}/{} interrupt", ctx.index, ctx.total);

        // Remove the partial output if there is one. analyze tasks
        // have output = None; nothing to clean up. We log a separate
        // warn on failure but keep the batch-level summary line
        // identical so post-mortem tooling can pattern-match it.
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

        // Batch-level scaffold: summary of the interrupted batch.
        // M = tasks completed before interrupt.
        let completed = ctx.index.saturating_sub(1);
        tracing::warn!(
            "{} interrupt at {}, {} files successed",
            ctx.batch_label,
            ctx.batch_path.display(),
            completed
        );
    } else {
        // Interrupt arrived outside any batch (e.g. before any task
        // started). Still log for the operator.
        tracing::warn!("received {signal} (no active batch)");
    }

    // `process::exit(1)` skips destructors but the tracing subscriber's
    // `fmt::Layer` writes synchronously per event via our FileSink (no
    // internal buffer), and the kernel flushes its page cache during
    // process teardown -- so the log line lands on disk before we exit.
    std::process::exit(1);
}

/// Unix: listen for SIGINT / SIGTERM / SIGHUP.
///
/// `signal-hook`'s `Signals::forever()` blocks until a signal arrives;
/// each iteration consumes one signal. The body always calls
/// `std::process::exit(1)`, so the loop runs once per process --
/// clippy::never_loop is a false positive here.
#[cfg(unix)]
fn register_handler(current: CurrentOutput) {
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals =
        Signals::new([SIGINT, SIGTERM, SIGHUP]).expect("failed to register signal handlers");
    std::thread::spawn(move || {
        #[allow(clippy::never_loop)]
        for signal in signals.forever() {
            let name = match signal {
                SIGINT => "SIGINT",
                SIGTERM => "SIGTERM",
                SIGHUP => "SIGHUP",
                _ => "unknown",
            };
            cleanup_and_exit(&current, name);
        }
    });
}

/// Windows: listen for console control events.
#[cfg(windows)]
fn register_handler(current: CurrentOutput) {
    windows::register(current);
}

/// Any other platform: no interrupt handling (the app still runs; it
/// just leaves partial output behind on Ctrl-C).
#[cfg(not(any(unix, windows)))]
fn register_handler(current: CurrentOutput) {
    let _ = current;
    tracing::warn!("interrupt handling is not implemented on this platform");
}

/// Windows console control handler.
///
/// The callback has to be a plain `extern "system"` function (no
/// captured state), so the shared `CurrentOutput` handle is parked in
/// a `OnceLock` and read from there.
#[cfg(windows)]
mod windows {
    use super::{cleanup_and_exit, CurrentOutput};
    use std::sync::OnceLock;

    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::{FALSE, TRUE};
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
        CTRL_SHUTDOWN_EVENT,
    };

    static CURRENT: OnceLock<CurrentOutput> = OnceLock::new();

    unsafe extern "system" fn on_control(event: u32) -> BOOL {
        let name = match event {
            CTRL_C_EVENT => "CTRL_C_EVENT",
            CTRL_BREAK_EVENT => "CTRL_BREAK_EVENT",
            CTRL_CLOSE_EVENT => "CTRL_CLOSE_EVENT",
            CTRL_LOGOFF_EVENT => "CTRL_LOGOFF_EVENT",
            CTRL_SHUTDOWN_EVENT => "CTRL_SHUTDOWN_EVENT",
            _ => return FALSE,
        };
        if let Some(current) = CURRENT.get() {
            cleanup_and_exit(current, name);
        }
        FALSE
    }

    pub(super) fn register(current: CurrentOutput) {
        assert!(
            CURRENT.set(current).is_ok(),
            "signal handler already installed"
        );
        // SAFETY: the callback uses the Windows ABI and stays valid for
        // the lifetime of the process; the shared state it reads is
        // initialized right above.
        let registered = unsafe { SetConsoleCtrlHandler(Some(on_control), TRUE) };
        assert_ne!(
            registered,
            FALSE,
            "failed to register console handler: {}",
            std::io::Error::last_os_error()
        );
    }
}
