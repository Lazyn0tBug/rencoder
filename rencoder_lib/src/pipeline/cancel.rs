//! Cancellation token shared between the UI's `Command::Cancel` and the
//! signal handler. Same token covers both paths: pressing the GUI
//! cancel button sets the flag, and so does SIGINT (via the handler
//! installed in `rencoder_cli::signal`).
//!
//! Implementation: `Arc<AtomicBool>` with `SeqCst` ordering — cancel
//! is rare (one per batch), SeqCst's stronger guarantees are cheap
//! at this frequency and the cost of missing a cancel is "user
//! presses cancel and encode keeps running", so over-synchronize
//! rather than risk the race.
//!
//! Used by `run_ffmpeg_with_progress` (U21, §3.8.6) to poll the flag
//! between ffmpeg progress reads; on detection, the child process is
//! killed and partial output is cleaned up by the existing
//! `PartialOutputHandle` mechanism.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Shareable cancel flag. Cheap to clone (Arc bump), safe to share
/// across threads.
#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    /// New, un-cancelled token.
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signal cancellation. Idempotent — calling twice is harmless.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Has cancellation been signalled? Polled by long-running work
    /// (ffmpeg subprocess reader).
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_token_is_not_cancelled() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn cancel_marks_cancelled() {
        let token = CancelToken::new();
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn clone_shares_state() {
        let a = CancelToken::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled(), "clone must observe cancel via shared Arc");
    }

    #[test]
    fn cancel_is_idempotent() {
        let token = CancelToken::new();
        token.cancel();
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
    }
}
