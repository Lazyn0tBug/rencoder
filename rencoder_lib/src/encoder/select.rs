//! Encoder selection: maps a `VideoFormat` + `EncoderCache` to a concrete
//! encoder name, with config overrides and NVENC fallback.
//!
//! Selection priority (highest first):
//! 1. Config override (from the supplied `Config`)
//! 2. NVENC variant -- only when the capability probe said this machine
//!    can actually run it
//! 3. Software default
//!
//! Each candidate is verified against `cache.encoders_output`. If a
//! preferred encoder isn't compiled into ffmpeg, or the NVENC probe
//! rejected it, the chain falls through to the next entry -- with a
//! warning, because a silent substitution (the user asked for hardware,
//! got software) is the worst outcome here. Errors only when no
//! candidate is reachable.
//!
//! The caller loads `Config` once per batch and passes `&Config` here,
//! avoiding a per-file file read.

use crate::config::Config;
use crate::encoder::nvenc;
use crate::error::EncodeError;
use crate::types::{EncoderCache, VideoFormat};

/// Choose an encoder for `format`. Never spawns subprocesses -- consults
/// the pre-built cache and the supplied config only.
pub fn select_encoder(
    format: VideoFormat,
    cache: &EncoderCache,
    config: &Config,
) -> Result<String, EncodeError> {
    // 1. Config override: the user's explicit choice. If this ffmpeg
    //    build doesn't have it (or it failed at runtime earlier in this
    //    process), fall back -- but say so.
    if let Some(override_enc) = config.encoders.for_format(format) {
        if nvenc::is_disabled(override_enc) {
            tracing::warn!(
                encoder = override_enc,
                format = format_name(format),
                "configured encoder failed earlier in this run; falling back"
            );
        } else if cache.has_encoder(override_enc) {
            return Ok(override_enc.to_string());
        } else {
            tracing::warn!(
                encoder = override_enc,
                format = format_name(format),
                "configured encoder is not available in this ffmpeg build; falling back"
            );
        }
    }

    // 2. NVENC variant: only when the capability probe passed for it
    //    and it has not failed at runtime.
    if let Some(nvenc_enc) = nvenc_for(format) {
        if nvenc::is_disabled(nvenc_enc) {
            tracing::warn!(
                encoder = nvenc_enc,
                format = format_name(format),
                "NVENC encoder failed earlier in this run; using software encoding"
            );
        } else if cache.nvenc_usable(nvenc_enc) {
            return Ok(nvenc_enc.to_string());
        } else if let Some(reason) = cache.nvenc_failure_reason(nvenc_enc) {
            // Not usable: either the probe rejected it (we know why) or
            // NVENC is disabled entirely (nothing to report).
            tracing::warn!(
                encoder = nvenc_enc,
                reason = %reason,
                format = format_name(format),
                "NVENC encoder unavailable on this machine; falling back to software encoding"
            );
        }
    }

    // 3. Software default.
    let default = default_for(format);
    if cache.has_encoder(default) {
        return Ok(default.to_string());
    }

    Err(EncodeError::EncoderNotFound(format!(
        "no {} encoder available in this ffmpeg build",
        format_name(format)
    )))
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
    use crate::config::EncoderConfig;
    use crate::types::NvencSupport;

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

    /// Cache whose NVENC encoders all passed the capability probe.
    fn cache_with_usable_nvenc(usable: &[&str]) -> EncoderCache {
        EncoderCache {
            encoders_output: ENCODERS_WITH_NVENC.to_string(),
            nvenc: usable
                .iter()
                .map(|encoder| NvencSupport {
                    encoder: (*encoder).to_string(),
                    usable: true,
                    reason: None,
                })
                .collect(),
        }
    }

    /// Cache whose NVENC encoders all failed the capability probe.
    fn cache_with_failed_nvenc(usable: &[&str]) -> EncoderCache {
        EncoderCache {
            encoders_output: ENCODERS_WITH_NVENC.to_string(),
            nvenc: usable
                .iter()
                .map(|encoder| NvencSupport {
                    encoder: (*encoder).to_string(),
                    usable: false,
                    reason: Some("Cannot load libcuda.so.1".to_string()),
                })
                .collect(),
        }
    }

    fn defaults() -> Config {
        Config::default()
    }

    #[test]
    fn select_software_defaults_without_nvidia() {
        let cache = cache_with_usable_nvenc(&[]);
        let cfg = defaults();
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache, &cfg).unwrap(),
            "libsvtav1"
        );
        assert_eq!(
            select_encoder(VideoFormat::X264, &cache, &cfg).unwrap(),
            "libx264"
        );
        assert_eq!(
            select_encoder(VideoFormat::Hevc, &cache, &cfg).unwrap(),
            "libx265"
        );
        assert_eq!(
            select_encoder(VideoFormat::Vp9, &cache, &cfg).unwrap(),
            "libvpx-vp9"
        );
    }

    #[test]
    fn select_nvenc_when_available() {
        let cache = cache_with_usable_nvenc(&["h264_nvenc", "hevc_nvenc", "av1_nvenc"]);
        let cfg = defaults();
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache, &cfg).unwrap(),
            "av1_nvenc"
        );
        assert_eq!(
            select_encoder(VideoFormat::X264, &cache, &cfg).unwrap(),
            "h264_nvenc"
        );
        assert_eq!(
            select_encoder(VideoFormat::Hevc, &cache, &cfg).unwrap(),
            "hevc_nvenc"
        );
        // VP9 has no NVENC variant; falls through to software.
        assert_eq!(
            select_encoder(VideoFormat::Vp9, &cache, &cfg).unwrap(),
            "libvpx-vp9"
        );
    }

    #[test]
    fn nvenc_falls_back_to_software_when_not_compiled() {
        // The ffmpeg build has no av1_nvenc encoder at all.
        let stripped = ENCODERS_WITH_NVENC.replace(
            " V..... av1_nvenc            NVIDIA NVENC AV1 encoder\n",
            "",
        );
        let cache = EncoderCache {
            encoders_output: stripped,
            nvenc: Vec::new(),
        };
        let cfg = defaults();
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache, &cfg).unwrap(),
            "libsvtav1"
        );
    }

    #[test]
    fn nvenc_falls_back_to_software_when_probe_failed() {
        // The encoder IS compiled in, but the capability probe rejected
        // it (e.g. a GPU below the required compute capability).
        let cache = cache_with_failed_nvenc(&["av1_nvenc"]);
        let cfg = defaults();
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache, &cfg).unwrap(),
            "libsvtav1"
        );
    }

    #[test]
    fn nvenc_failure_reason_is_exposed_for_the_warning() {
        let cache = cache_with_failed_nvenc(&["av1_nvenc"]);
        assert_eq!(
            cache.nvenc_failure_reason("av1_nvenc"),
            Some("Cannot load libcuda.so.1")
        );
        assert!(!cache.nvenc_usable("av1_nvenc"));
        assert!(cache.nvenc_failure_reason("hevc_nvenc").is_none());
    }

    #[test]
    fn no_encoder_available_errors() {
        let cache = EncoderCache {
            encoders_output: String::new(),
            nvenc: Vec::new(),
        };
        let cfg = defaults();
        let result = select_encoder(VideoFormat::Av1, &cache, &cfg);
        assert!(matches!(result, Err(EncodeError::EncoderNotFound(_))));
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
            nvenc: Vec::new(),
        };
        let cfg = Config {
            encoders: EncoderConfig {
                av1: Some("libaom-av1".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache, &cfg).unwrap(),
            "libaom-av1"
        );
    }

    #[test]
    fn config_override_falls_back_when_not_in_cache() {
        // Override specifies an encoder the build doesn't have.
        let cache = cache_with_usable_nvenc(&[]);
        let cfg = Config {
            encoders: EncoderConfig {
                av1: Some("libdav1d".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        // libdav1d not in cache, so the chain falls through to default.
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache, &cfg).unwrap(),
            "libsvtav1"
        );
    }

    #[test]
    fn config_override_preferred_over_nvenc() {
        let cache = EncoderCache {
            encoders_output: "\
Encoders:
 V..... libsvtav1            libsvtav1 SVT-AV1
 V..... av1_nvenc            NVIDIA NVENC AV1 encoder
 V..... libaom-av1           libaom AOM AV1
"
            .to_string(),
            nvenc: vec![NvencSupport {
                encoder: "av1_nvenc".to_string(),
                usable: true,
                reason: None,
            }],
        };
        let cfg = Config {
            encoders: EncoderConfig {
                av1: Some("libaom-av1".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache, &cfg).unwrap(),
            "libaom-av1"
        );
    }
}
