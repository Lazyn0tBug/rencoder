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

/// Build an `EncoderCache` for the current run, probing NVENC if asked.
///
/// `enable_nv = false` skips the probe entirely (zero extra cost).
/// `true` probes the NVENC encoders this ffmpeg build has; encoders
/// that fail the probe are simply not usable, and `select_encoder`
/// falls back to software encoding with a warning.
///
/// This is the "no store" path (the GUI, and any caller that has not
/// resolved the capability yet). Callers that keep the capability on
/// disk resolve it with [`crate::encoder::nvenc::resolve`] and build
/// the cache with [`cache_from_resolution`].
pub fn init_encoder_cache(enable_nv: bool) -> Result<EncoderCache, EncodeError> {
    let encoders_output = nvenc::run_ffmpeg(&["-encoders"])?;
    let support = if enable_nv {
        nvenc::probe(&encoders_output)
    } else {
        Vec::new()
    };
    if enable_nv && !support.iter().any(|s| s.usable) {
        tracing::warn!(
            "NVENC is enabled but no encoder passed the capability probe; using software encoding"
        );
    }
    Ok(EncoderCache {
        encoders_output,
        nvenc: support,
    })
}

/// Build an `EncoderCache` from an already-resolved NVENC capability.
///
/// No subprocess, no probe: the caller already knows which encoders
/// this machine can run.
pub fn cache_from_resolution(resolution: &nvenc::Resolution) -> EncoderCache {
    EncoderCache {
        encoders_output: resolution.encoders_output.clone(),
        nvenc: resolution.support.clone(),
    }
}

/// Build an `EncoderCache` on a background thread; return a receiver
/// the caller can poll/await (review #126).
///
/// `ffmpeg -encoders` takes ~50-200ms on a cold start; `nvidia-smi -L`
/// adds another 50-100ms. Blocking the actor thread for this duration
/// delays `Command::StartBatch` processing. By spawning the init on a
/// background thread and returning a one-shot receiver, the actor can
/// drain the `cmd_rx` loop while the cache is being built.
///
/// The receiver delivers the final `Result<EncoderCache, EncodeError>`
/// exactly once. A failed init returns `Err(EncodeError::Probe(...))`;
/// consumers should map that to `Event::Error` like the sync variant.
pub fn init_encoder_cache_async(
    enable_nv: bool,
) -> std::sync::mpsc::Receiver<Result<EncoderCache, EncodeError>> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("rencoder-cache-init".into())
        .spawn(move || {
            let result = init_encoder_cache(enable_nv);
            // Block on the send until the consumer picks up. With
            // sync_channel(1), the first send succeeds (buffer slot
            // empty); subsequent sends would fail because the
            // consumer has already received. We only call once so
            // this is fine.
            let _ = tx.send(result);
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
            nvenc: Vec::new(),
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
            nvenc: Vec::new(),
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
            to_save: None,
            review_due: false,
            rebuilt: false,
        };
        let cache = cache_from_resolution(&resolution);
        assert!(cache.nvenc_usable("av1_nvenc"));
        assert!(!cache.nvenc_usable("hevc_nvenc"));
        assert!(cache.has_encoder("libsvtav1"));
    }

    // Review delta #8 (delta-review): regression coverage for
    // init_encoder_cache_async. The async path was previously
    // untested; this test exercises the thread spawn + receiver
    // round-trip. A failure to populate the channel would hang
    // here (timeout).
    #[test]
    fn init_encoder_cache_async_delivers_result() {
        // Use a custom runner via init_encoder_cache_impl would
        // require re-plumbing; instead we exercise the real path
        // but tolerate the ffmpeg binary missing on the test host.
        let rx = init_encoder_cache_async(false);
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("init_encoder_cache_async channel closed without result");
        // Either Ok (ffmpeg present) or Err (ffmpeg missing) is
        // acceptable -- what we're testing is that the channel
        // delivers a result within the timeout. The exact
        // variant depends on the host.
        let _ = result; // suppress unused warning
    }
}
