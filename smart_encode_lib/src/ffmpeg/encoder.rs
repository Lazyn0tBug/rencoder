//! Encoder selection: maps a `VideoFormat` + `EncoderCache` to a concrete
//! encoder name, with optional config-file overrides and NVENC fallback.
//!
//! Selection priority (highest first):
//! 1. Config override from `--config` (if `config_path` is `Some`)
//! 2. NVENC variant (when `cache.has_nvidia` is true and the format has one)
//! 3. Software default
//!
//! Each candidate is verified against `cache.encoders_output`. If a
//! preferred encoder isn't compiled into ffmpeg, the chain falls through
//! to the next entry. Errors only when no candidate is reachable.

use std::path::Path;

use crate::config::Config;
use crate::error::EncodeError;
use crate::types::{EncoderCache, VideoFormat};

/// Choose an encoder for `format`. Never spawns subprocesses -- consults
/// the pre-built cache and (optionally) the config file only.
pub fn select_encoder(
    format: VideoFormat,
    cache: &EncoderCache,
    config_path: Option<&Path>,
) -> Result<String, EncodeError> {
    let config = match config_path {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };

    let preference = preference_for(format, cache.has_nvidia, &config);

    for encoder in preference {
        if cache.has_encoder(&encoder) {
            return Ok(encoder);
        }
    }

    Err(EncodeError::EncoderNotFound(format!(
        "no {} encoder available in this ffmpeg build",
        format_name(format)
    )))
}

/// Build the preference chain for a format. Returns candidates in
/// priority order; the first one present in `cache.encoders_output` wins.
fn preference_for(format: VideoFormat, has_nvidia: bool, config: &Config) -> Vec<String> {
    let mut chain: Vec<String> = Vec::with_capacity(3);

    if let Some(override_enc) = config.encoders.for_format(format) {
        chain.push(override_enc.to_string());
    }
    if has_nvidia {
        if let Some(nvenc) = nvenc_for(format) {
            chain.push(nvenc.to_string());
        }
    }
    chain.push(default_for(format).to_string());
    chain
}

fn default_for(format: VideoFormat) -> &'static str {
    match format {
        VideoFormat::Av1 => "libsvtav1",
        VideoFormat::X264 => "libx264",
        VideoFormat::Hevc => "libx265",
        VideoFormat::Vp9 => "libvpx-vp9",
    }
}

fn nvenc_for(format: VideoFormat) -> Option<&'static str> {
    match format {
        VideoFormat::Av1 => Some("av1_nvenc"),
        VideoFormat::X264 => Some("h264_nvenc"),
        VideoFormat::Hevc => Some("hevc_nvenc"),
        VideoFormat::Vp9 => None, // No NVENC variant in our default mapping.
    }
}

fn format_name(format: VideoFormat) -> &'static str {
    match format {
        VideoFormat::Av1 => "AV1",
        VideoFormat::X264 => "H.264",
        VideoFormat::Hevc => "HEVC",
        VideoFormat::Vp9 => "VP9",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENCODERS_WITH_NVENC: &str = "\
Encoders:
 V..... libx264              libx264 H.264 / AVC
 V..... libx265              libx265 H.265 / HEVC
 V..... libsvtav1            libsvtav1 SVT-AV1
 V..... libvpx-vp9           libvpx-vp9 VP9
 V..... h264_nvenc           NVIDIA NVENC H.264 encoder
 V..... hevc_nvenc           NVIDIA NVENC HEVC encoder
 V..... av1_nvenc            NVIDIA NVENC AV1 encoder
";

    fn cache_with(has_nvidia: bool) -> EncoderCache {
        EncoderCache {
            encoders_output: ENCODERS_WITH_NVENC.to_string(),
            has_nvidia,
        }
    }

    #[test]
    fn select_software_defaults_without_nvidia() {
        let cache = cache_with(false);
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache, None).unwrap(),
            "libsvtav1"
        );
        assert_eq!(
            select_encoder(VideoFormat::X264, &cache, None).unwrap(),
            "libx264"
        );
        assert_eq!(
            select_encoder(VideoFormat::Hevc, &cache, None).unwrap(),
            "libx265"
        );
        assert_eq!(
            select_encoder(VideoFormat::Vp9, &cache, None).unwrap(),
            "libvpx-vp9"
        );
    }

    #[test]
    fn select_nvenc_when_available() {
        let cache = cache_with(true);
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache, None).unwrap(),
            "av1_nvenc"
        );
        assert_eq!(
            select_encoder(VideoFormat::X264, &cache, None).unwrap(),
            "h264_nvenc"
        );
        assert_eq!(
            select_encoder(VideoFormat::Hevc, &cache, None).unwrap(),
            "hevc_nvenc"
        );
        // VP9 has no NVENC variant; falls through to software.
        assert_eq!(
            select_encoder(VideoFormat::Vp9, &cache, None).unwrap(),
            "libvpx-vp9"
        );
    }

    #[test]
    fn nvenc_falls_back_to_software_when_not_compiled() {
        // has_nvidia = true but ffmpeg has no nvenc encoders for AV1.
        let stripped = ENCODERS_WITH_NVENC.replace(
            " V..... av1_nvenc            NVIDIA NVENC AV1 encoder\n",
            "",
        );
        let cache = EncoderCache {
            encoders_output: stripped,
            has_nvidia: true,
        };
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache, None).unwrap(),
            "libsvtav1"
        );
    }

    #[test]
    fn no_encoder_available_errors() {
        let cache = EncoderCache {
            encoders_output: String::new(),
            has_nvidia: false,
        };
        let result = select_encoder(VideoFormat::Av1, &cache, None);
        assert!(matches!(result, Err(EncodeError::EncoderNotFound(_))));
    }

    #[test]
    fn missing_config_path_uses_defaults() {
        let cache = cache_with(false);
        let result =
            select_encoder(VideoFormat::Av1, &cache, Some(Path::new("/nonexistent/cfg.json")))
                .unwrap();
        assert_eq!(result, "libsvtav1");
    }

    #[test]
    fn config_override_used_when_in_cache() {
        let cache = EncoderCache {
            encoders_output: "\
Encoders:
 V..... libaom-av1           libaom AOM AV1
 V..... libsvtav1            libsvtav1 SVT-AV1
"
            .to_string(),
            has_nvidia: false,
        };

        let dir = std::env::temp_dir().join("rencoder_test_encoder_override");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cfg.json");
        std::fs::write(&path, r#"{"encoders": {"av1": "libaom-av1"}}"#).unwrap();

        let result = select_encoder(VideoFormat::Av1, &cache, Some(&path)).unwrap();
        assert_eq!(result, "libaom-av1");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn config_override_falls_back_when_not_in_cache() {
        // Override specifies an encoder the build doesn't have.
        let cache = EncoderCache {
            encoders_output: ENCODERS_WITH_NVENC.to_string(),
            has_nvidia: false,
        };

        let dir = std::env::temp_dir().join("rencoder_test_encoder_fallback");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cfg.json");
        std::fs::write(&path, r#"{"encoders": {"av1": "libdav1d"}}"#).unwrap();

        // libdav1d not in cache, so chain falls through to default.
        let result = select_encoder(VideoFormat::Av1, &cache, Some(&path)).unwrap();
        assert_eq!(result, "libsvtav1");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn config_override_preferred_over_nvenc() {
        // Override should beat NVENC even when both are available.
        let cache = EncoderCache {
            encoders_output: "\
Encoders:
 V..... libsvtav1            libsvtav1 SVT-AV1
 V..... av1_nvenc            NVIDIA NVENC AV1 encoder
 V..... libaom-av1           libaom AOM AV1
"
            .to_string(),
            has_nvidia: true,
        };

        let dir = std::env::temp_dir().join("rencoder_test_override_beats_nvenc");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cfg.json");
        std::fs::write(&path, r#"{"encoders": {"av1": "libaom-av1"}}"#).unwrap();

        let result = select_encoder(VideoFormat::Av1, &cache, Some(&path)).unwrap();
        assert_eq!(result, "libaom-av1");

        std::fs::remove_file(&path).ok();
    }
}
