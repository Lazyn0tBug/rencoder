//! One-shot `EncoderCache`: `ffmpeg -encoders` output plus the NVENC
//! capability probe.
//!
//! Built once per batch via `init_encoder_cache`, then passed by `&` to
//! every `select_encoder` call. The `ffmpeg -encoders` output never
//! changes during a run, so cloning the (potentially multi-MB) string
//! per file would be wasteful.
//!
//! NVENC detection is a **capability probe**, not an existence check:
//! `ffmpeg -encoders` says the encoder was compiled in, `nvidia-smi -L`
//! says a GPU exists -- neither says this machine can actually run
//! `av1_nvenc` (compute capability >= 8.0, matching driver, free NVENC
//! session). The probe answers that with a 1-frame test encode.
//!
//! The probe result is a property of the machine, not of a batch: it is
//! kept on disk by [`crate::encoder::nvenc`] and reused across
//! processes. This module owns the "build a cache from a resolved
//! capability" part.

use crate::encoder::nvenc;
use crate::error::EncodeError;
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

/// What the background cache init produces: the cache (or the error
/// that prevented building it) plus the NVENC capability summary the
/// UI shows next to the NVENC switch.
#[derive(Debug)]
pub struct CacheInit {
    pub cache: Result<EncoderCache, EncodeError>,
    /// `None` when the resolve failed.
    ///
    /// The one capability the whole chain shares: the encoder
    /// selection reads it, the runtime fallback updates it, the UI
    /// shows it (design doc §3.10.5).
    pub capability: Option<nvenc::Capability>,
}

/// The cache plus the capability summary, from one resolution.
///
/// The **only** place a resolution becomes what the frontends see
/// (design doc §3.10.3: the wording and the mapping live in the lib,
/// the frontends only display).
pub fn cache_init_from_resolution(resolution: &nvenc::Resolution) -> CacheInit {
    CacheInit {
        cache: Ok(cache_from_resolution(resolution)),
        capability: Some(resolution.capability()),
    }
}

/// Build the cache on a background thread; return a receiver the
/// caller can poll/await (review #126).
///
/// `ffmpeg -encoders` takes ~50-200ms on a cold start, and the
/// capability probe spawns a 1-frame encode per NVENC encoder. Blocking
/// the actor thread for that delays `Command::StartBatch` processing,
/// so the init runs on its own thread.
///
/// The record's path is a **parameter**: the caller decides where the
/// capability record lives (the app directory in production, a
/// temporary directory in a test). The lib does not resolve it -- a
/// test that resolved it would write the developer's real capability
/// record (design doc §3.10.4).
pub fn init_encoder_cache_async_with_capability_at<F>(
    path: Option<std::path::PathBuf>,
    runner: F,
) -> std::sync::mpsc::Receiver<CacheInit>
where
    F: Fn(&[&str]) -> Result<String, EncodeError> + Send + Sync + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("renencoder-cache-init".into())
        .spawn(move || {
            let init = match nvenc::resolve_and_store_with(path.as_deref(), false, runner) {
                Ok(resolution) => cache_init_from_resolution(&resolution),
                Err(e) => CacheInit {
                    cache: Err(e),
                    capability: None,
                },
            };
            // Block on the send until the consumer picks up. With
            // sync_channel(1), the first send succeeds (buffer slot
            // empty); subsequent sends would fail because the
            // consumer has already received. We only call once so
            // this is fine.
            let _ = tx.send(init);
        })
        .expect("spawn cache-init thread");
    rx
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

    // Review delta #8 (delta-review): regression coverage for
    // init_encoder_cache_async. The async path was previously
    // untested; this test exercises the thread spawn + receiver
    // round-trip. A failure to populate the channel would hang
    // here (timeout).
    #[test]
    fn init_encoder_cache_async_delivers_result_into_the_given_path() {
        // The record path is a parameter: the test points it at a
        // temporary directory, so the test never writes the
        // developer's real capability record -- and it never spawns
        // the real ffmpeg (the fake runner answers every call).
        let dir = std::env::temp_dir().join(format!(
            "renencoder_cache_async_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp dir must be created");
        let record_path = dir.join("nvenc-capability.json");

        let rx = init_encoder_cache_async_with_capability_at(Some(record_path.clone()), |_args| {
            Ok(String::new())
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("init_encoder_cache_async channel closed without result");

        assert!(result.cache.is_ok(), "the fake runner always answers");
        assert!(
            result.capability.is_some(),
            "the capability summary must ride along"
        );
        assert!(
            record_path.exists(),
            "the record must land in the path the caller gave: {}",
            record_path.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
