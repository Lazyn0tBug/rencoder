//! Cancellation token shared between the UI's `Command::Cancel` and the
//! signal handler. Same token covers both paths: pressing the GUI
//! cancel button sets the flag, and so does SIGINT (via the handler
//! installed in `rencoder_cli::signal`).
//!
//! # Implementation
//!
//! `Arc<AtomicBool>` with `SeqCst` ordering. Cancel is rare
//! (one per batch), SeqCst's stronger guarantees are cheap at this
//! frequency, and the cost of missing a cancel is "user presses
//! cancel and encode keeps running" — over-synchronize rather than
//! risk the race.
//!
//! # Used by
//!
//! `run_ffmpeg_with_progress` (U21, §3.8.6) polls the flag between
//! ffmpeg progress reads; on detection, the child process is killed
//! and partial output is cleaned up by the existing
//! `PartialOutputHandle` mechanism.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Shareable cancel flag. Cheap to clone (Arc bump), safe to share
/// across threads.
///
/// Two sources of cancellation:
///
/// - the in-process flag ([`CancelToken::cancel`]) -- the GUI's cancel
///   button, the CLI's own bookkeeping;
/// - **a state file** ([`CancelToken::watching`]) -- `<app_dir>/cancel`.
///   Its mere existence means "cancel". That is the portable channel:
///   the guarantee is one poll interval, on every platform, and anyone
///   can raise it (a script, another process, the UI).
///
/// The file's *content* does not matter -- only its existence -- so
/// raising it needs no atomic write (design doc §3.11.2).
#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    watch: Option<Arc<PathBuf>>,
}

impl CancelToken {
    /// New, un-cancelled token.
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            watch: None,
        }
    }

    /// A token that is also cancelled when `path` exists.
    pub fn watching(path: impl Into<PathBuf>) -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            watch: Some(Arc::new(path.into())),
        }
    }

    /// Signal cancellation. Idempotent — calling twice is harmless.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Has cancellation been signalled? Polled by long-running work
    /// (ffmpeg subprocess reader).
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst) || self.file_cancelled()
    }

    /// Is the watched cancel file there?
    fn file_cancelled(&self) -> bool {
        self.watch
            .as_deref()
            .is_some_and(|path| Path::new(path).exists())
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

    #[test]
    fn a_watched_file_cancels_the_token() {
        // The cancel request is the file's *existence* (design doc
        // §3.11.2) -- its content does not matter.
        let dir = std::env::temp_dir().join(format!(
            "rencoder_cancel_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("the temp dir must be created");
        let path = dir.join("cancel");

        let token = CancelToken::watching(&path);
        assert!(!token.is_cancelled(), "a missing file is not a request");

        std::fs::write(&path, b"").expect("the cancel file must be writable");
        assert!(token.is_cancelled(), "the file's existence is the request");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_watched_file_does_not_cancel_a_token_without_one() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
    }
}
