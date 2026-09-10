//! Per-process handle for the in-flight ffmpeg output path. The signal
//! handler reads this on SIGINT (U18 §3.4.5) and removes the half-written
//! file. The per-file batch loop writes to it before each ffmpeg call.
//!
//! The type lives in `rencoder_lib` (not the CLI) so `transcode_batch`
//! can accept it as a parameter without depending on the CLI's
//! `signal` module. The CLI's `signal::CurrentOutput` is just a
//! type-alias to this.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

/// Per-process handle for the in-flight ffmpeg output path.
/// `None` means "no partial output is registered right now".
pub type PartialOutputHandle = Arc<Mutex<Option<PathBuf>>>;

/// Set the current partial output path. Returns the previously-set path
/// (or `None`), so the batch loop can detect file transitions.
pub fn set(handle: &PartialOutputHandle, path: PathBuf) -> Option<PathBuf> {
    handle.lock().replace(path)
}

/// Clear the current partial output. Called after ffmpeg exits so a
/// subsequent SIGINT doesn't remove a successfully-written file (or, on
/// failure, so the next iteration's `set` overwrites cleanly).
pub fn clear(handle: &PartialOutputHandle) {
    handle.lock().take();
}

/// RAII guard: registers `path` on construction, clears on drop. Use
/// inside `transcode_single` around the ffmpeg invocation so cleanup
/// happens even on early return (probe failure paths don't construct
/// the guard, since no ffmpeg is running and no partial exists).
pub struct Guard<'a> {
    handle: Option<&'a PartialOutputHandle>,
}

impl<'a> Guard<'a> {
    /// Register `path` as the current partial output. If `handle` is
    /// `None`, the guard is a no-op (useful for tests and for callers
    /// that haven't installed a signal handler).
    pub fn new(handle: Option<&'a PartialOutputHandle>, path: PathBuf) -> Self {
        if let Some(h) = handle {
            set(h, path);
        }
        Self { handle }
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        if let Some(h) = self.handle {
            clear(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_then_clear() {
        let h: PartialOutputHandle = Arc::new(Mutex::new(None));
        assert!(h.lock().is_none());
        assert_eq!(set(&h, PathBuf::from("/a.mkv")), None);
        assert_eq!(*h.lock(), Some(PathBuf::from("/a.mkv")));
        clear(&h);
        assert!(h.lock().is_none());
    }

    #[test]
    fn set_replaces_previous() {
        let h: PartialOutputHandle = Arc::new(Mutex::new(None));
        set(&h, PathBuf::from("/a.mkv"));
        assert_eq!(
            set(&h, PathBuf::from("/b.mkv")),
            Some(PathBuf::from("/a.mkv"))
        );
        assert_eq!(*h.lock(), Some(PathBuf::from("/b.mkv")));
    }

    #[test]
    fn guard_register_and_clear_on_drop() {
        let h: PartialOutputHandle = Arc::new(Mutex::new(None));
        {
            let _g = Guard::new(Some(&h), PathBuf::from("/x.mkv"));
            assert_eq!(*h.lock(), Some(PathBuf::from("/x.mkv")));
        }
        // Guard dropped at end of scope
        assert!(h.lock().is_none());
    }

    #[test]
    fn guard_noop_when_handle_is_none() {
        // No handle, no panic, no registration
        {
            let _g = Guard::new(None, PathBuf::from("/x.mkv"));
        }
    }

    #[test]
    fn guard_clear_on_early_return_simulation() {
        // Simulate: register, then function returns early (e.g., ffmpeg
        // error). The guard's drop should still clear the registration.
        let h: PartialOutputHandle = Arc::new(Mutex::new(None));
        fn early_return(h: &PartialOutputHandle) -> Result<(), ()> {
            let _g = Guard::new(Some(h), PathBuf::from("/y.mkv"));
            Err(()) // early return; guard drops here
        }
        assert!(early_return(&h).is_err());
        assert!(h.lock().is_none(), "guard must clear on early return");
    }
}
