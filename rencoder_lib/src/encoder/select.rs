//! Encoder selection: maps a `VideoFormat` + `EncoderCache` to a concrete
//! encoder name, with config overrides and NVENC fallback.
//!
//! Selection priority (highest first):
//! 1. Config override (`[encode.encoders]`)
//! 2. NVENC variant -- only when this run asked for hardware encoding
//!    (`params.nv`) *and* the capability probe said this machine can
//!    actually run it
//! 3. Software default
//!
//! Each candidate is verified against `cache.encoders_output`. If a
//! preferred encoder isn't compiled into ffmpeg, or the NVENC probe
//! rejected it, the chain falls through to the next entry -- with a
//! warning, because a silent substitution (the user asked for hardware,
//! got software) is the worst outcome here. Errors only when no
//! candidate is reachable.
//!
//! The caller merges the configuration once per batch and passes
//! `&EncodeParams` here, avoiding a per-file file read.

use crate::encoder::nvenc::Capability;
use crate::error::EncodeError;
use crate::types::{EncodeParams, EncoderCache, VideoFormat};

/// Choose an encoder for `format`. Never spawns subprocesses -- consults
/// the pre-built cache, the supplied config and the run's capability
/// only.
///
/// `capability` is this machine's NVENC capability: the check's
/// conclusion plus what the run found since (an encoder that failed at
/// runtime is not tried again).
pub fn select_encoder(
    format: VideoFormat,
    cache: &EncoderCache,
    params: &EncodeParams,
    capability: &Capability,
) -> Result<String, EncodeError> {
    // 1. Config override: the user's explicit choice. If this ffmpeg
    //    build doesn't have it (or it failed at runtime earlier in this
    //    process), fall back -- but say so.
    if let Some(override_enc) = params.encoders.for_format(format) {
        if capability.verdicts().is_disabled(override_enc) {
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

    // 2. NVENC variant: only when this run asked for hardware encoding
    //    and the capability probe passed for it (and it has not failed
    //    at runtime). The two conditions are independent: `nv` is the
    //    run's choice, the probe is the machine's fact (design doc
    //    §3.10.5).
    if let Some(nvenc_enc) = nvenc_for(format).filter(|_| params.nv) {
        if !capability.any_usable() {
            // The overall verdict first: this machine cannot do
            // hardware encoding at all, so there is no point in asking
            // per encoder. The reason is the check's -- say it, so the
            // user learns why the run is in software.
            tracing::warn!(
                encoder = nvenc_enc,
                reason = capability
                    .reason(nvenc_enc)
                    .unwrap_or("no NVENC encoder is usable"),
                format = format_name(format),
                "this machine cannot do hardware encoding; falling back to software encoding"
            );
        } else if capability.verdicts().is_disabled(nvenc_enc) {
            tracing::warn!(
                encoder = nvenc_enc,
                format = format_name(format),
                "NVENC encoder failed earlier in this run; using software encoding"
            );
        } else if capability.usable(nvenc_enc) {
            return Ok(nvenc_enc.to_string());
        } else if let Some(reason) = capability.reason(nvenc_enc) {
            // Not usable. The reason says why: either this ffmpeg
            // build does not have the encoder at all, or the probe
            // failed on this machine. The wording does not pick one --
            // the reason carries it.
            tracing::warn!(
                encoder = nvenc_enc,
                reason = %reason,
                format = format_name(format),
                "NVENC encoder unavailable; falling back to software encoding"
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
    use crate::config::app::EncodersConfig;
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

    /// The ffmpeg build's encoder list.
    fn cache() -> EncoderCache {
        EncoderCache {
            encoders_output: ENCODERS_WITH_NVENC.to_string(),
        }
    }

    /// A capability whose NVENC encoders all passed the check.
    fn capability_with_usable_nvenc(usable: &[&str]) -> Capability {
        Capability::from_support(
            usable
                .iter()
                .map(|encoder| NvencSupport {
                    encoder: (*encoder).to_string(),
                    usable: true,
                    reason: None,
                })
                .collect(),
        )
    }

    /// A capability whose NVENC encoders all failed the check.
    fn capability_with_failed_nvenc(failed: &[&str]) -> Capability {
        Capability::from_support(
            failed
                .iter()
                .map(|encoder| NvencSupport {
                    encoder: (*encoder).to_string(),
                    usable: false,
                    reason: Some("Cannot load libcuda.so.1".to_string()),
                })
                .collect(),
        )
    }

    /// Params with hardware encoding off (the default).
    fn params() -> EncodeParams {
        EncodeParams::default()
    }

    /// Params that ask for hardware encoding.
    fn params_nv() -> EncodeParams {
        EncodeParams {
            nv: true,
            ..EncodeParams::default()
        }
    }

    /// Params with a per-format encoder override.
    fn params_with_override(encoder: &str) -> EncodeParams {
        EncodeParams {
            encoders: EncodersConfig {
                av1: Some(encoder.to_string()),
                ..Default::default()
            },
            ..EncodeParams::default()
        }
    }

    /// An empty capability: nothing is known to be unusable yet.
    fn capability() -> Capability {
        Capability::default()
    }

    #[test]
    fn select_software_defaults_without_nvidia() {
        let capability = capability_with_usable_nvenc(&[]);
        let cfg = params();
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache(), &cfg, &capability).unwrap(),
            "libsvtav1"
        );
        assert_eq!(
            select_encoder(VideoFormat::X264, &cache(), &cfg, &capability).unwrap(),
            "libx264"
        );
        assert_eq!(
            select_encoder(VideoFormat::Hevc, &cache(), &cfg, &capability).unwrap(),
            "libx265"
        );
        assert_eq!(
            select_encoder(VideoFormat::Vp9, &cache(), &cfg, &capability).unwrap(),
            "libvpx-vp9"
        );
    }

    #[test]
    fn select_nvenc_when_available() {
        let capability = capability_with_usable_nvenc(&["h264_nvenc", "hevc_nvenc", "av1_nvenc"]);
        let cfg = params_nv();
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache(), &cfg, &capability).unwrap(),
            "av1_nvenc"
        );
        assert_eq!(
            select_encoder(VideoFormat::X264, &cache(), &cfg, &capability).unwrap(),
            "h264_nvenc"
        );
        assert_eq!(
            select_encoder(VideoFormat::Hevc, &cache(), &cfg, &capability).unwrap(),
            "hevc_nvenc"
        );
        // VP9 has no NVENC variant; falls through to software.
        assert_eq!(
            select_encoder(VideoFormat::Vp9, &cache(), &cfg, &capability).unwrap(),
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
        };
        // The check never saw the encoder -> the overall verdict is
        // "this machine cannot do hardware encoding".
        let capability = capability_with_usable_nvenc(&[]);
        let cfg = params_nv();
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache, &cfg, &capability).unwrap(),
            "libsvtav1"
        );
    }

    #[test]
    fn nvenc_falls_back_to_software_when_probe_failed() {
        // The encoder IS compiled in, but the capability probe rejected
        // it (e.g. a GPU below the required compute capability).
        let capability = capability_with_failed_nvenc(&["av1_nvenc"]);
        let cfg = params_nv();
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache(), &cfg, &capability).unwrap(),
            "libsvtav1"
        );
    }

    #[test]
    fn nvenc_failure_reason_is_exposed_for_the_warning() {
        let capability = capability_with_failed_nvenc(&["av1_nvenc"]);
        assert_eq!(
            capability.reason("av1_nvenc"),
            Some("Cannot load libcuda.so.1")
        );
        assert!(!capability.usable("av1_nvenc"));
        assert!(capability.reason("hevc_nvenc").is_none());
        assert!(
            !capability.any_usable(),
            "no NVENC encoder is usable -> software encoding"
        );
    }

    #[test]
    fn no_encoder_available_errors() {
        let cache = EncoderCache {
            encoders_output: String::new(),
        };
        let cfg = params();
        let result = select_encoder(VideoFormat::Av1, &cache, &cfg, &capability());
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
        };
        let cfg = params_with_override("libaom-av1");
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache, &cfg, &capability()).unwrap(),
            "libaom-av1"
        );
    }

    #[test]
    fn config_override_falls_back_when_not_in_cache() {
        // Override specifies an encoder the build doesn't have.
        let capability = capability_with_usable_nvenc(&[]);
        let cfg = params_with_override("libdav1d");
        // libdav1d not in cache, so the chain falls through to default.
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache(), &cfg, &capability).unwrap(),
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
        };
        let capability = capability_with_usable_nvenc(&["av1_nvenc"]);
        let cfg = params_with_override("libaom-av1");
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache, &cfg, &capability).unwrap(),
            "libaom-av1"
        );
    }

    #[test]
    fn nvenc_is_not_chosen_when_the_run_did_not_ask_for_it() {
        // The machine can do it, but this run did not ask for hardware
        // encoding: the run's choice wins over the machine's ability
        // (design doc §3.10.5).
        let capability = capability_with_usable_nvenc(&["av1_nvenc"]);
        assert_eq!(
            select_encoder(VideoFormat::Av1, &cache(), &params(), &capability).unwrap(),
            "libsvtav1"
        );
    }
}
