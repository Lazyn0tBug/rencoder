//! Signal handling: clean up partial output on SIGINT.
//!
//! The handler is global (one per process), but the partial-output path
//! it consults is per-batch via `Arc<Mutex<Option<PathBuf>>>`. `main.rs`
//! installs the handler at process start, then registers the current
//! output path before each `transcode_single` invocation and clears it
//! after success.
//!
//! Functions here are wired up by Phase 11 (CLI integration). Until
//! then, only the test module uses them; `#[allow(dead_code)]` keeps
//! the main build clean.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

/// Shared state between the SIGINT handler and the per-batch loop.
/// `None` means "no partial output is registered right now".
pub type CurrentOutput = Arc<Mutex<Option<PathBuf>>>;

/// Install a SIGINT handler that removes the registered partial output
/// (if any) and exits with status 1. The returned `CurrentOutput` is
/// the cell main.rs reads/writes to coordinate with the per-file loop.
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

/// Register a partial-output path. Returns the previously-registered
/// path, if any, so callers can detect overwrites.
pub fn register_partial(current: &CurrentOutput, path: PathBuf) -> Option<PathBuf> {
    current.lock().replace(path)
}

/// Clear the registered path (called after the per-file batch completes
/// successfully, so a subsequent SIGINT doesn't delete the just-written
/// file).
pub fn clear_partial(current: &CurrentOutput) {
    current.lock().take();
}

/// Remove a partial file at `path`. Exposed for tests and for callers
/// that want to clean up without going through the signal handler.
pub fn remove_partial(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn register_then_clear() {
        let current: CurrentOutput = Arc::new(Mutex::new(None));
        assert!(current.lock().is_none());

        assert_eq!(
            register_partial(&current, PathBuf::from("/a.mkv")),
            None
        );
        assert_eq!(*current.lock(), Some(PathBuf::from("/a.mkv")));

        clear_partial(&current);
        assert!(current.lock().is_none());
    }

    #[test]
    fn register_replaces_previous_value() {
        let current: CurrentOutput = Arc::new(Mutex::new(None));
        register_partial(&current, PathBuf::from("/a.mkv"));
        // Re-registering returns the previous value.
        assert_eq!(
            register_partial(&current, PathBuf::from("/b.mkv")),
            Some(PathBuf::from("/a.mkv"))
        );
        assert_eq!(*current.lock(), Some(PathBuf::from("/b.mkv")));
        clear_partial(&current);
    }

    #[test]
    fn remove_partial_deletes_existing_file() {
        let dir = std::env::temp_dir().join("rencoder_signal_test_remove");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("partial.mkv");
        fs::write(&path, b"incomplete").unwrap();
        assert!(path.exists());

        remove_partial(&path).unwrap();
        assert!(!path.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_partial_missing_file_errors() {
        let path = std::env::temp_dir().join("rencoder_signal_test_missing.mkv");
        let _ = fs::remove_file(&path);
        let err = remove_partial(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn install_handler_returns_fresh_arc_initially_none() {
        // Calling install_handler multiple times in tests is OK -- each
        // call replaces the global handler closure. The previous Arc is
        // dropped; the handler uses the latest one.
        let current = install_handler();
        assert!(current.lock().is_none());
        // Don't register anything; subsequent SIGINT would just exit.
    }
}
