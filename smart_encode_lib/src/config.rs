//! Persistent configuration loaded from JSON.
//!
//! Lets users change runtime knobs (max FPS, encoder preferences, ...) without
//! recompiling. Missing file returns defaults; malformed file is an error.
//! Unknown keys are silently ignored so an old binary reading a newer config
//! still loads.

use std::path::Path;

use serde_json::Value;

use crate::error::EncodeError;
use crate::types::MaxFpsConfig;

/// Top-level config. Flat for now; a nested `encoders` block lands with
/// Phase 4 when `select_encoder` gains `config_path` plumbing.
#[derive(Debug, Clone)]
pub struct Config {
    /// Cap source FPS at this value when > 0. `0` (default) disables capping.
    pub max_fps: f64,
    /// Tolerance band around `max_fps` (matches `encode.zsh` semantics:
    /// a 29.97 source at cap=30 is NOT reduced because 29.97 < 30 + 0.05).
    pub max_fps_eps: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_fps: 0.0,
            max_fps_eps: 0.05,
        }
    }
}

impl Config {
    /// Load from a JSON file. Missing file returns `Default::default()`.
    pub fn load(path: &Path) -> Result<Self, EncodeError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)?;
        let v: Value = serde_json::from_str(&raw)
            .map_err(|e| EncodeError::InvalidParam(format!("config parse: {e}")))?;
        Ok(Self::from_value(&v))
    }

    fn from_value(v: &Value) -> Self {
        let defaults = Self::default();
        Self {
            max_fps: v
                .get("max_fps")
                .and_then(Value::as_f64)
                .unwrap_or(defaults.max_fps),
            max_fps_eps: v
                .get("max_fps_eps")
                .and_then(Value::as_f64)
                .unwrap_or(defaults.max_fps_eps),
        }
    }

    /// Build the runtime `MaxFpsConfig` from this config.
    pub fn max_fps_config(&self) -> MaxFpsConfig {
        MaxFpsConfig {
            max_fps: self.max_fps,
            eps: self.max_fps_eps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_returns_defaults() {
        let cfg = Config::load(Path::new("/nonexistent/never.json")).unwrap();
        assert_eq!(cfg.max_fps, 0.0);
        assert_eq!(cfg.max_fps_eps, 0.05);
    }

    #[test]
    fn empty_object_uses_all_defaults() {
        let v: Value = serde_json::from_str("{}").unwrap();
        let cfg = Config::from_value(&v);
        assert_eq!(cfg.max_fps, 0.0);
        assert_eq!(cfg.max_fps_eps, 0.05);
    }

    #[test]
    fn reads_max_fps_and_eps() {
        let v: Value = serde_json::from_str(r#"{"max_fps": 30, "max_fps_eps": 0.1}"#).unwrap();
        let cfg = Config::from_value(&v);
        assert_eq!(cfg.max_fps, 30.0);
        assert_eq!(cfg.max_fps_eps, 0.1);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let v: Value =
            serde_json::from_str(r#"{"max_fps": 60, "future_field": true}"#).unwrap();
        let cfg = Config::from_value(&v);
        assert_eq!(cfg.max_fps, 60.0);
    }

    #[test]
    fn max_fps_config_conversion() {
        let cfg = Config {
            max_fps: 30.0,
            max_fps_eps: 0.1,
        };
        let m = cfg.max_fps_config();
        assert_eq!(m.max_fps, 30.0);
        assert_eq!(m.eps, 0.1);
    }
}
