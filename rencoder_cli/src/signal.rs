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

use rencoder_lib::pipeline::cancel::CancelToken;

/// Install the interrupt handler.
///
/// The handler sets `cancel`; the batch notices it (design doc
/// §3.11.2). It is global (one per process): `main.rs` calls this
/// once at process start.
pub fn install_handler(cancel: &CancelToken) {
    register_handler(cancel.clone());
}

/// Record the interrupt and ask the batch to stop.
///
/// The signal arrives on a dedicated thread (`signal-hook`'s iterator
/// loop) or on a console-control thread (Windows), so setting an
/// atomic flag is safe here. Everything else -- killing the child,
/// removing the partial output, writing the records, exiting -- happens
/// on the batch's own thread.
fn request_cancel(cancel: &CancelToken, signal: &str) {
    tracing::warn!("received {signal}; asking the batch to stop");
    cancel.cancel();
}

/// Unix: listen for SIGINT / SIGTERM / SIGHUP.
///
/// `signal-hook`'s `Signals::forever()` blocks until a signal arrives;
/// each iteration consumes one signal.
#[cfg(unix)]
fn register_handler(cancel: CancelToken) {
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
            request_cancel(&cancel, name);
        }
    });
}

/// Windows: listen for console control events.
#[cfg(windows)]
fn register_handler(cancel: CancelToken) {
    windows::register(cancel);
}

/// Any other platform: no interrupt handling (the app still runs; a
/// Ctrl-C just kills it).
#[cfg(not(any(unix, windows)))]
fn register_handler(cancel: CancelToken) {
    let _ = cancel;
    tracing::warn!("interrupt handling is not implemented on this platform");
}

/// Windows console control handler.
///
/// The callback has to be a plain `extern "system"` function (no
/// captured state), so the shared `CancelToken` is parked in a
/// `OnceLock` and read from there.
#[cfg(windows)]
mod windows {
    use super::{request_cancel, CancelToken};
    use std::sync::OnceLock;

    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::{FALSE, TRUE};
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
        CTRL_SHUTDOWN_EVENT,
    };

    static CANCEL: OnceLock<CancelToken> = OnceLock::new();

    unsafe extern "system" fn on_control(event: u32) -> BOOL {
        let name = match event {
            CTRL_C_EVENT => "CTRL_C_EVENT",
            CTRL_BREAK_EVENT => "CTRL_BREAK_EVENT",
            CTRL_CLOSE_EVENT => "CTRL_CLOSE_EVENT",
            CTRL_LOGOFF_EVENT => "CTRL_LOGOFF_EVENT",
            CTRL_SHUTDOWN_EVENT => "CTRL_SHUTDOWN_EVENT",
            _ => return FALSE,
        };
        if let Some(cancel) = CANCEL.get() {
            request_cancel(cancel, name);
        }
        // TRUE = "handled". Returning FALSE would let the default
        // action run -- the process is killed, the records are lost,
        // the partial output stays (design doc §3.11.2).
        TRUE
    }

    pub(super) fn register(cancel: CancelToken) {
        assert!(
            CANCEL.set(cancel).is_ok(),
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
