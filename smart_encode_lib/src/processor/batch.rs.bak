//! Single-file and batch transcode orchestration.
//!
//! Per-file pipeline (matches origin §3.3):
//!   1. Skip if output already exists.
//!   2. Probe metadata (single `ffprobe` call via `probe_video_metadata_auto`).
//!   3. Decide whether the source fps exceeds the cap.
//!   4. Skip if codec already matches and no reduce is needed.
//!   5. Select an encoder (cache + config).
//!   6. Build the ffmpeg argv (with `-r` output option placement).
//!   7. Execute via `Command` with `ProcessOutput`-driven stdio.
//!
//! `transcode_batch` iterates over a file list and never short-circuits on
//! failures -- each file gets its own `TranscodeResult`. Per-file errors
//! land in `TranscodeResult::Failed(EncodeError)`, not in the batch return.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::Config;
use crate::error::EncodeError;
use crate::ffmpeg::{
    command::{build_ffmpeg_command, should_skip_encoding},
    encoder::select_encoder,
    probe::probe_video_metadata_auto,
};
use crate::path_util::output_path_for;
use crate::types::{EncoderCache, EncodeParams, ProcessOutput, TranscodeResult, TranscodeTask};

/// Per-file callback fired after the result is recorded. Used by callers
/// (CLI, GUI) to stream progress events instead of waiting for the
/// whole batch.
type ResultCallback = dyn Fn(&Path, &TranscodeResult);

/// Transcode a single file. Returns the result status rather than aborting
/// on `ffmpeg` failure -- that lands in `TranscodeResult::Failed`. Probe
/// failures (e.g. corrupt/unreadable input) still surface as `Err`.
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

/// Run a batch over a file list. Failures are captured per-file; the batch
/// never aborts. `result_callback` (if set) fires once per file, in input
/// order, after the result is recorded.
pub fn transcode_batch(
    params: &EncodeParams,
    files: &[PathBuf],
    cache: &EncoderCache,
    config: &Config,
    output: ProcessOutput,
    result_callback: Option<&ResultCallback>,
) -> Vec<(PathBuf, TranscodeResult)> {
    transcode_batch_with_processor(
        params,
        files,
        |task, params| transcode_single(task, params, cache, config, output),
        result_callback,
    )
}

/// Core batch loop with an injectable per-file processor. Used by
/// `transcode_batch` in production; tests inject a mock processor that
/// returns canned results without spawning `ffmpeg`/`ffprobe`.
fn transcode_batch_with_processor<F>(
    params: &EncodeParams,
    files: &[PathBuf],
    mut processor: F,
    result_callback: Option<&ResultCallback>,
) -> Vec<(PathBuf, TranscodeResult)>
where
    F: FnMut(&TranscodeTask, &EncodeParams) -> Result<TranscodeResult, EncodeError>,
{
    let mut results = Vec::with_capacity(files.len());
    for input in files {
        let output_path = output_path_for(input, params.format);
        let task = TranscodeTask {
            input: input.clone(),
            output: output_path,
        };
        let result = match processor(&task, params) {
            Ok(r) => r,
            Err(e) => TranscodeResult::Failed(e),
        };
        if let Some(cb) = result_callback {
            cb(input, &result);
        }
        results.push((input.clone(), result));
    }
    results
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
            ..Default::default()
        }
    }

    fn vp9_metadata(fps: Option<f64>) -> VideoMetadata {
        VideoMetadata {
            codec: Some("vp9".into()),
            avg_fps: fps,
            ..Default::default()
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

    /// Runner type alias: closure + shared buffer of captured argv.
    type RecordingRunner = (
        Box<dyn Fn(&[String]) -> Result<(), EncodeError> + Send + 'static>,
        std::sync::Arc<StdMutex<Vec<String>>>,
    );

    /// Runner that records the argv it was called with.
    fn recording_runner() -> RecordingRunner {
        use std::sync::{Arc, Mutex};
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        let runner = move |argv: &[String]| -> Result<(), EncodeError> {
            cap.lock().unwrap().extend_from_slice(argv);
            Ok(())
        };
        (Box::new(runner), captured)
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

    // ---- transcode_batch / transcode_batch_with_processor ----

    use super::transcode_batch_with_processor;
    use std::sync::{Arc, Mutex};

    /// Canned processor: returns Success for the first input, Failed for
    /// the second, Success for the third. Used to verify that one failure
    /// doesn't abort the batch.
    fn canned_processor(
        task: &TranscodeTask,
        _params: &EncodeParams,
    ) -> Result<TranscodeResult, EncodeError> {
        let name = task
            .input
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        match name {
            "fail.mp4" => Err(EncodeError::Ffmpeg("canned failure".into())),
            "ok.mp4" => Ok(TranscodeResult::Success),
            _ => Ok(TranscodeResult::Success),
        }
    }

    #[test]
    fn batch_processes_all_files_in_order() {
        let files = vec![
            PathBuf::from("/d/a.mp4"),
            PathBuf::from("/d/b.mp4"),
            PathBuf::from("/d/c.mp4"),
        ];
        let params = EncodeParams::default();
        let results = transcode_batch_with_processor(
            &params,
            &files,
            |task, _params| {
                let name = task.input.file_name().unwrap().to_str().unwrap();
                Ok(if name == "b.mp4" {
                    TranscodeResult::ReducedFps
                } else {
                    TranscodeResult::Success
                })
            },
            None,
        );
        assert_eq!(results.len(), 3);
        assert!(matches!(results[0].1, TranscodeResult::Success));
        assert!(matches!(results[1].1, TranscodeResult::ReducedFps));
        assert!(matches!(results[2].1, TranscodeResult::Success));
        // Order preserved: input paths in same order as input list.
        assert_eq!(results[0].0, PathBuf::from("/d/a.mp4"));
        assert_eq!(results[1].0, PathBuf::from("/d/b.mp4"));
        assert_eq!(results[2].0, PathBuf::from("/d/c.mp4"));
    }

    #[test]
    fn batch_failure_does_not_abort() {
        // Canned processor fails on the second file. Batch must still
        // produce a result for all three, with the middle one Failed.
        let files = vec![
            PathBuf::from("/d/ok.mp4"),
            PathBuf::from("/d/fail.mp4"),
            PathBuf::from("/d/ok.mp4"),
        ];
        let params = EncodeParams::default();
        let results = transcode_batch_with_processor(
            &params,
            &files,
            canned_processor,
            None,
        );
        assert_eq!(results.len(), 3);
        assert!(matches!(results[0].1, TranscodeResult::Success));
        match &results[1].1 {
            TranscodeResult::Failed(EncodeError::Ffmpeg(msg)) => {
                assert!(msg.contains("canned failure"));
            }
            other => panic!("expected Failed(Ffmpeg), got {other:?}"),
        }
        assert!(matches!(results[2].1, TranscodeResult::Success));
    }

    #[test]
    fn batch_callback_fires_once_per_file_in_order() {
        #[derive(Clone)]
        struct Call {
            path: PathBuf,
            is_success: bool,
        }

        let calls: Arc<Mutex<Vec<Call>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_clone = calls.clone();

        let files = vec![
            PathBuf::from("/d/a.mp4"),
            PathBuf::from("/d/fail.mp4"),
            PathBuf::from("/d/c.mp4"),
        ];
        let params = EncodeParams::default();

        let callback: &dyn Fn(&Path, &TranscodeResult) = &move |path, result| {
            calls_clone.lock().unwrap().push(Call {
                path: path.to_path_buf(),
                is_success: !matches!(result, TranscodeResult::Failed(_)),
            });
        };

        let _ = transcode_batch_with_processor(
            &params,
            &files,
            canned_processor,
            Some(callback),
        );

        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 3);
        assert_eq!(recorded[0].path, PathBuf::from("/d/a.mp4"));
        assert!(recorded[0].is_success);
        assert_eq!(recorded[1].path, PathBuf::from("/d/fail.mp4"));
        assert!(!recorded[1].is_success);
        assert_eq!(recorded[2].path, PathBuf::from("/d/c.mp4"));
        assert!(recorded[2].is_success);
    }

    #[test]
    fn batch_no_callback_still_records_results() {
        let files = vec![PathBuf::from("/d/a.mp4"), PathBuf::from("/d/b.mp4")];
        let params = EncodeParams::default();
        let results = transcode_batch_with_processor(
            &params,
            &files,
            |_, _| Ok(TranscodeResult::Success),
            None,
        );
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|(_, r)| matches!(r, TranscodeResult::Success)));
    }

    #[test]
    fn batch_empty_files_returns_empty() {
        let params = EncodeParams::default();
        let results = transcode_batch_with_processor(
            &params,
            &[],
            |_, _| unreachable!("processor not called for empty list"),
            None,
        );
        assert!(results.is_empty());
    }

    #[test]
    fn batch_uses_output_path_for_to_derive_task_output() {
        // Verifies the batch builds each TranscodeTask with the right
        // output path via output_path_for(input, format).
        let files = vec![PathBuf::from("/d/movie.avi")];
        let params = EncodeParams {
            format: VideoFormat::X264,
            ..Default::default()
        };
        let captured_task: Arc<Mutex<Option<TranscodeTask>>> = Arc::new(Mutex::new(None));
        let cap = captured_task.clone();
        let results = transcode_batch_with_processor(
            &params,
            &files,
            |task, _| {
                cap.lock().unwrap().replace(task.clone());
                Ok(TranscodeResult::Success)
            },
            None,
        );
        assert_eq!(results.len(), 1);
        let captured = captured_task.lock().unwrap().clone().unwrap();
        // AVI -> MP4 for X264 format via the current path_util stub.
        assert_eq!(captured.input, PathBuf::from("/d/movie.avi"));
        assert_eq!(captured.output, PathBuf::from("/d/movie.mp4"));
    }

    #[test]
    fn batch_public_api_routes_to_with_processor() {
        // Sanity: transcode_batch (public) wraps with_processor with the
        // real transcode_single. We can't easily test the ffmpeg path
        // without fixtures, so just assert the public function returns
        // the same shape (Vec<...>) and that calling it with an empty
        // list doesn't error.
        let params = EncodeParams::default();
        let cache = EncoderCache {
            encoders_output: String::new(),
            has_nvidia: false,
        };
        let config = Config::default();
        let results = transcode_batch(&params, &[], &cache, &config, ProcessOutput::Inherit, None);
        assert!(results.is_empty());
    }
}
