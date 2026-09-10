//! Persistent configuration loaded from JSON.
//!
//! Lets users change runtime knobs (max FPS, encoder preferences, ...) without
//! recompiling. Missing file returns defaults; malformed file is an error.
//! Unknown keys are silently ignored so an old binary reading a newer config
//! still loads.

use std::path::Path;

use serde_json::Value;

use crate::error::EncodeError;
use crate::types::{MaxFpsConfig, VideoFormat};

/// Allowed values for `max_fps`. Includes 0 (capping disabled) plus the
/// canonical video frame rates. Any other value is rejected at config-load
/// time: arbitrary rates like 0.5 or 29.97 aren't meaningful as caps.
const ALLOWED_MAX_FPS: &[f64] = &[0.0, 24.0, 25.0, 30.0, 60.0];

/// Top-level config. Flat scalar knobs plus a nested `encoders` block
/// (added in Phase 4's `select_encoder` plumbing).
#[derive(Debug, Clone)]
pub struct Config {
    /// Cap source FPS at this value when > 0. Must be 0 (disabled) or one
    /// of `ALLOWED_MAX_FPS`. Config::load rejects anything else.
    pub max_fps: f64,
    /// Tolerance band around `max_fps` (matches `encode.zsh` semantics:
    /// a 29.97 source at cap=30 is NOT reduced because 29.97 < 30 + 0.05).
    pub max_fps_eps: f64,
    /// Per-format encoder overrides. `None` means "use built-in default".
    pub encoders: EncoderConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_fps: 0.0,
            max_fps_eps: 0.05,
            encoders: EncoderConfig::default(),
        }
    }
}

/// Per-format encoder overrides. Each field names the encoder to use for
/// that format when set; `None` falls back to the built-in software/NVENC
/// default in `select_encoder`.
#[derive(Debug, Clone, Default)]
pub struct EncoderConfig {
    pub av1: Option<String>,
    pub x264: Option<String>,
    pub hevc: Option<String>,
    pub vp9: Option<String>,
}

impl EncoderConfig {
    /// Look up the override for `format`, if any.
    pub fn for_format(&self, format: VideoFormat) -> Option<&str> {
        match format {
            VideoFormat::Av1 => self.av1.as_deref(),
            VideoFormat::X264 => self.x264.as_deref(),
            VideoFormat::Hevc => self.hevc.as_deref(),
            VideoFormat::Vp9 => self.vp9.as_deref(),
        }
    }
}

impl Config {
    /// Load from a JSON file. Missing file returns `Default::default()`.
    /// `max_fps` must be one of `ALLOWED_MAX_FPS` (0 for disabled, plus
    /// the canonical video frame rates). Anything else surfaces as
    /// `EncodeError::InvalidParam`.
    pub fn load(path: &Path) -> Result<Self, EncodeError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)?;
        let v: Value = serde_json::from_str(&raw)
            .map_err(|e| EncodeError::InvalidParam(format!("config parse: {e}")))?;
        let cfg = Self::from_value(&v);
        if !ALLOWED_MAX_FPS.contains(&cfg.max_fps) {
            return Err(EncodeError::InvalidParam(format!(
                "max_fps must be one of {:?} (0 = disabled)",
                ALLOWED_MAX_FPS
            )));
        }
        Ok(cfg)
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
            encoders: parse_encoders(v.get("encoders")),
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

fn parse_encoders(v: Option<&Value>) -> EncoderConfig {
    let Some(obj) = v.and_then(Value::as_object) else {
        return EncoderConfig::default();
    };
    // Empty strings are rejected: `cache.has_encoder("")` would otherwise
    // always return true (str::contains with empty needle), causing
    // select_encoder to forward "" as the chosen encoder name.
    let parse = |k: &str| -> Option<String> {
        obj.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    EncoderConfig {
        av1: parse("av1"),
        x264: parse("x264"),
        hevc: parse("hevc"),
        vp9: parse("vp9"),
    }
}

impl From<&app::EncodersConfig> for EncoderConfig {
    fn from(e: &app::EncodersConfig) -> Self {
        Self {
            av1: e.av1.clone(),
            x264: e.x264.clone(),
            hevc: e.hevc.clone(),
            vp9: e.vp9.clone(),
        }
    }
}

/// Build a runtime `Config` from an `AppConfig`'s `encode` section.
/// Missing fields fall back to `Config::default()`. The CLI uses this
/// after loading `rencoder.toml` to feed `batch::transcode_batch` and
/// `encoder::select::select_encoder`.
impl Config {
    pub fn from_project(encode: Option<&app::EncodeConfig>) -> Self {
        let Some(enc) = encode else {
            return Self::default();
        };
        Self {
            max_fps: enc.max_fps.unwrap_or(0) as f64,
            max_fps_eps: 0.05,
            encoders: enc
                .encoders
                .as_ref()
                .map(EncoderConfig::from)
                .unwrap_or_default(),
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
        assert!(cfg.encoders.av1.is_none());
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
        let v: Value = serde_json::from_str(r#"{"max_fps": 60, "future_field": true}"#).unwrap();
        let cfg = Config::from_value(&v);
        assert_eq!(cfg.max_fps, 60.0);
    }

    #[test]
    fn max_fps_config_conversion() {
        let cfg = Config {
            max_fps: 30.0,
            max_fps_eps: 0.1,
            ..Default::default()
        };
        let m = cfg.max_fps_config();
        assert_eq!(m.max_fps, 30.0);
        assert_eq!(m.eps, 0.1);
    }

    #[test]
    fn parses_encoder_overrides() {
        let v: Value = serde_json::from_str(
            r#"{
                "max_fps": 30,
                "encoders": {
                    "av1": "libaom-av1",
                    "vp9": "libvpx-vp9"
                }
            }"#,
        )
        .unwrap();
        let cfg = Config::from_value(&v);
        assert_eq!(
            cfg.encoders.for_format(VideoFormat::Av1),
            Some("libaom-av1")
        );
        assert_eq!(
            cfg.encoders.for_format(VideoFormat::Vp9),
            Some("libvpx-vp9")
        );
        assert_eq!(cfg.encoders.for_format(VideoFormat::X264), None);
        assert_eq!(cfg.encoders.for_format(VideoFormat::Hevc), None);
        assert_eq!(cfg.max_fps, 30.0);
    }

    #[test]
    fn malformed_encoders_block_falls_back_to_default() {
        let v: Value = serde_json::from_str(r#"{"encoders": "not an object"}"#).unwrap();
        let cfg = Config::from_value(&v);
        assert!(cfg.encoders.av1.is_none());
    }

    #[test]
    fn encoder_field_must_be_string() {
        let v: Value = serde_json::from_str(r#"{"encoders": {"av1": 123}}"#).unwrap();
        let cfg = Config::from_value(&v);
        assert!(cfg.encoders.av1.is_none());
    }

    #[test]
    fn empty_encoder_string_is_rejected() {
        // Empty string would otherwise pass through to select_encoder and
        // match any cache via str::contains("") == true.
        let v: Value = serde_json::from_str(r#"{"encoders": {"av1": ""}}"#).unwrap();
        let cfg = Config::from_value(&v);
        assert!(cfg.encoders.av1.is_none());
    }

    #[test]
    fn negative_max_fps_rejected_on_load() {
        let dir = std::env::temp_dir().join("rencoder_test_negative_fps");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cfg.json");
        std::fs::write(&path, r#"{"max_fps": -30}"#).unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(matches!(err, EncodeError::InvalidParam(_)));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn sub_one_max_fps_rejected_on_load() {
        // 0.5 is not a canonical video frame rate; the cap value would
        // produce a nonsensical `-r 0.5` command.
        let dir = std::env::temp_dir().join("rencoder_test_subone_fps");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cfg.json");
        std::fs::write(&path, r#"{"max_fps": 0.5}"#).unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(matches!(err, EncodeError::InvalidParam(_)));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn zero_max_fps_accepted_as_disabled() {
        let dir = std::env::temp_dir().join("rencoder_test_zero_fps");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cfg.json");
        std::fs::write(&path, r#"{"max_fps": 0}"#).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.max_fps, 0.0);
        assert!(!cfg.max_fps_config().enabled());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn canonical_max_fps_values_accepted() {
        // 24, 25, 30, 60 are the canonical video frame rates.
        let dir = std::env::temp_dir().join("rencoder_test_canonical_fps");
        std::fs::create_dir_all(&dir).unwrap();
        for &fps in &[24.0, 25.0, 30.0, 60.0] {
            let path = dir.join(format!("cfg_{fps}.json"));
            std::fs::write(&path, format!(r#"{{"max_fps": {fps}}}"#)).unwrap();
            let cfg = Config::load(&path).unwrap();
            assert_eq!(cfg.max_fps, fps, "expected {fps} to be accepted");
            assert!(cfg.max_fps_config().enabled());
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn non_canonical_max_fps_rejected() {
        // NTSC variants (29.97), common but non-canonical rates (23, 50, 120),
        // and arbitrary sub-canonical values must all be rejected.
        let dir = std::env::temp_dir().join("rencoder_test_noncanonical_fps");
        std::fs::create_dir_all(&dir).unwrap();
        for &fps in &[1.0, 23.0, 29.0, 29.97, 50.0, 120.0] {
            let path = dir.join(format!("cfg_{fps}.json"));
            std::fs::write(&path, format!(r#"{{"max_fps": {fps}}}"#)).unwrap();
            let err = Config::load(&path).unwrap_err();
            assert!(
                matches!(err, EncodeError::InvalidParam(_)),
                "expected reject for {fps}, got {err:?}"
            );
            std::fs::remove_file(&path).ok();
        }
    }
}

pub mod app;
pub mod fs_perms;
pub mod path;

// Re-exports for ergonomics: callers can write `rencoder_lib::config::AppConfig`
// instead of `rencoder_lib::config::app::AppConfig`.
pub use app::{
    AnalyzeConfig, AnalyzeFormatArg, AppConfig, EncodeConfig, EncodersConfig, FormatArg,
};
