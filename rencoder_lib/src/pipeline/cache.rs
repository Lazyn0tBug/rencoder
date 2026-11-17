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

use std::sync::OnceLock;

use crate::error::EncodeError;
use crate::types::{EncoderCache, NvencSupport};

/// NVENC encoders that may exist in an ffmpeg build. Only the ones
/// actually present in `ffmpeg -encoders` get probed.
const NVENC_CANDIDATES: [&str; 3] = ["h264_nvenc", "hevc_nvenc", "av1_nvenc"];

/// Process-level cache of the probe result. Probing costs ~200-500ms per
/// candidate; a long-lived process (the GUI) may run several batches and
/// the machine's capabilities do not change under it.
static NVENC_SUPPORT: OnceLock<Vec<NvencSupport>> = OnceLock::new();

/// Build an `EncoderCache` for the current run.
///
/// `enable_nv = false` skips the probe entirely (zero extra cost).
/// `true` runs the NVENC capability probe; the result is cached
/// process-wide and reused by later calls.
///
/// A probe that fails never aborts the run: the affected encoder is
/// simply not usable, and `select_encoder` falls back to software
/// encoding with a warning.
pub fn init_encoder_cache(enable_nv: bool) -> Result<EncoderCache, EncodeError> {
    let encoders_output = run_subprocess("ffmpeg", &["-encoders"])?;
    let nvenc = if enable_nv {
        NVENC_SUPPORT
            .get_or_init(|| detect_nvenc(&run_subprocess, &encoders_output))
            .clone()
    } else {
        Vec::new()
    };
    if enable_nv && !nvenc.iter().any(|s| s.usable) {
        tracing::warn!(
            "NVENC is enabled but no encoder passed the capability probe; using software encoding"
        );
    }
    Ok(EncoderCache {
        encoders_output,
        nvenc,
    })
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
#[cfg(test)]
fn init_encoder_cache_impl<F>(enable_nv: bool, runner: F) -> Result<EncoderCache, EncodeError>
where
    F: Fn(&str, &[&str]) -> Result<String, EncodeError>,
{
    let encoders_output = runner("ffmpeg", &["-encoders"])?;
    let nvenc = if enable_nv {
        detect_nvenc(&runner, &encoders_output)
    } else {
        Vec::new()
    };
    Ok(EncoderCache {
        encoders_output,
        nvenc,
    })
}

/// Probe every NVENC encoder this ffmpeg build has.
///
/// The probe is a 1-frame test encode through lavfi: it exercises the
/// driver, the ffmpeg build, the NVENC session quota and the GPU's
/// compute capability in one go -- the only combination that actually
/// proves the encoder can run here.
fn detect_nvenc<F>(runner: &F, encoders_output: &str) -> Vec<NvencSupport>
where
    F: Fn(&str, &[&str]) -> Result<String, EncodeError>,
{
    let mut support = Vec::new();
    for encoder in NVENC_CANDIDATES {
        if !encoders_output.contains(encoder) {
            // Not compiled into this ffmpeg -- nothing to probe.
            continue;
        }
        match probe_nvenc(runner, encoder) {
            Ok(()) => {
                tracing::info!(encoder, "NVENC capability probe passed");
                support.push(NvencSupport {
                    encoder: encoder.to_string(),
                    usable: true,
                    reason: None,
                });
            }
            Err(reason) => {
                tracing::warn!(
                    encoder,
                    reason = %reason,
                    "NVENC capability probe failed; encoder unavailable on this machine"
                );
                support.push(NvencSupport {
                    encoder: encoder.to_string(),
                    usable: false,
                    reason: Some(reason),
                });
            }
        }
    }
    support
}

/// Can this NVENC encoder run here? Encode one frame of a synthetic
/// source with it.
fn probe_nvenc<F>(runner: &F, encoder: &str) -> Result<(), String>
where
    F: Fn(&str, &[&str]) -> Result<String, EncodeError>,
{
    runner(
        "ffmpeg",
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=s=64x64:d=0.1",
            "-frames:v",
            "1",
            "-c:v",
            encoder,
            "-f",
            "null",
            "-",
        ],
    )
    .map(|_| ())
    .map_err(|e| failure_reason(&e))
}

/// Turn a probe error into a log-friendly reason.
///
/// `run_subprocess` formats failures as `<program> exited <code>: <stderr>`;
/// the stderr part is what actually explains the failure, so keep it and
/// drop the wrapper.
fn failure_reason(err: &EncodeError) -> String {
    let owned;
    let text = match err {
        EncodeError::Probe(msg) => msg.as_str(),
        other => {
            owned = other.to_string();
            owned.as_str()
        }
    };
    let detail = match text.split_once(" exited ") {
        Some((_, rest)) => match rest.split_once(": ") {
            Some((_, stderr)) => stderr,
            None => rest,
        },
        None => text,
    };
    let detail: &str = detail;
    let detail = detail.trim();
    const MAX: usize = 200;
    if detail.chars().count() > MAX {
        let mut trimmed: String = detail.chars().take(MAX).collect();
        trimmed.push('…');
        trimmed
    } else {
        detail.to_string()
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
    use std::sync::{Arc, Mutex};

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

    /// Runner that answers `ffmpeg -encoders` and NVENC capability
    /// probes. Records every invocation so the tests can assert how
    /// many probes ran.
    fn recording_runner(
        encoders: &'static str,
        probe_ok: bool,
        calls: Arc<Mutex<Vec<String>>>,
    ) -> impl Fn(&str, &[&str]) -> Result<String, EncodeError> {
        move |program: &str, args: &[&str]| {
            assert_eq!(program, "ffmpeg", "only ffmpeg is spawned");
            calls.lock().unwrap().push(args.join(" "));
            if args == ["-encoders"] {
                return Ok(encoders.to_string());
            }
            if probe_ok {
                Ok(String::new())
            } else {
                Err(EncodeError::Probe(
                    "ffmpeg exited 1: Cannot load libcuda.so.1".into(),
                ))
            }
        }
    }

    #[test]
    fn enable_nv_false_skips_the_probe() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = recording_runner(ENCODERS_WITH_NVENC, true, calls.clone());
        let cache = init_encoder_cache_impl(false, runner).unwrap();
        assert!(cache.nvenc.is_empty());
        // Only `ffmpeg -encoders` ran -- no capability probe.
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(calls.lock().unwrap()[0], "-encoders");
    }

    #[test]
    fn probe_marks_every_nvenc_encoder_usable() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = recording_runner(ENCODERS_WITH_NVENC, true, calls.clone());
        let cache = init_encoder_cache_impl(true, runner).unwrap();
        assert_eq!(cache.nvenc.len(), 3);
        for encoder in ["h264_nvenc", "hevc_nvenc", "av1_nvenc"] {
            assert!(cache.nvenc_usable(encoder), "{encoder} should be usable");
        }
        // One probe per candidate, on top of `ffmpeg -encoders`.
        assert_eq!(calls.lock().unwrap().len(), 4);
    }

    #[test]
    fn probe_failure_marks_the_encoder_unusable_with_a_reason() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = recording_runner(ENCODERS_WITH_NVENC, false, calls.clone());
        let cache = init_encoder_cache_impl(true, runner).unwrap();
        assert_eq!(cache.nvenc.len(), 3);
        for encoder in ["h264_nvenc", "hevc_nvenc", "av1_nvenc"] {
            assert!(!cache.nvenc_usable(encoder), "{encoder} should be unusable");
            assert_eq!(
                cache.nvenc_failure_reason(encoder),
                Some("Cannot load libcuda.so.1")
            );
        }
    }

    #[test]
    fn probe_only_runs_for_encoders_in_the_build() {
        // The build has no NVENC encoders -> nothing to probe.
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = recording_runner(ENCODERS_BASIC, true, calls.clone());
        let cache = init_encoder_cache_impl(true, runner).unwrap();
        assert!(cache.nvenc.is_empty());
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn probe_command_is_a_one_frame_test_encode() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = recording_runner(ENCODERS_WITH_NVENC, true, calls.clone());
        let _cache = init_encoder_cache_impl(true, runner).unwrap();
        let recorded = calls.lock().unwrap().clone();
        let probe = recorded
            .iter()
            .find(|c| c.contains("-c:v av1_nvenc"))
            .expect("av1_nvenc probe should have run");
        assert!(probe.contains("-frames:v 1"), "probe must be one frame");
        assert!(
            probe.contains("-f lavfi"),
            "probe must use a synthetic source"
        );
    }

    #[test]
    fn failure_reason_is_trimmed_to_the_stderr_part() {
        let err = EncodeError::Probe("ffmpeg exited 1: No NVENC capable devices found".into());
        assert_eq!(failure_reason(&err), "No NVENC capable devices found");
        let long: String = "x".repeat(500);
        let err = EncodeError::Probe(format!("ffmpeg exited 1: {long}"));
        assert_eq!(failure_reason(&err).chars().count(), 201); // 200 + ellipsis
    }

    #[test]
    fn ffmpeg_failure_propagates() {
        let runner = |_program: &str, _args: &[&str]| -> Result<String, EncodeError> {
            Err(EncodeError::Probe("ffmpeg not found".into()))
        };
        let result = init_encoder_cache_impl(false, runner);
        assert!(matches!(result, Err(EncodeError::Probe(_))));
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
        // acceptable — what we're testing is that the channel
        // delivers a result within the timeout. The exact
        // variant depends on the host.
        let _ = result; // suppress unused warning
    }
}
