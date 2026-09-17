//! One-shot `EncoderCache`: `ffmpeg -encoders` output plus optional
//! `nvidia-smi -L` detection.
//!
//! Built once per batch via `init_encoder_cache`, then passed by `&` to
//! every `select_encoder` call. The `ffmpeg -encoders` output never
//! changes during a run, so cloning the (potentially multi-MB) string
//! per file would be wasteful.

use crate::error::EncodeError;
use crate::types::EncoderCache;

/// Build an `EncoderCache` for the current run.
///
/// `enable_nv = false` skips `nvidia-smi` entirely. `true` runs the probe;
/// missing binary, spawn failure, or empty output all set `has_nvidia = false`
/// with an `info!` log (R20) -- not an `EncodeError`.
pub fn init_encoder_cache(enable_nv: bool) -> Result<EncoderCache, EncodeError> {
    init_encoder_cache_impl(enable_nv, run_subprocess)
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

/// Test seam: same as `init_encoder_cache` but with an injectable runner.
/// Production calls `init_encoder_cache` (which passes `run_subprocess`).
fn init_encoder_cache_impl<F>(enable_nv: bool, runner: F) -> Result<EncoderCache, EncodeError>
where
    F: Fn(&str, &[&str]) -> Result<String, EncodeError>,
{
    let encoders_output = runner("ffmpeg", &["-encoders"])?;
    let has_nvidia = enable_nv && detect_nvidia(&runner);
    Ok(EncoderCache {
        encoders_output,
        has_nvidia,
    })
}

fn detect_nvidia<F>(runner: &F) -> bool
where
    F: Fn(&str, &[&str]) -> Result<String, EncodeError>,
{
    match runner("nvidia-smi", &["-L"]) {
        Ok(out) => !out.trim().is_empty(),
        Err(e) => {
            log::info!("nvidia-smi unavailable, falling back to software encoding: {e}");
            false
        }
    }
}

fn run_subprocess(program: &str, args: &[&str]) -> Result<String, EncodeError> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| EncodeError::Probe(format!("{program} spawn: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(EncodeError::Probe(format!(
            "{program} exited {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EncoderCache;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

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
            has_nvidia: false,
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
            has_nvidia: true,
        };
        assert!(cache.has_encoder("av1_nvenc"));
        assert!(cache.has_encoder("h264_nvenc"));
        assert!(cache.has_encoder("hevc_nvenc"));
        assert!(cache.has_encoder("libsvtav1"));
    }

    #[test]
    fn enable_nv_false_skips_nvidia_check() {
        let nvidia_calls = Arc::new(AtomicUsize::new(0));
        let n = nvidia_calls.clone();
        let runner = move |program: &str, _args: &[&str]| -> Result<String, EncodeError> {
            match program {
                "ffmpeg" => Ok(ENCODERS_BASIC.to_string()),
                "nvidia-smi" => {
                    n.fetch_add(1, Ordering::SeqCst);
                    Ok("GPU 0: Test".to_string())
                }
                _ => Err(EncodeError::Probe(format!("unexpected {program}"))),
            }
        };
        let cache = init_encoder_cache_impl(false, runner).unwrap();
        assert!(!cache.has_nvidia);
        assert_eq!(nvidia_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn nvidia_present_sets_flag() {
        let runner = |program: &str, _args: &[&str]| -> Result<String, EncodeError> {
            match program {
                "ffmpeg" => Ok(ENCODERS_BASIC.to_string()),
                "nvidia-smi" => Ok("GPU 0: NVIDIA GeForce RTX 4090\n".to_string()),
                _ => Err(EncodeError::Probe(format!("unexpected {program}"))),
            }
        };
        let cache = init_encoder_cache_impl(true, runner).unwrap();
        assert!(cache.has_nvidia);
    }

    #[test]
    fn nvidia_missing_silently_falls_back() {
        let runner = |program: &str, _args: &[&str]| -> Result<String, EncodeError> {
            match program {
                "ffmpeg" => Ok(ENCODERS_BASIC.to_string()),
                "nvidia-smi" => Err(EncodeError::Probe("not found".into())),
                _ => Err(EncodeError::Probe(format!("unexpected {program}"))),
            }
        };
        let cache = init_encoder_cache_impl(true, runner).unwrap();
        assert!(!cache.has_nvidia);
    }

    #[test]
    fn nvidia_empty_output_silently_falls_back() {
        let runner = |program: &str, _args: &[&str]| -> Result<String, EncodeError> {
            match program {
                "ffmpeg" => Ok(ENCODERS_BASIC.to_string()),
                "nvidia-smi" => Ok(String::new()),
                _ => Err(EncodeError::Probe(format!("unexpected {program}"))),
            }
        };
        let cache = init_encoder_cache_impl(true, runner).unwrap();
        assert!(!cache.has_nvidia);
    }

    #[test]
    fn ffmpeg_invoked_exactly_once() {
        let ffmpeg_calls = Arc::new(AtomicUsize::new(0));
        let f = ffmpeg_calls.clone();
        let runner = move |program: &str, _args: &[&str]| -> Result<String, EncodeError> {
            match program {
                "ffmpeg" => {
                    f.fetch_add(1, Ordering::SeqCst);
                    Ok(ENCODERS_WITH_NVENC.to_string())
                }
                "nvidia-smi" => Ok("GPU 0: Test".to_string()),
                _ => Err(EncodeError::Probe(format!("unexpected {program}"))),
            }
        };
        let _cache = init_encoder_cache_impl(true, runner).unwrap();
        assert_eq!(ffmpeg_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ffmpeg_failure_propagates() {
        let runner = |_program: &str, _args: &[&str]| -> Result<String, EncodeError> {
            Err(EncodeError::Probe("ffmpeg not found".into()))
        };
        let result = init_encoder_cache_impl(false, runner);
        assert!(matches!(result, Err(EncodeError::Probe(_))));
    }
}
