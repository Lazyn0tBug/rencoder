//! App-level configuration file (TOML) — U18: lives at the app-level
//! location (binary's parent dir; `$RENENCODER_CONFIG` overrides).
//!
//! CLI args override file values; file values override built-in defaults.
//! Replaces the per-invocation flags that the U17 CLI/config split moved
//! off the command line.
//!
//! **Auto-create**: when the resolved path doesn't exist,
//! [`AppConfig::load`] writes [`DEFAULT_CONFIG_TOML`] (atomic
//! tmp+rename; mode bits `0o644` on Unix), stderr-notices, then returns
//! the parsed config. Existing-but-invalid files are **never** touched.
//!
//! Schema (illustrative):
//!
//! ```toml
//! [encode]
//! format = "av1"
//! crf = 45   # 0 = bitrate mode; default 45 (libsvtav1 medium)
//! max_fps = 30
//! nv = false
//! extensions = ["mp4", "mov", "avi"]
//! output_dir = "encoded"
//!
//! [encode.encoders]
//! av1 = "libsvtav1"
//!
//! [analyze]
//! format = "table"
//! parallel = true
//! output = "analyze.csv"
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::fs_perms;
use crate::types::VideoFormat;

/// Canonical auto-generated config content. Used when the resolved
/// config path doesn't exist on first run. This is a starter template
/// (not the literal `Default::default()` output) — it includes inline
/// documentation of every field plus recommended starting values
/// (e.g., `bitrate = 5000`, `extensions = ["mp4", "mov", "avi"]`).
///
/// When schema fields change (e.g., U18 step 6's `crf` u32 redesign),
/// this string is updated in lockstep with `EncodeConfig` /
/// `AnalyzeConfig` definitions. The `AppConfig::load` round-trip
/// test (`missing_file_auto_creates_and_returns_parsed_config`) catches
/// drift between this constant and the schema.
pub const DEFAULT_CONFIG_TOML: &str = r#"# Rencoder user config (TOML).
#
# Auto-generated on first run because no config file was found at the
# app-level location (binary dir; or $RENENCODER_CONFIG if set). Edit
# values below to customize behavior; the file is reloaded on each
# invocation.
#
# CLI flags (-i, -o, --config, passthrough args) override values here.
# Unknown keys are rejected so typos surface immediately.

[encode]
# Target format. One of: x264 | hevc | vp9 | av1. Default: av1.
format = "av1"

# Bitrate in Kbps (used when crf = 0). Ignored in CRF mode.
# Default: 5000.
bitrate = 5000

# CRF quality value (libsvtav1 scale, 0..=63). Aligns 1:1 with ffmpeg's
# `-crf N`. Sentinel `0` switches to bitrate mode (uses `bitrate` field).
# Default: 45 (libsvtav1 medium quality).
crf = 45

# Cap source FPS at canonical values (0, 24, 25, 30, 60). 0 disables.
# Default: 0.
max_fps = 30

# Enable NVIDIA hardware acceleration via NVENC. Falls back silently
# to software encoding when nvidia-smi is missing or no GPU is detected.
# Default: false.
nv = false

# Extension filter for `encode -d DIR`. Empty list = all files.
extensions = ["mp4", "mov", "avi"]

# Optional: directory where encoded outputs land. Not wired yet (output
# paths are computed per-input); reserved for a future stage where
# batch output lands in a single project directory.
# output_dir = "encoded"

[encode.encoders]
# Per-format encoder overrides. Uncomment to use a different encoder
# than the built-in default (libsvtav1, libx264, libx265, libvpx-vp9).
# av1 = "libsvtav1"
# x264 = "libx264"
# hevc = "libx265"
# vp9 = "libvpx-vp9"

[analyze]
# Output format. One of: csv | table | text | json. Default: csv.
format = "csv"

# Parallel analysis via rayon. Default: false.
parallel = true

# Optional: write the analyze output to this path instead of stdout.
# The CLI `-o` flag overrides this per-run; `-` means stdout (default).
# output = "analyze.csv"
"#;

/// Top-level user-facing config. Both sections are optional; missing
/// sections fall back to their per-action defaults.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default)]
    pub encode: Option<EncodeConfig>,
    #[serde(default)]
    pub analyze: Option<AnalyzeConfig>,
}

/// Default CRF value (libsvtav1 medium quality). Sentinel `0` switches
/// to bitrate mode at the ffmpeg CLI layer; this default keeps the
/// quality mode unless the user explicitly opts out.
fn default_crf() -> u32 {
    45
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EncodeConfig {
    pub format: Option<FormatArg>,
    pub bitrate: Option<u32>,
    /// CRF quality value (libsvtav1 scale, 0..=63).
    ///
    /// `0` is the sentinel for bitrate mode (uses `bitrate` field).
    /// `>0` is the CRF quality (aligns 1:1 with ffmpeg's `-crf N`).
    ///
    /// Default: 45 (libsvtav1 medium quality).
    #[serde(default = "default_crf")]
    pub crf: u32,
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

impl AppConfig {
    /// Load a project config from a TOML file.
    ///
    /// Behavior:
    /// - Path doesn't exist → auto-create with [`DEFAULT_CONFIG_TOML`]
    ///   (atomic tmp+rename; mode bits `0o644` on Unix), stderr notice,
    ///   then return the parsed config.
    /// - Path exists → read + parse + validate.
    /// - Parse / validate fails → `Err`. **Existing files are never
    ///   overwritten** — only a missing file triggers auto-create.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            let content = DEFAULT_CONFIG_TOML;
            write_atomic(path, content.as_bytes())?;
            // Surface auto-create via the project's log infrastructure
            // (design doc §2.3: lib uses `log`, CLI bridges via
            // `tracing-log`). User can silence via RUST_LOG.
            log::info!(
                "rencoder: created {} (no config found; edit to customize)",
                path.display()
            );
            // Re-parse the constant as a self-consistency check: catches
            // drift between DEFAULT_CONFIG_TOML and the schema. If the
            // round-trip fails, the build/test fails loudly.
            let cfg: Self = toml::from_str(content)?;
            cfg.validate()?;
            return Ok(cfg);
        }
        let raw = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate that all set values are sensible. Today this checks
    /// `max_fps` is in the canonical set, `bitrate` is non-zero when
    /// set, and `crf` is within libsvtav1's documented 0..=63 range;
    /// other fields are type-validated at parse time.
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
            if let Some(b) = enc.bitrate {
                if b == 0 {
                    return Err(ConfigError::InvalidValue {
                        field: "encode.bitrate".into(),
                        value: "0".into(),
                    });
                }
            }
            if enc.crf > 63 {
                return Err(ConfigError::InvalidValue {
                    field: "encode.crf".into(),
                    value: enc.crf.to_string(),
                });
            }
        }
        Ok(())
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

/// Atomic write: tmp file → set perms → rename to target. Avoids
/// half-written files if the process is killed mid-write — the target
/// is either fully-present or fully-absent (assuming `rename` is
/// atomic on the platform, which std guarantees on Unix and on
/// Windows via `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`).
///
/// On any error during the write sequence (File::create, write_all,
/// sync_all, fs_perms::set_config_file_mode, rename), the tmp file is
/// cleaned up so it doesn't leak. A successful `rename` atomically
/// removes the tmp, so cleanup is only needed on the error path.
fn write_atomic(path: &Path, content: &[u8]) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = tmp_path_for(path);
    let result: Result<(), ConfigError> = (|| {
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(content)?;
            f.sync_all()?;
        }
        fs_perms::set_config_file_mode(&tmp)?;
        std::fs::rename(&tmp, path).map_err(ConfigError::Io)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Tmp path: `<target>.tmp` in the same directory. Falls back to a
/// stable sibling if `path` has no usable file_name (shouldn't happen
/// for normal config paths, but defensive).
fn tmp_path_for(path: &Path) -> PathBuf {
    let mut tmp = path.to_path_buf();
    let name = match path.file_name() {
        Some(n) => format!("{}.tmp", n.to_string_lossy()),
        None => "rencoder.tmp".to_string(),
    };
    tmp.set_file_name(name);
    tmp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("rencoder_test_{label}_{}", std::process::id()))
    }

    #[test]
    fn missing_file_auto_creates_and_returns_parsed_config() {
        let dir = unique_temp_dir("auto_create");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rencoder.toml");
        assert!(!path.exists(), "precondition: file must not exist");

        let cfg = AppConfig::load(&path).expect("auto-create load should succeed");
        assert!(path.exists(), "auto-create must write the file");

        // Re-read the file and verify it's parseable + valid.
        let raw = std::fs::read_to_string(&path).unwrap();
        let re_parsed: AppConfig = toml::from_str(&raw).unwrap();
        re_parsed.validate().expect("re-parse must validate");

        // The auto-generated content has explicit values (bitrate=5000
        // etc.), so the parsed config is NOT literal Self::default().
        // But the encode and analyze sections should both be present.
        assert!(re_parsed.encode.is_some());
        assert!(re_parsed.analyze.is_some());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);

        // Suppress unused-variable warning on `cfg`.
        let _ = cfg;
    }

    #[test]
    fn existing_invalid_file_is_not_overwritten() {
        let dir = unique_temp_dir("no_overwrite");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rencoder.toml");
        let original_content = "[encode]\nthis_is_a_typo = true\n";
        std::fs::write(&path, original_content).unwrap();

        let result = AppConfig::load(&path);
        assert!(result.is_err(), "should reject unknown keys");

        // Existing file MUST NOT be overwritten — only missing triggers
        // auto-create. This is the critical safety property of U18.
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after, original_content,
            "invalid existing file should not be touched"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn existing_valid_file_is_used_as_is() {
        let dir = unique_temp_dir("existing");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rencoder.toml");
        let custom_content = "[encode]\nformat = \"hevc\"\nmax_fps = 30\n";
        std::fs::write(&path, custom_content).unwrap();

        let cfg = AppConfig::load(&path).unwrap();
        let enc = cfg.encode.as_ref().expect("encode section present");
        assert_eq!(enc.format, Some(FormatArg::Hevc));
        assert_eq!(enc.max_fps, Some(30));

        // File content unchanged
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, custom_content);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn parses_minimal_toml() {
        let dir = std::env::temp_dir();
        let path = dir.join("rencoder_minimal.toml");
        std::fs::write(&path, "[encode]\nformat = \"av1\"\n").unwrap();
        let cfg = AppConfig::load(&path).unwrap();
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
        let err = AppConfig::load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn zero_bitrate_rejected() {
        let dir = std::env::temp_dir();
        let path = dir.join("rencoder_zero_bitrate.toml");
        std::fs::write(&path, "[encode]\nbitrate = 0\n").unwrap();
        let err = AppConfig::load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unknown_keys_rejected() {
        let dir = std::env::temp_dir();
        let path = dir.join("rencoder_unknown.toml");
        std::fs::write(&path, "[encode]\nthis_is_a_typo = true\n").unwrap();
        assert!(AppConfig::load(&path).is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn crf_defaults_to_45_when_field_missing() {
        // U18 step 6: `crf: u32` with serde default = 45 (libsvtav1 medium).
        // A config that omits `crf` entirely must still load — the field
        // is non-Option with a default, so deserialization fills in 45.
        let dir = std::env::temp_dir();
        let path = dir.join("rencoder_crf_default.toml");
        std::fs::write(&path, "[encode]\nformat = \"av1\"\n").unwrap();
        let cfg = AppConfig::load(&path).expect("load should succeed");
        let enc = cfg.encode.as_ref().expect("encode section");
        assert_eq!(enc.crf, 45, "default crf must be 45");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn crf_explicit_value_accepted() {
        let dir = std::env::temp_dir();
        let path = dir.join("rencoder_crf_explicit.toml");
        std::fs::write(&path, "[encode]\ncrf = 25\n").unwrap();
        let cfg = AppConfig::load(&path).expect("load should succeed");
        assert_eq!(cfg.encode.unwrap().crf, 25);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn crf_zero_accepted_as_bitrate_sentinel() {
        // crf = 0 is the sentinel for bitrate mode (uses the `bitrate` field).
        // Validation must allow it; the runtime semantics are CLI-side.
        let dir = std::env::temp_dir();
        let path = dir.join("rencoder_crf_zero.toml");
        std::fs::write(&path, "[encode]\ncrf = 0\nbitrate = 8000\n").unwrap();
        let cfg = AppConfig::load(&path).expect("load should succeed");
        assert_eq!(cfg.encode.unwrap().crf, 0);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn crf_out_of_range_rejected() {
        // libsvtav1 max is 63; values above are silently accepted by ffmpeg
        // but produce undefined behavior, so we reject them at load time.
        let dir = std::env::temp_dir();
        let path = dir.join("rencoder_crf_oor.toml");
        std::fs::write(&path, "[encode]\ncrf = 64\n").unwrap();
        let err = AppConfig::load(&path).unwrap_err();
        match err {
            ConfigError::InvalidValue { field, value } => {
                assert_eq!(field, "encode.crf");
                assert_eq!(value, "64");
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
        std::fs::remove_file(&path).ok();
    }
}
