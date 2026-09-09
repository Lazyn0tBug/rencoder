//! Project-level configuration file (TOML).
//!
//! Aimed at the user-facing `rencoder.toml` that lives at the project
//! root. CLI args override file values; file values override built-in
//! defaults. Replaces the per-invocation flags that the U17 CLI/config
//! split moved off the command line.
//!
//! Schema (illustrative):
//!
//! ```toml
//! [encode]
//! format = "av1"
//! crf = true
//! max_fps = 30
//! nv = true
//! extensions = ["mp4", "mov", "mkv"]
//! output_dir = "encoded"
//!
//! [encode.encoders]
//! av1 = "libsvtav1"
//!
//! [analyze]
//! format = "table"
//! parallel = true
//! output = "fps_stats.txt"
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::types::{MaxFpsConfig, VideoFormat};

/// Top-level user-facing config. Both sections are optional; missing
/// sections fall back to their per-action defaults.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    #[serde(default)]
    pub encode: Option<EncodeConfig>,
    #[serde(default)]
    pub analyze: Option<AnalyzeConfig>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EncodeConfig {
    pub format: Option<FormatArg>,
    pub bitrate: Option<u32>,
    pub crf: Option<bool>,
    /// Cap source FPS at this canonical value. Must be 0 (disabled)
    /// or one of the documented set (24, 25, 30, 60). Other values are
    /// rejected at config-load time.
    pub max_fps: Option<u32>,
    pub nv: Option<bool>,
    pub extensions: Option<Vec<String>>,
    pub output_dir: Option<PathBuf>,
    pub encoders: Option<EncodersConfig>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeConfig {
    pub format: Option<AnalyzeFormatArg>,
    pub parallel: Option<bool>,
    pub extensions: Option<Vec<String>>,
    pub output: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EncodersConfig {
    pub av1: Option<String>,
    pub x264: Option<String>,
    pub hevc: Option<String>,
    pub vp9: Option<String>,
}

impl ProjectConfig {
    /// Load a project config from a TOML file. Missing file → defaults.
    /// Malformed TOML or unknown keys → error.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate that all set values are sensible. Today this checks
    /// `max_fps` is in the canonical set; other fields are
    /// type-validated at parse time.
    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(enc) = &self.encode {
            if let Some(m) = enc.max_fps {
                if m != 0 && m != 24 && m != 25 && m != 30 && m != 60 {
                    return Err(ConfigError::InvalidValue {
                        field: "encode.max_fps".into(),
                        value: m.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Build a `MaxFpsConfig` from this project's encode config (if
    /// set) merged with the CLI override (if any).
    pub fn max_fps(&self, cli_override: Option<u32>) -> MaxFpsConfig {
        let m = cli_override
            .or_else(|| self.encode.as_ref()?.max_fps)
            .unwrap_or(0);
        MaxFpsConfig {
            max_fps: m as f64,
            eps: 0.05,
        }
    }

    /// Build the per-format encoder override map, merging CLI overrides
    /// on top of the project's encoder config. CLI takes precedence.
    pub fn encoders(
        &self,
        cli_overrides: &std::collections::HashMap<FormatArg, String>,
    ) -> std::collections::HashMap<FormatArg, String> {
        let mut out = std::collections::HashMap::new();
        if let Some(enc) = self.encode.as_ref().and_then(|e| e.encoders.as_ref()) {
            for (k, v) in [
                (FormatArg::Av1, enc.av1.clone()),
                (FormatArg::X264, enc.x264.clone()),
                (FormatArg::Hevc, enc.hevc.clone()),
                (FormatArg::Vp9, enc.vp9.clone()),
            ] {
                if let Some(v) = v {
                    out.insert(k, v);
                }
            }
        }
        for (k, v) in cli_overrides {
            out.insert(*k, v.clone());
        }
        out
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum FormatArg {
    X264,
    Hevc,
    Vp9,
    Av1,
}

impl From<FormatArg> for VideoFormat {
    fn from(f: FormatArg) -> Self {
        match f {
            FormatArg::X264 => VideoFormat::X264,
            FormatArg::Hevc => VideoFormat::Hevc,
            FormatArg::Vp9 => VideoFormat::Vp9,
            FormatArg::Av1 => VideoFormat::Av1,
        }
    }
}

/// Output format for the `analyze` subcommand. Mirrors the CLI's
/// `AnalyzeFormatArg` (defined in `rencoder_cli/src/args.rs`) but with
/// serde-derived `Deserialize` for TOML. The two stay in sync; a small
/// mapping in `main.rs` is the bridge.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum AnalyzeFormatArg {
    Csv,
    Table,
    Text,
    Json,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid value for {field}: {value}")]
    InvalidValue { field: String, value: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_returns_defaults() {
        let cfg = ProjectConfig::load(Path::new("/nonexistent/never.toml")).unwrap();
        assert_eq!(cfg, ProjectConfig::default());
    }

    #[test]
    fn parses_minimal_toml() {
        let dir = std::env::temp_dir();
        let path = dir.join("rencoder_minimal.toml");
        std::fs::write(&path, "[encode]\nformat = \"av1\"\n").unwrap();
        let cfg = ProjectConfig::load(&path).unwrap();
        assert_eq!(
            cfg.encode.as_ref().and_then(|e| e.format),
            Some(FormatArg::Av1)
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn invalid_max_fps_rejected() {
        let dir = std::env::temp_dir();
        let path = dir.join("rencoder_bad_fps.toml");
        std::fs::write(&path, "[encode]\nmax_fps = 15\n").unwrap();
        let err = ProjectConfig::load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn max_fps_merges_cli_over_default() {
        let cfg = ProjectConfig::default();
        let m = cfg.max_fps(Some(60));
        assert_eq!(m.max_fps as u32, 60);
    }

    #[test]
    fn max_fps_uses_config_value() {
        let cfg = ProjectConfig {
            encode: Some(EncodeConfig {
                max_fps: Some(30),
                ..Default::default()
            }),
            ..Default::default()
        };
        let m = cfg.max_fps(None);
        assert_eq!(m.max_fps as u32, 30);
    }

    #[test]
    fn unknown_keys_rejected() {
        let dir = std::env::temp_dir();
        let path = dir.join("rencoder_unknown.toml");
        std::fs::write(&path, "[encode]\nthis_is_a_typo = true\n").unwrap();
        assert!(ProjectConfig::load(&path).is_err());
        std::fs::remove_file(&path).ok();
    }
}
