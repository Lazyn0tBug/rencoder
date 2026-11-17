//! Path resolution for app-level state (U18).
//!
//! All persistent app state lives under `app_dir` — the directory containing
//! the running binary. This module exposes:
//!
//! - [`app_dir`] — the binary's parent dir
//! - [`resolve_config`] — config file path (`$RENENCODER_CONFIG` > `app_dir`)
//! - [`resolve_log_dir`] — log dir (`$RENENCODER_LOG_DIR` > `<app_dir>/log`)
//! - [`resolve_data_dir`] — data dir (`$RENENCODER_DATA_DIR` > `<app_dir>/data`)
//!
//! See `docs/smart_code.md` §3.6 for the full env-override matrix and
//! §3.4.5 for the resolution contract.
//!
//! Env var overrides take priority over the app_dir default. When `app_dir`
//! can't be determined (e.g. binary deleted between launch and exec, or
//! running on a transient filesystem where `current_exe()` fails), the
//! resolvers return `None` and the caller surfaces a clear error — never
//! silently fall back to CWD.

use std::env;
use std::path::{Path, PathBuf};

/// File name of the user-facing config inside `app_dir`.
const CONFIG_FILE_NAME: &str = "rencoder.toml";

/// Returns the directory containing the running binary (the "app_dir").
///
/// `std::env::current_exe()` is the canonical source. Returns `None` if the
/// path can't be determined — the caller surfaces this as a clear error,
/// not a silent fallback. CWD is never used as a fallback because U18 makes
/// the contract explicit: app state lives with the binary.
pub fn app_dir() -> Option<PathBuf> {
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Resolves the config file path.
///
/// Priority:
///
/// 1. `$RENENCODER_CONFIG` if set and non-empty (explicit override for
///    read-only fs / debug / testing)
/// 2. `<app_dir>/rencoder.toml`
///
/// Returns `None` only when the env var is unset *and* `app_dir()` is
/// unavailable. Empty-string env var is treated as unset (matches POSIX
/// convention).
pub fn resolve_config() -> Option<PathBuf> {
    if let Some(raw) = env::var_os("RENENCODER_CONFIG") {
        if !raw.is_empty() {
            return Some(PathBuf::from(raw));
        }
    }
    app_dir().map(|d| d.join(CONFIG_FILE_NAME))
}

/// Resolves the log directory.
///
/// Priority:
///
/// 1. `$RENENCODER_LOG_DIR` if set and non-empty (explicit override for
///    debugging / CI / read-only systems)
/// 2. `<app_dir>/log`
///
/// Returns `None` only when the env var is unset *and* `app_dir()` is
/// unavailable. Empty-string env var is treated as unset.
pub fn resolve_log_dir() -> Option<PathBuf> {
    if let Some(raw) = env::var_os("RENENCODER_LOG_DIR") {
        if !raw.is_empty() {
            return Some(PathBuf::from(raw));
        }
    }
    app_dir().map(|d| d.join("log"))
}

/// Resolves the data directory (work records, Parquet outputs, etc.).
///
/// Priority:
///
/// 1. `$RENENCODER_DATA_DIR` if set and non-empty
/// 2. `<app_dir>/data`
///
/// Returns `None` only when the env var is unset *and* `app_dir()` is
/// unavailable. Empty-string env var is treated as unset.
pub fn resolve_data_dir() -> Option<PathBuf> {
    if let Some(raw) = env::var_os("RENENCODER_DATA_DIR") {
        if !raw.is_empty() {
            return Some(PathBuf::from(raw));
        }
    }
    app_dir().map(|d| d.join("data"))
}

/// Resolves the NVENC capability record (`nvenc-capability.json`).
///
/// The record lives next to the config in the app dir: it is machine
/// state, like the config, not a log or a work record.
///
/// Priority:
///
/// 1. `$RENENCODER_NVENC_CACHE` if set and non-empty (explicit override
///    for debugging / CI / read-only systems)
/// 2. `<app_dir>/nvenc-capability.json`
///
/// Returns `None` only when the env var is unset *and* `app_dir()` is
/// unavailable. Empty-string env var is treated as unset.
pub fn resolve_nvenc_cache() -> Option<PathBuf> {
    if let Some(raw) = env::var_os("RENENCODER_NVENC_CACHE") {
        if !raw.is_empty() {
            return Some(PathBuf::from(raw));
        }
    }
    app_dir().map(|d| d.join(crate::encoder::nvenc::CAPABILITY_FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::AssertUnwindSafe;
    use std::sync::Mutex;

    // Env var mutation tests must serialize across threads — concurrent
    // `set_var` / `remove_var` on the same key corrupts state for other
    // tests. The mutex is held throughout env mutation + restoration.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Set env var to `value` (or remove if `None`) for the duration of `f`.
    /// Restores the prior value even if `f` panics. Caller can run tests
    /// without worrying about leaking env state to sibling tests.
    fn with_env_var<F: FnOnce()>(key: &str, value: Option<&str>, f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = env::var_os(key);
        match value {
            Some(v) => env::set_var(key, v),
            None => env::remove_var(key),
        }
        let result = std::panic::catch_unwind(AssertUnwindSafe(f));
        // Restore env while still holding the lock — no other test can
        // observe a torn state.
        match prior {
            Some(v) => env::set_var(key, v),
            None => env::remove_var(key),
        }
        // _guard drops here.
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn app_dir_is_some_when_current_exe_known() {
        // The test binary itself has a known location.
        let dir = app_dir().expect("app_dir() should resolve in a test process");
        assert!(
            dir.is_absolute(),
            "app_dir should be absolute, got {}",
            dir.display()
        );
    }

    #[test]
    fn resolve_config_defaults_to_app_dir_when_no_env_override() {
        with_env_var("RENENCODER_CONFIG", None, || {
            let resolved = resolve_config().expect("resolve_config must succeed");
            let expected = app_dir().unwrap().join(CONFIG_FILE_NAME);
            assert_eq!(resolved, expected);
        });
    }

    #[test]
    fn resolve_config_honors_env_override() {
        with_env_var("RENENCODER_CONFIG", Some("/tmp/override.toml"), || {
            let resolved = resolve_config().expect("resolve_config must succeed");
            assert_eq!(resolved, PathBuf::from("/tmp/override.toml"));
        });
    }

    #[test]
    fn resolve_log_dir_defaults_to_app_dir_log() {
        with_env_var("RENENCODER_LOG_DIR", None, || {
            let resolved = resolve_log_dir().expect("resolve_log_dir must succeed");
            assert_eq!(resolved, app_dir().unwrap().join("log"));
        });
    }

    #[test]
    fn resolve_log_dir_honors_env_override() {
        with_env_var("RENENCODER_LOG_DIR", Some("/var/log/rencoder"), || {
            let resolved = resolve_log_dir().expect("resolve_log_dir must succeed");
            assert_eq!(resolved, PathBuf::from("/var/log/rencoder"));
        });
    }

    #[test]
    fn resolve_data_dir_defaults_to_app_dir_data() {
        with_env_var("RENENCODER_DATA_DIR", None, || {
            let resolved = resolve_data_dir().expect("resolve_data_dir must succeed");
            assert_eq!(resolved, app_dir().unwrap().join("data"));
        });
    }

    #[test]
    fn resolve_data_dir_honors_env_override() {
        with_env_var("RENENCODER_DATA_DIR", Some("/srv/rencoder/data"), || {
            let resolved = resolve_data_dir().expect("resolve_data_dir must succeed");
            assert_eq!(resolved, PathBuf::from("/srv/rencoder/data"));
        });
    }

    #[test]
    fn empty_env_var_is_treated_as_unset() {
        // POSIX convention: empty string in env is indistinguishable from
        // unset for most tools. Treating it as "unset" lets users clear an
        // override without `unset`.
        with_env_var("RENENCODER_CONFIG", Some(""), || {
            let resolved = resolve_config().expect("resolve_config must succeed");
            let expected = app_dir().unwrap().join(CONFIG_FILE_NAME);
            assert_eq!(resolved, expected);
        });
    }

    #[test]
    fn env_override_restored_after_test() {
        // Regression guard: helper must restore prior state. Set a sentinel,
        // run a no-op test body, then verify the sentinel is gone.
        env::set_var("RENENCODER_CONFIG", "/tmp/leak_check.toml");
        with_env_var("RENENCODER_CONFIG", None, || {
            // During the closure, RENENCODER_CONFIG is unset.
            assert!(env::var_os("RENENCODER_CONFIG").is_none());
        });
        // After the closure, the sentinel we set above must be restored.
        assert_eq!(
            env::var_os("RENENCODER_CONFIG").as_deref(),
            Some(std::ffi::OsStr::new("/tmp/leak_check.toml"))
        );
        env::remove_var("RENENCODER_CONFIG");
    }
}
