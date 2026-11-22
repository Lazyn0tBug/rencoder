//! Signal handling: request a cancel, nothing else.
//!
//! Unix (macOS / Linux) covers `SIGINT` / `SIGTERM` / `SIGHUP` via
//! `signal-hook`; Windows covers the console control events
//! (`CTRL_C`, `CTRL_BREAK`, `CTRL_CLOSE`, `CTRL_LOGOFF`,
//! `CTRL_SHUTDOWN`) via `SetConsoleCtrlHandler`.
//!
//! Both sides share the same logic ([`request_cancel`]); the platform
//! divergence lives here, at the definition site. Callers (e.g.
//! `main.rs`) see one stable signature: `install_handler(&cancel)`.
//!
//! The handler **only sets the cancel token**. It does not clean up
//! and it does not exit:
//!
//! - the lib's execution path kills and reaps the in-flight ffmpeg and
//!   removes its half-written output (design doc §3.11.2);
//! - the batch stops at the next file boundary and the records of the
//!   files that finished are written out;
//! - only then does the process exit, with a non-zero status.
//!
//! An interrupt used to cost the whole run: the handler removed the
//! partial file and called `process::exit(1)` right there, so the
//! records of the files that had already been encoded were lost.
//!
//! This mirrors `encode.zsh`'s `trap _cleanup INT TERM HUP`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Where the cancel request goes.
///
/// The handler is installed as the process's **first** thing (design
/// doc §3.11.2): before it is installed, a signal takes the default
/// action -- the process dies, the records are lost, the partial
/// output stays. The path itself comes from the operation directory
/// (§3.6), which the arguments decide; the config can still point it
/// elsewhere, so the slot is filled in again once the paths resolve.
pub type CancelSlot = Arc<Mutex<Option<PathBuf>>>;

/// A fresh, empty cancel slot.
pub fn new_cancel_slot() -> CancelSlot {
    Arc::new(Mutex::new(None))
}

/// Point the handler at the cancel request file.
pub fn set_cancel_path(slot: &CancelSlot, path: &Path) {
    if let Ok(mut slot) = slot.lock() {
        *slot = Some(path.to_path_buf());
    }
}

/// Install the interrupt handler.
///
/// The handler sets `cancel`; the batch notices it (design doc
/// §3.11.2). It is global (one per process): `main.rs` calls this
/// once at process start.
/// `cancel_file` is where the request is written -- the portable
/// channel: whoever can write that file can stop the batch. `None` when
/// the app directory could not be resolved; the in-process flag still
/// carries the cancel then.
pub fn install_handler(slot: &CancelSlot) {
    register_handler(Arc::clone(slot));
}

/// Record the interrupt and ask the batch to stop.
///
/// The signal arrives on a dedicated thread (`signal-hook`'s iterator
/// loop) or on a console-control thread (Windows), so setting an
/// atomic flag is safe here. Everything else -- killing the child,
/// removing the partial output, writing the records, exiting -- happens
/// on the batch's own thread.
fn request_cancel(slot: &CancelSlot, signal: &str) {
    tracing::warn!("received {signal}; asking the batch to stop");
    let cancel_file = match slot.lock() {
        Ok(slot) => slot.clone(),
        Err(_) => {
            tracing::warn!("the cancel path lock is poisoned; the request is dropped");
            return;
        }
    };
    let Some(cancel_file) = cancel_file else {
        // The paths are not resolved yet (design doc §3.6): no batch
        // has started, so there is nothing to cancel -- but the
        // process must still stop, the way it would without the
        // handler. No records exist yet, so nothing is lost.
        tracing::warn!("received {signal} before the paths resolved; stopping the process");
        std::process::exit(130);
    };
    // The file *is* the request -- its content does not matter, so
    // creating it needs no atomic write.
    //
    // A write that fails means the directory is not writable -- which
    // means the records cannot be written either. That is a
    // precondition of the run, checked before it starts (design doc
    // §3.10.7), not something to recover from here.
    if let Err(e) = std::fs::write(&cancel_file, b"") {
        tracing::warn!(error = %e, path = %cancel_file.display(), "could not write the cancel file");
    }
}

/// Unix: listen for SIGINT / SIGTERM / SIGHUP.
///
/// `signal-hook`'s `Signals::forever()` blocks until a signal arrives;
/// each iteration consumes one signal.
#[cfg(unix)]
fn register_handler(slot: CancelSlot) {
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals =
        Signals::new([SIGINT, SIGTERM, SIGHUP]).expect("failed to register signal handlers");
    std::thread::spawn(move || {
        for signal in signals.forever() {
            let name = match signal {
                SIGINT => "SIGINT",
                SIGTERM => "SIGTERM",
                SIGHUP => "SIGHUP",
                _ => "unknown",
            };
            request_cancel(&slot, name);
        }
    });
}

/// Windows: listen for console control events.
#[cfg(windows)]
fn register_handler(slot: CancelSlot) {
    windows::register(slot);
}

/// Any other platform: no interrupt handling (the app still runs; a
/// Ctrl-C just kills it).
#[cfg(not(any(unix, windows)))]
fn register_handler(slot: CancelSlot) {
    let _ = slot;
    tracing::warn!("interrupt handling is not implemented on this platform");
}

/// Windows console control handler.
///
/// The callback has to be a plain `extern "system"` function (no
/// captured state), so the cancel file's path is parked in a
/// `OnceLock` and read from there.
#[cfg(windows)]
mod windows {
    use super::{request_cancel, CancelSlot};
    use std::sync::OnceLock;

    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::{FALSE, TRUE};
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
        CTRL_SHUTDOWN_EVENT,
    };

    static CANCEL: OnceLock<CancelSlot> = OnceLock::new();

    unsafe extern "system" fn on_control(event: u32) -> BOOL {
        let name = match event {
            CTRL_C_EVENT => "CTRL_C_EVENT",
            CTRL_BREAK_EVENT => "CTRL_BREAK_EVENT",
            CTRL_CLOSE_EVENT => "CTRL_CLOSE_EVENT",
            CTRL_LOGOFF_EVENT => "CTRL_LOGOFF_EVENT",
            CTRL_SHUTDOWN_EVENT => "CTRL_SHUTDOWN_EVENT",
            _ => return FALSE,
        };
        if let Some(cancel_slot) = CANCEL.get() {
            request_cancel(cancel_slot, name);
        }
        // TRUE = "handled". Returning FALSE would let the default
        // action run -- the process is killed, the records are lost,
        // the partial output stays (design doc §3.11.2).
        TRUE
    }

    pub(super) fn register(cancel_slot: CancelSlot) {
        assert!(
            CANCEL.set(cancel_slot).is_ok(),
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
