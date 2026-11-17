//! Persistent configuration loaded from JSON.
//!
//! Lets users change runtime knobs (max FPS, encoder preferences, ...) without
//! recompiling. Missing file returns defaults; malformed file is an error.
//! Unknown keys are silently ignored so an old binary reading a newer config
//! still loads.

use std::path::Path;

use serde_json::Value;

use crate::error::EncodeError;
use crate::types::VideoFormat;

/// Top-level config. Flat scalar knobs plus a nested `encoders` block
/// (added in Phase 4's `select_encoder` plumbing).
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Per-format encoder overrides. `None` means "use built-in default".
    pub encoders: EncoderConfig,
    /// ffmpeg argv construction knobs (Tiers follow-up review). Mirrors
    /// `EncodeConfig.ffmpeg_args` in the user-facing AppConfig; defaults
    /// are populated by `Config::from_encode_config`.
    pub ffmpeg_args: crate::config::app::FfmpegArgsConfig,
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
    ///
    /// Legacy path: the user-facing config is TOML (`AppConfig`), and the
    /// frame-rate cap lives there — it is *not* mirrored here (see
    /// design doc §3.10.2: one field, one conversion entry point).
    /// This JSON path only carries the encoder overrides and the ffmpeg
    /// argv knobs, and is kept for the lib's internal tests.
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
            ffmpeg_args: defaults.ffmpeg_args,
            encoders: parse_encoders(v.get("encoders")),
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
    pub fn from_encode_config(encode: Option<&app::EncodeConfig>) -> Self {
        let Some(enc) = encode else {
            return Self::default();
        };
        Self {
            encoders: enc
                .encoders
                .as_ref()
                .map(EncoderConfig::from)
                .unwrap_or_default(),
            ffmpeg_args: enc.ffmpeg_args.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_returns_defaults() {
        let cfg = Config::load(Path::new("/nonexistent/never.json")).unwrap();
        assert!(cfg.encoders.av1.is_none());
    }

    #[test]
    fn empty_object_uses_all_defaults() {
        let v: Value = serde_json::from_str("{}").unwrap();
        let cfg = Config::from_value(&v);
        assert!(cfg.encoders.av1.is_none());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        // The frame-rate cap is not mirrored here any more (design doc
        // §3.10.2): it lives in the TOML config and reaches the run
        // through `EncodeParams::from_app_config`.
        let v: Value = serde_json::from_str(r#"{"max_fps": 60, "future_field": true}"#).unwrap();
        let cfg = Config::from_value(&v);
        assert!(cfg.encoders.av1.is_none());
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
}

pub mod app;
pub mod fs_perms;
pub mod path;

// Re-exports for ergonomics: callers can write `rencoder_lib::config::AppConfig`
// instead of `rencoder_lib::config::app::AppConfig`.
pub use app::{
    AnalyzeConfig, AnalyzeFormatArg, AppConfig, EncodeConfig, EncodersConfig, FormatArg,
};
