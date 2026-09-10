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
        #[allow(clippy::never_loop)]
        for _sig in signals.forever() {
            if let Some(path) = signal_output.lock().take() {
                match std::fs::remove_file(&path) {
                    Ok(()) => eprintln!("interrupted: removed partial output {}", path.display()),
                    Err(e) => eprintln!(
                        "warning: failed to remove partial output {}: {e}",
                        path.display()
                    ),
                }
            }
            std::process::exit(1);
        }
    });

    current
}
