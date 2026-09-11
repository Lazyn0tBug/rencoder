//! Per-process handle for the in-flight ffmpeg output path. The signal
//! handler reads this on SIGINT (U18 §3.4.5) and removes the half-written
//! file. The per-file batch loop writes to it before each ffmpeg call.
//!
//! The type lives in `rencoder_lib` (not the CLI) so `transcode_batch`
//! can accept it as a parameter without depending on the CLI's
//! `signal` module. The CLI's `signal::CurrentOutput` is just a
//! type-alias to this.
//!
//! U19: the [`TaskContext`] type moved to [`crate::pipeline::task`]
//! (it is the per-task orchestration record; this module just stores
//! it). Re-exported here for backward-compatible imports.

use std::sync::Arc;

use parking_lot::Mutex;

pub use crate::pipeline::task::TaskContext;

/// Per-process handle for the in-flight ffmpeg task.
/// `None` means "no partial output is registered right now".
pub type PartialOutputHandle = Arc<Mutex<Option<TaskContext>>>;

/// Set the current partial output task. Returns the previously-set
/// context (or `None`), so the batch loop can detect file transitions.
pub fn set(handle: &PartialOutputHandle, ctx: TaskContext) -> Option<TaskContext> {
    handle.lock().replace(ctx)
}

/// Clear the current partial output. Called after ffmpeg exits so a
/// subsequent SIGINT doesn't remove a successfully-written file (or, on
/// failure, so the next iteration's `set` overwrites cleanly).
pub fn clear(handle: &PartialOutputHandle) {
    handle.lock().take();
}

/// RAII guard: registers `ctx` on construction, clears on drop. Use
/// inside `transcode_single` around the ffmpeg invocation so cleanup
/// happens even on early return (probe failure paths don't construct
/// the guard, since no ffmpeg is running and no partial exists).
pub struct Guard<'a> {
    handle: Option<&'a PartialOutputHandle>,
}

impl<'a> Guard<'a> {
    /// Register `ctx` as the current partial output. If `handle` is
    /// `None`, the guard is a no-op (useful for tests and for callers
    /// that haven't installed a signal handler).
    pub fn new(handle: Option<&'a PartialOutputHandle>, ctx: TaskContext) -> Self {
        if let Some(h) = handle {
            set(h, ctx);
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
    use std::path::PathBuf;

    fn ctx(name: &str, output: &str) -> TaskContext {
        TaskContext {
            index: 1,
            total: 3,
            batch_label: "encoding".into(),
            batch_path: PathBuf::from("/in"),
            source: PathBuf::from(name),
            output: Some(PathBuf::from(output)),
        }
    }

    #[test]
    fn set_then_clear() {
        let h: PartialOutputHandle = Arc::new(Mutex::new(None));
        assert!(h.lock().is_none());
        assert_eq!(set(&h, ctx("/in/a.mp4", "/out/a.mkv")), None);
        assert_eq!(
            h.lock().as_ref().map(|c| c.output.clone()),
            Some(Some(PathBuf::from("/out/a.mkv")))
        );
        clear(&h);
        assert!(h.lock().is_none());
    }

    #[test]
    fn set_replaces_previous() {
        let h: PartialOutputHandle = Arc::new(Mutex::new(None));
        set(&h, ctx("/in/a.mp4", "/out/a.mkv"));
        assert_eq!(
            set(&h, ctx("/in/b.mp4", "/out/b.mkv")).map(|c| c.source),
            Some(PathBuf::from("/in/a.mp4"))
        );
        assert_eq!(
            h.lock().as_ref().map(|c| c.source.clone()),
            Some(PathBuf::from("/in/b.mp4"))
        );
    }

    #[test]
    fn guard_register_and_clear_on_drop() {
        let h: PartialOutputHandle = Arc::new(Mutex::new(None));
        {
            let _g = Guard::new(Some(&h), ctx("/in/x.mp4", "/out/x.mkv"));
            assert_eq!(
                h.lock().as_ref().map(|c| c.output.clone()),
                Some(Some(PathBuf::from("/out/x.mkv")))
            );
        }
        // Guard dropped at end of scope
        assert!(h.lock().is_none());
    }

    #[test]
    fn guard_noop_when_handle_is_none() {
        // No handle, no panic, no registration
        {
            let _g = Guard::new(None, ctx("/in/x.mp4", "/out/x.mkv"));
        }
    }

    #[test]
    fn guard_clear_on_early_return_simulation() {
        // Simulate: register, then function returns early (e.g., ffmpeg
        // error). The guard's drop should still clear the registration.
        let h: PartialOutputHandle = Arc::new(Mutex::new(None));
        fn early_return(h: &PartialOutputHandle) -> Result<(), ()> {
            let _g = Guard::new(Some(h), ctx("/in/y.mp4", "/out/y.mkv"));
            Err(()) // early return; guard drops here
        }
        assert!(early_return(&h).is_err());
        assert!(h.lock().is_none(), "guard must clear on early return");
    }
}
