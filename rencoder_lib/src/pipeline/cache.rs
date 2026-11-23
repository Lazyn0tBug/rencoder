//! The `EncoderCache`: the ffmpeg build's encoder list.
//!
//! One cache per **published fact** (design doc §3.10.6 step 5): the
//! publisher builds it from the resolution and hands it to every batch
//! together with the capability. The `ffmpeg -encoders` output never
//! changes during a run, so cloning the (potentially multi-MB) string
//! per file would be wasteful.
//!
//! NVENC detection is a **capability probe**, not an existence check:
//! `ffmpeg -encoders` says the encoder was compiled in, `nvidia-smi -L`
//! says a GPU exists -- neither says this machine can actually run
//! `av1_nvenc` (compute capability >= 8.0, matching driver, free NVENC
//! session). The probe answers that with a 1-frame test encode -- and
//! its answer is a **capability**, not a cache entry (design doc
//! §3.10.5).
//!
//! The probe result is a property of the machine, not of a batch: it is
//! kept on disk by [`crate::encoder::nvenc`] and reused across
//! processes. This module owns the "build a cache from a resolved
//! capability" part.

use crate::encoder::nvenc;
use crate::types::EncoderCache;

/// Build an `EncoderCache` from an already-resolved NVENC capability.
///
/// No subprocess, no probe: the caller already knows which encoders
/// this machine can run.
pub fn cache_from_resolution(resolution: &nvenc::Resolution) -> EncoderCache {
    EncoderCache {
        encoders_output: resolution.encoders_output.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EncoderCache;

    const ENCODERS_BASIC: &str = "\
Encoders:
 V..... libx264              libx264 H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10 (codec h264)
 V..... libx265              libx265 H.265 / HEVC (codec hevc)
 V..... libsvtav1            libsvtav1 SVT-AV1 (codec av1)
 V..... libvpx-vp9           libvpx-vp9 VP9 (codec vp9)
";

    const ENCODERS_WITH_NVENC: &str = "\
Encoders:
 V..... libx264              libx264 H.264 / AVC
 V..... libx265              libx265 H.265 / HEVC
 V..... libsvtav1            libsvtav1 SVT-AV1
 V..... libvpx-vp9           libvpx-vp9 VP9
 V..... h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)
 V..... hevc_nvenc           NVIDIA NVENC HEVC encoder (codec hevc)
 V..... av1_nvenc            NVIDIA NVENC AV1 encoder (codec av1)
";

    #[test]
    fn has_encoder_substring_match() {
        let cache = EncoderCache {
            encoders_output: ENCODERS_BASIC.to_string(),
        };
        assert!(cache.has_encoder("libsvtav1"));
        assert!(cache.has_encoder("libx264"));
        assert!(!cache.has_encoder("libdav1d"));
        assert!(!cache.has_encoder("h264_nvenc"));
    }

    #[test]
    fn has_encoder_finds_nvenc_in_full_list() {
        let cache = EncoderCache {
            encoders_output: ENCODERS_WITH_NVENC.to_string(),
        };
        assert!(cache.has_encoder("av1_nvenc"));
        assert!(cache.has_encoder("h264_nvenc"));
        assert!(cache.has_encoder("hevc_nvenc"));
        assert!(cache.has_encoder("libsvtav1"));
    }

    #[test]
    fn cache_from_resolution_carries_the_support_list() {
        let resolution = nvenc::Resolution {
            encoders_output: ENCODERS_WITH_NVENC.to_string(),
            support: vec![crate::types::NvencSupport {
                encoder: "av1_nvenc".to_string(),
                usable: true,
                reason: None,
            }],
            record: nvenc::CapabilityRecord {
                version: 1,
                ffmpeg_version: "ffmpeg version 9.0.1".to_string(),
                ffmpeg_fingerprint: "abc".to_string(),
                probed_at: chrono::Utc::now(),
                next_review_at: chrono::Utc::now(),
                encoders: Vec::new(),
            },
            probed: false,
            review_due: false,
        };
        let cache = cache_from_resolution(&resolution);
        assert!(cache.has_encoder("libsvtav1"));
        // The NVENC conclusion is not in the cache any more -- it is
        // the capability (design doc §3.10.5).
        let capability = resolution.capability();
        assert!(capability.usable("av1_nvenc"));
        assert!(!capability.usable("hevc_nvenc"));
    }

    // The publisher writes the record; the probe does not (design doc
    // §3.10.6 step 5): an older probe's result must not overwrite a
    // newer record on disk.
    #[test]
    fn the_publisher_writes_the_record_where_the_caller_says() {
        // The record path is a parameter: the test points it at a
        // temporary directory, so the test never writes the
        // developer's real capability record.
        let dir = std::env::temp_dir().join(format!(
            "rencoder_cache_store_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp dir must be created");
        let record_path = dir.join("nvenc-capability.json");

        let resolution = nvenc::Resolution {
            encoders_output: ENCODERS_WITH_NVENC.to_string(),
            support: vec![crate::types::NvencSupport {
                encoder: "av1_nvenc".to_string(),
                usable: true,
                reason: None,
            }],
            record: nvenc::CapabilityRecord {
                version: nvenc::RECORD_VERSION,
                ffmpeg_version: "ffmpeg version 9.0.1".to_string(),
                ffmpeg_fingerprint: "abc".to_string(),
                probed_at: chrono::Utc::now(),
                next_review_at: chrono::Utc::now(),
                encoders: Vec::new(),
            },
            probed: true,
            review_due: false,
        };

        assert!(
            !record_path.exists(),
            "nothing is written before the publisher writes"
        );
        nvenc::store_resolution(Some(record_path.as_path()), &resolution, chrono::Utc::now());
        assert!(
            record_path.exists(),
            "the record must land in the path the caller gave: {}",
            record_path.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
