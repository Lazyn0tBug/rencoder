//! Cross-platform file permission helpers (U18).
//!
//! Encapsulates OS-specific permission semantics behind a uniform API.
//!
//! **Current implementation: Unix only.** Built and tested on macOS;
//! Linux shares the `#[cfg(unix)]` branch (will be verified when we
//! cross-compile or run on Linux). Non-Unix platforms are a no-op
//! placeholder — the placeholder is explicit (`let _ = path;`) so the
//! "not implemented" state is a compile-time fact rather than a runtime
//! surprise.
//!
//! Callers see one stable signature and never touch `#[cfg]` themselves.
//! When porting beyond Unix, edit only this file.

use std::io;
use std::path::Path;

/// Apply Unix mode bits appropriate for a config file the user is
/// expected to edit: user-writable, world-readable.
///
/// - Unix: `0o644` (rw-r--r--).
/// - Non-Unix: no-op, `Ok(())` (TBD future work when porting beyond macOS).
pub fn set_config_file_mode<P: AsRef<Path>>(path: P) -> io::Result<()> {
    apply_mode(path, 0o644)
}

/// Apply Unix mode bits appropriate for an app-managed directory the
/// user is expected to drop new files into: user-writable + executable
/// (executable bit needed for directory traversal), world-readable +
/// executable.
///
/// - Unix: `0o755` (rwxr-xr-x).
/// - Non-Unix: no-op, `Ok(())` (TBD future work when porting beyond macOS).
pub fn set_app_dir_mode<P: AsRef<Path>>(path: P) -> io::Result<()> {
    apply_mode(path, 0o755)
}

/// Internal helper that consolidates the cfg-at-definition pattern.
/// `mode_bits` is the POSIX mode (only meaningful on Unix). Non-Unix
/// is a no-op placeholder; the explicit `let _ = path` makes "unused
/// on this target" a compile-time fact, not a runtime surprise.
fn apply_mode<P: AsRef<Path>>(path: P, mode_bits: u32) -> io::Result<()> {
    let path = path.as_ref();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode_bits))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode_bits);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn unique_temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("rencoder_fs_perms_{name}_{}", std::process::id()))
    }

    #[cfg(unix)]
    #[test]
    fn set_config_file_mode_sets_0644_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let path = unique_temp("config");
        std::fs::write(&path, b"").expect("write temp file");
        set_config_file_mode(&path).expect("set_config_file_mode should succeed");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "expected 0o644, got 0o{mode:o}");
        std::fs::remove_file(&path).ok();
    }

    #[cfg(unix)]
    #[test]
    fn set_app_dir_mode_sets_0755_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let path = unique_temp("appdir");
        std::fs::create_dir_all(&path).expect("create temp dir");
        set_app_dir_mode(&path).expect("set_app_dir_mode should succeed");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "expected 0o755, got 0o{mode:o}");
        std::fs::remove_dir_all(&path).ok();
    }

    #[cfg(unix)]
    #[test]
    fn missing_path_propagates_io_error() {
        let nonexistent =
            std::path::PathBuf::from("/nonexistent/never/exists_rencoder_fs_perms_test");
        let result = set_config_file_mode(&nonexistent);
        assert!(result.is_err(), "expected Err for missing path");
    }

    // Non-Unix smoke tests: verify the function compiles AND returns
    // Ok(()) without panicking on missing platform impls. These run
    // only when the test binary is built for a non-Unix target (which
    // we don't currently do — they exist as a compile-time witness
    // that the placeholder branches are reachable).
    #[cfg(not(unix))]
    #[test]
    fn non_unix_set_config_file_mode_is_ok_noop() {
        let result = set_config_file_mode("/nonexistent/never/exists");
        assert!(result.is_ok());
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_set_app_dir_mode_is_ok_noop() {
        let result = set_app_dir_mode("/nonexistent/never/exists");
        assert!(result.is_ok());
    }
}
