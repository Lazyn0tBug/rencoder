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
//! - only then does the process exit -- with a non-zero status for an
//!   interrupt (130), and with 0 for a deliberate quit (Ctrl-D, see
//!   [`Reason`]).
//!
//! An interrupt used to cost the whole run: the handler removed the
//! partial file and called `process::exit(1)` right there, so the
//! records of the files that had already been encoded were lost.
//!
//! This mirrors `encode.zsh`'s `trap _cleanup INT TERM HUP`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

/// Why the run is stopping.
///
/// The cancel **file** only says "stop": it is the portable channel,
/// and anyone who can write it can stop a batch. *Why* the run stops is
/// local to this process -- the CLI knows whether the user pressed
/// Ctrl-C (an interrupt) or Ctrl-D (a deliberate quit), and the two
/// leave with different exit codes (design doc §3.11.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    /// An interrupt: Ctrl-C, SIGTERM, SIGHUP or a console control event.
    Interrupt,
    /// A deliberate quit: the user asked the app to stop.
    Quit,
}

/// Record why the run is stopping -- the first reason wins.
///
/// Returns the reason that is now on record.
pub fn record_reason(slot: &CancelSlot, reason: Reason) -> Reason {
    let mut slot = slot.lock();
    *slot.reason.get_or_insert(reason)
}

/// The exit code of a finished run.
///
/// The whole policy lives here, next to the reason it is about -- the
/// frontends only ask. **One function, one rule**: every stop asks
/// it, nobody hands out a code by hand.
///
/// - **Not cancelled** → 0.
/// - **Cancelled by an interrupt** → 130 (128 + SIGINT): the run did
///   not finish, and the shell should know.
/// - **Cancelled by a deliberate quit** (Ctrl-D) → 0: the user asked
///   for it, the records are on disk, the run ended the way they
///   wanted.
/// - Cancelled by *someone else* (the cancel file was written by a
///   script) → 130: we cannot tell what was meant, so the
///   conservative code stands.
pub fn exit_code_for(cancelled: bool, reason: Option<Reason>) -> i32 {
    match (cancelled, reason) {
        (false, _) => 0,
        (true, Some(Reason::Interrupt)) => 130,
        (true, Some(Reason::Quit)) => 0,
        (true, None) => 130,
    }
}

/// The cancel channel's state.
#[derive(Default)]
pub struct CancelState {
    /// Where the cancel request goes -- filled in once the paths
    /// resolve (design doc §3.6).
    path: Option<PathBuf>,
    /// Why the run is stopping -- set by whoever asked.
    reason: Option<Reason>,
}

/// Where the cancel request goes, and why the run is stopping.
///
/// The handler is installed as the process's **first** thing (design
/// doc §3.11.2): before it is installed, a signal takes the default
/// action -- the process dies, the records are lost, the partial
/// output stays. The path itself comes from the operation directory
/// (§3.6), which the arguments decide; the config can still point it
/// elsewhere, so the slot is filled in again once the paths resolve.
pub type CancelSlot = Arc<Mutex<CancelState>>;

/// A fresh, empty cancel slot.
pub fn new_cancel_slot() -> CancelSlot {
    Arc::new(Mutex::new(CancelState::default()))
}

/// Point the handler at the cancel request file.
pub fn set_cancel_path(slot: &CancelSlot, path: &Path) {
    slot.lock().path = Some(path.to_path_buf());
}

/// Why the run is stopping, if it is.
pub fn reason(slot: &CancelSlot) -> Option<Reason> {
    slot.lock().reason
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

/// The user asked to quit (Ctrl-D).
///
/// The **same** stop path as an interrupt -- the cancel request is
/// written, the batch stops at the next file boundary, the records are
/// written -- but the reason is [`Reason::Quit`], so the process exits
/// with 0 instead of 130 (design doc §3.11.2).
pub fn request_quit(slot: &CancelSlot) {
    tracing::warn!("quit requested; asking the batch to stop");
    write_request(slot, Reason::Quit);
}

/// Record the interrupt and ask the batch to stop.
///
/// The signal arrives on a dedicated thread (`signal-hook`'s iterator
/// loop) or on a console-control thread (Windows), so setting the
/// reason is safe here. Everything else -- killing the child, removing
/// the partial output, writing the records, exiting -- happens on the
/// batch's own thread.
fn request_cancel(slot: &CancelSlot, signal: &str) {
    tracing::warn!("received {signal}; asking the batch to stop");
    write_request(slot, Reason::Interrupt);
}

/// Write the cancel request, and record why.
///
/// The reason is **first-writer-wins**: it records *why the run
/// stopped*, and the first request is the one the batch answered. A
/// second request (the user hits Ctrl-C because the quit seems to
/// take long) must not rewrite the story -- otherwise a deliberate
/// quit would leave with 130 again.
fn write_request(slot: &CancelSlot, reason: Reason) {
    record_reason(slot, reason);
    let cancel_file = slot.lock().path.clone();
    let Some(cancel_file) = cancel_file else {
        // The paths are not resolved yet (design doc §3.6): no batch
        // has started, so there is nothing to cancel -- but the
        // process must still stop, the way it would without the
        // handler. No records exist yet, so nothing is lost.
        //
        // This is the **only** place that leaves the process outside
        // the CLI's own exit path, and it is safe to do so here:
        // nothing has been produced (no records to write), the quit
        // key is not being read yet (the terminal is untouched), and
        // the target's lock dies with the process (design doc
        // §3.11.1.1 -- the marker is information, the lock is the
        // claim). The code it leaves with is the same one every other
        // stop asks for.
        tracing::warn!("stop requested before the paths resolved; stopping the process");
        std::process::exit(exit_code_for(true, Some(reason)));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_reason_is_the_one_that_stops_the_run() {
        // The user asks to quit (Ctrl-D), then gets impatient and
        // hits Ctrl-C. The quit is what the batch answered, so the
        // run leaves with 0 -- not 130.
        let slot = new_cancel_slot();
        assert_eq!(record_reason(&slot, Reason::Quit), Reason::Quit);
        assert_eq!(record_reason(&slot, Reason::Interrupt), Reason::Quit);
        assert_eq!(reason(&slot), Some(Reason::Quit));
    }

    #[test]
    fn an_interrupt_is_not_rewritten_by_a_later_quit() {
        let slot = new_cancel_slot();
        assert_eq!(record_reason(&slot, Reason::Interrupt), Reason::Interrupt);
        assert_eq!(record_reason(&slot, Reason::Quit), Reason::Interrupt);
    }

    #[test]
    fn the_exit_codes_follow_the_reason() {
        assert_eq!(exit_code_for(true, Some(Reason::Interrupt)), 130);
        assert_eq!(exit_code_for(true, Some(Reason::Quit)), 0);
    }
}
