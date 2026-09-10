//! Signal handling: clean up partial output on SIGINT.
//!
//! The handler is global (one per process). `main.rs` calls
//! `install_handler()` once at process start, then passes the returned
//! `CurrentOutput` handle to `transcode_batch` (U18 §3.4.5). Each
//! per-file iteration registers the current output path before
//! ffmpeg runs and clears after, so a SIGINT mid-encode removes the
//! half-written file.
//!
//! The `CurrentOutput` type is re-exported from
//! `rencoder_lib::pipeline::partial_output` so the lib's `transcode_batch`
//! can accept it as a parameter without depending on this module.

use std::sync::Arc;

use parking_lot::Mutex;

/// Re-export of the lib type for callers that don't want to depend on
/// `rencoder_lib::pipeline::partial_output` directly. The two are
/// literally the same type (`Arc<Mutex<Option<PathBuf>>>`).
pub type CurrentOutput = rencoder_lib::pipeline::partial_output::PartialOutputHandle;

/// Install a SIGINT handler that removes the registered partial output
/// (if any) and exits with status 1. The returned `CurrentOutput` is
/// the cell main.rs passes to `transcode_batch`.
pub fn install_handler() -> CurrentOutput {
    let current: CurrentOutput = Arc::new(Mutex::new(None));
    let signal_output = current.clone();

    ctrlc::set_handler(move || {
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
    })
    .expect("failed to install SIGINT handler");

    current
}
