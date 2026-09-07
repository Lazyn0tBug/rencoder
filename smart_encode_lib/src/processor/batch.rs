//! Single-file transcode orchestrator.
//!
//! Pipeline (matches origin §3.3):
//!   1. Skip if output already exists.
//!   2. Probe metadata (single `ffprobe` call via `probe_video_metadata_auto`).
//!   3. Decide whether the source fps exceeds the cap.
//!   4. Skip if codec already matches and no reduce is needed.
//!   5. Select an encoder (cache + config).
//!   6. Build the ffmpeg argv (with `-r` output option placement).
//!   7. Execute via `Command` with `ProcessOutput`-driven stdio.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::Config;
use crate::error::EncodeError;
use crate::ffmpeg::{
    command::{build_ffmpeg_command, should_skip_encoding},
    encoder::select_encoder,
    probe::probe_video_metadata_auto,
};
use crate::types::{EncoderCache, EncodeParams, ProcessOutput, TranscodeResult, TranscodeTask};

/// Transcode a single file. Returns the result status rather than aborting
/// on failure -- batch callers collect per-file results.
pub fn transcode_single(
    task: &TranscodeTask,
    params: &EncodeParams,
    cache: &EncoderCache,
    config: &Config,
    output: ProcessOutput,
) -> Result<TranscodeResult, EncodeError> {
    if task.output.exists() {
        return Ok(TranscodeResult::SkippedExists);
    }
    let metadata = probe_video_metadata_auto(&task.input)?;
    let runner = move |argv: &[String]| run_ffmpeg(argv, output);
    transcode_after_probe(task, params, cache, config, metadata, runner)
}

/// Core orchestration after metadata is known. Split out so tests can
/// inject a mock runner without touching real `ffmpeg`.
fn transcode_after_probe<F>(
    task: &TranscodeTask,
    params: &EncodeParams,
    cache: &EncoderCache,
    config: &Config,
    metadata: crate::types::VideoMetadata,
    runner: F,
) -> Result<TranscodeResult, EncodeError>
where
    F: Fn(&[String]) -> Result<(), EncodeError>,
{
    let src_fps = metadata.best_fps();
    let codec = metadata.codec.as_deref().unwrap_or("");

    let need_reduce = src_fps
        .map(|s| params.max_fps.enabled() && params.max_fps.need_reduce(s))
        .unwrap_or(false);

    if should_skip_encoding(codec, params.format, need_reduce) {
        return Ok(TranscodeResult::SkippedSameCodec);
    }

    let encoder = select_encoder(params.format, cache, config)?;
    let argv = build_ffmpeg_command(task, params, &encoder, src_fps);

    match runner(&argv) {
        Ok(()) => {
            if need_reduce {
                Ok(TranscodeResult::ReducedFps)
            } else {
                Ok(TranscodeResult::Success)
            }
        }
        Err(e) => Ok(TranscodeResult::Failed(e)),
    }
}

fn run_ffmpeg(argv: &[String], output: ProcessOutput) -> Result<(), EncodeError> {
    if argv.is_empty() {
        return Err(EncodeError::Ffmpeg("empty argv".into()));
    }
    let program = &argv[0];
    let args = &argv[1..];
    let mut cmd = Command::new(program);
    cmd.args(args);
    match output {
        ProcessOutput::Inherit => {}
        ProcessOutput::Piped => {
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
        }
        ProcessOutput::Null => {
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
        }
    }
    let status = cmd
        .status()
        .map_err(|e| EncodeError::Ffmpeg(format!("spawn {program}: {e}")))?;
    if !status.success() {
        return Err(EncodeError::Ffmpeg(format!("{program} exited {status}")));
    }
    Ok(())
}

/// Probe helper exposed for the batch caller (U7). Kept here so it lives
/// near the orchestration that uses it; the real probe lives in
/// `ffmpeg::probe`.
#[allow(dead_code)]
pub fn probe_input(path: &Path) -> Result<crate::types::VideoMetadata, EncodeError> {
    probe_video_metadata_auto(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MaxFpsConfig, VideoFormat, VideoMetadata};
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    fn task(input: &str, output: &str) -> TranscodeTask {
        TranscodeTask {
            input: PathBuf::from(input),
            output: PathBuf::from(output),
        }
    }

    fn h264_metadata(fps: Option<f64>) -> VideoMetadata {
        VideoMetadata {
            codec: Some("h264".into()),
            avg_fps: fps,
            nominal_fps: None,
        }
    }

    fn vp9_metadata(fps: Option<f64>) -> VideoMetadata {
        VideoMetadata {
            codec: Some("vp9".into()),
            avg_fps: fps,
            nominal_fps: None,
        }
    }

    fn cache_with_libx264_and_svtav1(has_nvidia: bool) -> EncoderCache {
        EncoderCache {
            encoders_output: "\
Encoders:
 V..... libx264              libx264 H.264
 V..... libsvtav1            libsvtav1 SVT-AV1
"
            .to_string(),
            has_nvidia,
        }
    }

    /// Runner that records the argv it was called with.
    fn recording_runner() -> (
        impl Fn(&[String]) -> Result<(), EncodeError> + Send + 'static,
        std::sync::Arc<StdMutex<Vec<String>>>,
    ) {
        use std::sync::{Arc, Mutex};
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        let runner = move |argv: &[String]| -> Result<(), EncodeError> {
            cap.lock().unwrap().extend_from_slice(argv);
            Ok(())
        };
        (runner, captured)
    }

    // ---- transcode_after_probe ----

    #[test]
    fn skips_same_codec_when_fps_within_cap() {
        let metadata = h264_metadata(Some(30.0));
        let (runner, captured) = recording_runner();
        let params = EncodeParams {
            format: VideoFormat::X264,
            is_crf: true,
            bitrate_or_crf: 28,
            ..Default::default()
        };
        let cache = cache_with_libx264_and_svtav1(false);
        let config = Config::default();

        let result =
            transcode_after_probe(&task("/in.mp4", "/out.mp4"), &params, &cache, &config, metadata, runner)
                .unwrap();

        assert!(matches!(result, TranscodeResult::SkippedSameCodec));
        assert!(captured.lock().unwrap().is_empty(), "runner should not have been called");
    }

    #[test]
    fn transcodes_when_codec_differs() {
        let metadata = vp9_metadata(Some(30.0));
        let (runner, captured) = recording_runner();
        let params = EncodeParams {
            format: VideoFormat::Av1,
            is_crf: true,
            bitrate_or_crf: 45,
            ..Default::default()
        };
        let cache = cache_with_libx264_and_svtav1(false);
        let config = Config::default();

        let result =
            transcode_after_probe(&task("/in.webm", "/out.mkv"), &params, &cache, &config, metadata, runner)
                .unwrap();

        assert!(matches!(result, TranscodeResult::Success));
        let argv = captured.lock().unwrap();
        assert!(argv.contains(&"ffmpeg".to_string()));
        assert!(argv.contains(&"libsvtav1".to_string()));
        assert!(argv.contains(&"-crf".to_string()));
    }

    #[test]
    fn reduces_fps_when_needed_and_codec_matches() {
        // h264 source at 60fps with X264 target + cap=30 -> transcode to reduce.
        let metadata = h264_metadata(Some(60.0));
        let (runner, captured) = recording_runner();
        let params = EncodeParams {
            format: VideoFormat::X264,
            is_crf: true,
            bitrate_or_crf: 28,
            max_fps: MaxFpsConfig { max_fps: 30.0, eps: 0.05 },
            ..Default::default()
        };
        let cache = cache_with_libx264_and_svtav1(false);
        let config = Config::default();

        let result =
            transcode_after_probe(&task("/in.mp4", "/out.mp4"), &params, &cache, &config, metadata, runner)
                .unwrap();

        assert!(matches!(result, TranscodeResult::ReducedFps));
        let argv = captured.lock().unwrap();
        let r_pos = argv.iter().position(|s| s == "-r").expect("-r present in argv");
        assert_eq!(argv[r_pos + 1], "30");
    }

    #[test]
    fn no_r_injected_when_src_fps_none() {
        // vp9 -> AV1 codec mismatch so we actually transcode; src_fps is None
        // so the -r output option must be omitted even though max_fps is set
        // (R21: prefer no transcoding-with-guessed-fps over corrupting output).
        let metadata = vp9_metadata(None);
        let (runner, captured) = recording_runner();
        let params = EncodeParams {
            format: VideoFormat::Av1,
            is_crf: true,
            bitrate_or_crf: 45,
            max_fps: MaxFpsConfig { max_fps: 30.0, eps: 0.05 },
            ..Default::default()
        };
        let cache = cache_with_libx264_and_svtav1(false);
        let config = Config::default();

        let result =
            transcode_after_probe(&task("/in.webm", "/out.mkv"), &params, &cache, &config, metadata, runner)
                .unwrap();

        assert!(matches!(result, TranscodeResult::Success));
        let argv = captured.lock().unwrap();
        assert!(!argv.contains(&"-r".to_string()), "R21: no -r when src_fps is None");
    }

    #[test]
    fn propagates_ffmpeg_failure_as_failed_result() {
        let metadata = vp9_metadata(Some(30.0));
        let runner = |_: &[String]| -> Result<(), EncodeError> {
            Err(EncodeError::Ffmpeg("simulated failure".into()))
        };
        let params = EncodeParams {
            format: VideoFormat::Av1,
            is_crf: true,
            bitrate_or_crf: 45,
            ..Default::default()
        };
        let cache = cache_with_libx264_and_svtav1(false);
        let config = Config::default();

        let result =
            transcode_after_probe(&task("/in.webm", "/out.mkv"), &params, &cache, &config, metadata, runner)
                .unwrap();

        match result {
            TranscodeResult::Failed(EncodeError::Ffmpeg(msg)) => {
                assert!(msg.contains("simulated failure"));
            }
            other => panic!("expected Failed(Ffmpeg), got {other:?}"),
        }
    }

    #[test]
    fn encoder_not_found_propagates() {
        let metadata = vp9_metadata(Some(30.0));
        let runner = |_: &[String]| -> Result<(), EncodeError> {
            panic!("runner should not be called when select_encoder fails");
        };
        let params = EncodeParams {
            format: VideoFormat::Av1,
            is_crf: true,
            bitrate_or_crf: 45,
            ..Default::default()
        };
        // Empty cache -> no AV1 encoder available.
        let cache = EncoderCache {
            encoders_output: String::new(),
            has_nvidia: false,
        };
        let config = Config::default();

        let err = transcode_after_probe(
            &task("/in.webm", "/out.mkv"),
            &params,
            &cache,
            &config,
            metadata,
            runner,
        )
        .unwrap_err();
        assert!(matches!(err, EncodeError::EncoderNotFound(_)));
    }

    // ---- transcode_single (public) ----

    #[test]
    fn skips_when_output_exists() {
        let dir = std::env::temp_dir().join("rencoder_transcode_skip_exists");
        std::fs::create_dir_all(&dir).unwrap();
        let output = dir.join("out.mkv");
        std::fs::write(&output, b"").unwrap();

        let task = TranscodeTask {
            input: PathBuf::from("/nonexistent_input.mp4"),
            output,
        };
        let params = EncodeParams::default();
        let cache = cache_with_libx264_and_svtav1(false);
        let config = Config::default();

        let result = transcode_single(&task, &params, &cache, &config, ProcessOutput::Inherit).unwrap();
        assert!(matches!(result, TranscodeResult::SkippedExists));
    }
}
