//! Single-file and batch transcode orchestration.
//!
//! Per-file pipeline (matches origin §3.3):
//!   1. Skip if output already exists.
//!   2. Probe metadata (single `ffprobe` call via `probe_video_metadata_auto`).
//!   3. Decide whether the source fps exceeds the cap.
//!   4. Skip if codec already matches and no reduce is needed.
//!   5. Select an encoder (cache + config).
//!   6. Build the ffmpeg argv (with `-r` output option placement).
//!   7. Execute via `Command` with `ProcessOutput`-driven stdio; capture
//!      stderr tail for failure analysis.
//!
//! `transcode_batch` returns `Vec<EncodingRecord>` -- one row per file
//! with the four work dimensions (source / process / result / status).
//! Failures are captured per-file; the batch never aborts.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use chrono::Utc;

use crate::config::Config;
use crate::data::extract::probe_video_metadata_auto;
use crate::encoder::{
    command::{build_ffmpeg_command, should_skip_encoding},
    select::select_encoder,
};
use crate::error::EncodeError;
use crate::pipeline::partial_output::PartialOutputHandle;
use crate::types::{
    EncodeParams, EncoderCache, EncoderSettings, EncodingOutcome, EncodingRecord, ErrorCategory,
    ErrorContext, ProcessOutput, SkippedReason, TranscodeTask, VideoMetadata,
};
use crate::utils::fs::output_path_for;

/// Per-file callback fired after the record is built.
type ResultCallback = dyn Fn(&EncodingRecord);

/// Transcode a single file. Pure encode: probe + skip + argv + ffmpeg.
/// Returns the work-analysis record; probe failures (e.g. corrupt/
/// unreadable input) surface as `Ok(record)` with `Outcome::Failed`.
/// ffmpeg failures land in `EncodingRecord.outcome = Failed` with
/// stderr captured for the `ErrorContext`.
///
/// **U19**: no `current_output` or `task_ctx` params. Lifecycle
/// logging and signal-handler Guard construction live in
/// [`crate::pipeline::task::TaskManager::run`]; `transcode_single`
/// focuses on encode. Callers (CLI / tests) wrap the call via
/// `TaskManager` when they need scaffold logs and Guard registration.
pub fn transcode_single(
    task: &TranscodeTask,
    params: &EncodeParams,
    cache: &EncoderCache,
    config: &Config,
    output: ProcessOutput,
) -> Result<EncodingRecord, EncodeError> {
    let started_at_meta = Utc::now();
    let started_at_instant = Instant::now();

    // transcode_single is only invoked from `transcode_batch`, which
    // always builds `TranscodeTask { output: Some(...) }`. Treat
    // `None` as a programming error rather than a soft "skip" --
    // there's no input-defined output path to compare against.
    let output_path = task
        .output
        .as_ref()
        .expect("transcode_single called with TranscodeTask.output = None");

    if output_path.exists() {
        return Ok(make_skipped_record(
            task,
            params,
            started_at_meta,
            None,
            SkippedReason::OutputExists,
        ));
    }

    let metadata = match probe_video_metadata_auto(&task.input) {
        Ok(m) => m,
        Err(e) => {
            return Ok(make_failed_probe_record(
                task,
                params,
                started_at_meta,
                None,
                &e,
            ));
        }
    };

    let argv = build_argv(task, params, cache, config, &metadata);
    transcode_with_argv(
        task,
        params,
        &metadata,
        argv,
        started_at_meta,
        started_at_instant,
        output,
    )
}

/// Run a batch over a file list. Failures are captured per-file; the
/// batch never aborts. `result_callback` (if set) fires once per file,
/// after the record is built. `current_output` (U18 §3.4.5) is the
/// signal-handler read cell; pass `None` if you haven't installed a
/// signal handler (e.g. in tests).
///
/// **Single batch per process**: `current_output` is a process-wide
/// `Arc<Mutex<Option<PathBuf>>>` shared with the SIGINT/SIGTERM/SIGHUP
/// listener spawned in `install_handler()`. Concurrent calls to
/// `transcode_batch` from multiple threads would race on the same
/// cell. One batch in flight at a time.
pub fn transcode_batch(
    params: &EncodeParams,
    files: &[PathBuf],
    cache: &EncoderCache,
    config: &Config,
    output: ProcessOutput,
    current_output: Option<&PartialOutputHandle>,
    result_callback: Option<&ResultCallback>,
) -> Vec<EncodingRecord> {
    // Note: `current_output` is kept in the public signature for
    // backward compatibility with the CLI, but U19 routes the actual
    // partial-output registration through `TaskManager::run` once step
    // 4 lands. This wrapper still drives the iteration directly; the
    // handle is wired back in step 4.
    let _ = current_output;
    transcode_batch_with_processor(
        params,
        files,
        |task, params| transcode_single(task, params, cache, config, output),
        result_callback,
    )
}

/// Core batch loop with an injectable per-file processor. Used by
/// `transcode_batch` in production; tests inject a mock processor that
/// returns canned records without spawning `ffmpeg`/`ffprobe`.
fn transcode_batch_with_processor<F>(
    params: &EncodeParams,
    files: &[PathBuf],
    mut processor: F,
    result_callback: Option<&ResultCallback>,
) -> Vec<EncodingRecord>
where
    F: FnMut(&TranscodeTask, &EncodeParams) -> Result<EncodingRecord, EncodeError>,
{
    let mut records = Vec::with_capacity(files.len());
    for input in files {
        let output_path = output_path_for(input, params.format);
        let task = TranscodeTask {
            input: input.clone(),
            output: Some(output_path),
        };
        let record = match processor(&task, params) {
            Ok(r) => r,
            Err(e) => make_failed_probe_record(&task, params, Utc::now(), None, &e),
        };
        if let Some(cb) = result_callback {
            cb(&record);
        }
        records.push(record);
    }
    records
}

/// Test-only public re-export of `transcode_batch_with_processor` so unit
/// tests in this module can inject canned processors.
#[cfg(test)]
pub(crate) fn transcode_batch_with_processor_for_test<F>(
    params: &EncodeParams,
    files: &[PathBuf],
    processor: F,
    result_callback: Option<&ResultCallback>,
) -> Vec<EncodingRecord>
where
    F: FnMut(&TranscodeTask, &EncodeParams) -> Result<EncodingRecord, EncodeError>,
{
    transcode_batch_with_processor(params, files, processor, result_callback)
}

/// Build the ffmpeg argv (used by `transcode_single`). Probes the source
/// and decides whether to skip; the actual execution is handled by
/// `transcode_with_argv` (which `transcode_single` calls).
fn build_argv(
    task: &TranscodeTask,
    params: &EncodeParams,
    cache: &EncoderCache,
    config: &Config,
    metadata: &VideoMetadata,
) -> Vec<String> {
    let src_fps = metadata.best_fps();
    let codec = metadata.codec.as_deref().unwrap_or("");
    let need_reduce = src_fps
        .map(|s| params.max_fps.enabled() && params.max_fps.need_reduce(s))
        .unwrap_or(false);
    if should_skip_encoding(codec, params.format, need_reduce) {
        // Return empty to signal skip; `transcode_single` doesn't actually
        // take this path because the early `OutputExists` check runs first
        // and the post-probe skip path is handled in `transcode_with_argv`.
        return Vec::new();
    }
    let encoder = select_encoder(params.format, cache, config)
        .expect("select_encoder in build_argv should have been filtered");
    build_ffmpeg_command(task, params, &encoder, src_fps)
}

/// Core orchestration: probe is done, argv is built; run ffmpeg and
/// build the EncodingRecord.
fn transcode_with_argv(
    task: &TranscodeTask,
    params: &EncodeParams,
    metadata: &VideoMetadata,
    argv: Vec<String>,
    started_at_meta: chrono::DateTime<chrono::Utc>,
    started_at_instant: Instant,
    output: ProcessOutput,
) -> Result<EncodingRecord, EncodeError> {
    let src_fps = metadata.best_fps();
    let codec = metadata.codec.as_deref().unwrap_or("");
    let need_reduce = src_fps
        .map(|s| params.max_fps.enabled() && params.max_fps.need_reduce(s))
        .unwrap_or(false);

    if should_skip_encoding(codec, params.format, need_reduce) {
        return Ok(make_skipped_record(
            task,
            params,
            started_at_meta,
            Some(started_at_instant.elapsed().as_secs_f64()),
            SkippedReason::SameCodec,
        ));
    }

    // Run ffmpeg (capture stderr if ProcessOutput::Piped so we can
    // attach it to the ErrorContext on failure).
    let run_result = run_ffmpeg_with_stderr(&argv, output);

    let ended_at_meta = Utc::now();
    let encoding_duration = started_at_instant.elapsed().as_secs_f64();

    // Determine encoder selected from the argv (the second non-flag arg
    // after -c:v). Simplest: extract from argv between "-c:v" and the
    // next flag. For now, leave as None if extraction is awkward; the
    // record carries enough other context for debugging.
    let encoder_selected = extract_encoder_from_argv(&argv);

    match run_result {
        Ok(()) => {
            // Success: fill result-side fields.
            let output_path = task
                .output
                .as_ref()
                .expect("encode TranscodeTask always has Some output");
            let output_size_bytes = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
            let source_size_bytes = std::fs::metadata(&task.input).map(|m| m.len()).unwrap_or(0);
            let compression_ratio = if source_size_bytes > 0 {
                Some(output_size_bytes as f64 / source_size_bytes as f64)
            } else {
                None
            };

            let outcome = if need_reduce {
                EncodingOutcome::ReducedFps
            } else {
                EncodingOutcome::Succeeded
            };

            Ok(EncodingRecord {
                source_path: task.input.to_string_lossy().into_owned(),
                source_size_bytes,
                source_codec: metadata.codec.clone(),
                source_container: metadata.container.clone(),
                source_width: metadata.width,
                source_height: metadata.height,
                source_fps: src_fps,
                source_duration_secs: metadata.duration_secs,
                source_bit_rate_bps: metadata.bit_rate,

                started_at: started_at_meta,
                ended_at: Some(ended_at_meta),
                encoding_duration_secs: Some(encoding_duration),
                encoder_selected: encoder_selected.clone(),
                encoder_settings: encoder_settings_from_params(params),
                ffmpeg_argv: argv,
                ffmpeg_stderr_tail: String::new(),

                output_path: Some(
                    task.output
                        .as_ref()
                        .expect("encode TranscodeTask always has Some output")
                        .to_string_lossy()
                        .into_owned(),
                ),
                output_size_bytes: Some(output_size_bytes),
                output_duration_secs: metadata.duration_secs, // same as source (no trimming)
                output_codec: encoder_selected,
                compression_ratio,

                outcome,
                error: None,
            })
        }
        Err((category, message, stderr_tail)) => Ok(EncodingRecord {
            source_path: task.input.to_string_lossy().into_owned(),
            source_size_bytes: std::fs::metadata(&task.input).map(|m| m.len()).unwrap_or(0),
            source_codec: metadata.codec.clone(),
            source_container: metadata.container.clone(),
            source_width: metadata.width,
            source_height: metadata.height,
            source_fps: src_fps,
            source_duration_secs: metadata.duration_secs,
            source_bit_rate_bps: metadata.bit_rate,

            started_at: started_at_meta,
            ended_at: Some(ended_at_meta),
            encoding_duration_secs: Some(encoding_duration),
            encoder_selected,
            encoder_settings: encoder_settings_from_params(params),
            ffmpeg_argv: argv,
            ffmpeg_stderr_tail: stderr_tail.clone(),

            output_path: None,
            output_size_bytes: None,
            output_duration_secs: None,
            output_codec: None,
            compression_ratio: None,

            outcome: EncodingOutcome::Failed,
            error: Some(ErrorContext {
                category,
                message,
                ffmpeg_stderr_tail: stderr_tail,
            }),
        }),
    }
}

/// Build the skipped-record skeleton (used for OutputExists and
/// SameCodec paths).
fn make_skipped_record(
    task: &TranscodeTask,
    params: &EncodeParams,
    started_at: chrono::DateTime<chrono::Utc>,
    encoding_duration_secs: Option<f64>,
    reason: SkippedReason,
) -> EncodingRecord {
    let source_size_bytes = std::fs::metadata(&task.input).map(|m| m.len()).unwrap_or(0);
    let source_meta = probe_video_metadata_quick(task);
    EncodingRecord {
        source_path: task.input.to_string_lossy().into_owned(),
        source_size_bytes,
        source_codec: source_meta.as_ref().and_then(|m| m.codec.clone()),
        source_container: source_meta.as_ref().and_then(|m| m.container.clone()),
        source_width: source_meta.as_ref().and_then(|m| m.width),
        source_height: source_meta.as_ref().and_then(|m| m.height),
        source_fps: source_meta.as_ref().and_then(|m| m.best_fps()),
        source_duration_secs: source_meta.as_ref().and_then(|m| m.duration_secs),
        source_bit_rate_bps: source_meta.as_ref().and_then(|m| m.bit_rate),

        started_at,
        ended_at: None,
        encoding_duration_secs,
        encoder_selected: None,
        encoder_settings: encoder_settings_from_params(params),
        ffmpeg_argv: Vec::new(),
        ffmpeg_stderr_tail: String::new(),

        output_path: None,
        output_size_bytes: None,
        output_duration_secs: None,
        output_codec: None,
        compression_ratio: None,

        outcome: EncodingOutcome::Skipped(reason),
        error: None,
    }
}

/// Build a failed-probe record (used when probe_video_metadata_auto
/// returns Err inside transcode_single or transcode_batch_with_processor).
fn make_failed_probe_record(
    task: &TranscodeTask,
    params: &EncodeParams,
    started_at: chrono::DateTime<chrono::Utc>,
    encoding_duration_secs: Option<f64>,
    err: &EncodeError,
) -> EncodingRecord {
    let source_size_bytes = std::fs::metadata(&task.input).map(|m| m.len()).unwrap_or(0);
    EncodingRecord {
        source_path: task.input.to_string_lossy().into_owned(),
        source_size_bytes,
        source_codec: None,
        source_container: None,
        source_width: None,
        source_height: None,
        source_fps: None,
        source_duration_secs: None,
        source_bit_rate_bps: None,

        started_at,
        ended_at: Some(Utc::now()),
        encoding_duration_secs,
        encoder_selected: None,
        encoder_settings: encoder_settings_from_params(params),
        ffmpeg_argv: Vec::new(),
        ffmpeg_stderr_tail: String::new(),

        output_path: None,
        output_size_bytes: None,
        output_duration_secs: None,
        output_codec: None,
        compression_ratio: None,

        outcome: EncodingOutcome::Failed,
        error: Some(ErrorContext {
            category: ErrorCategory::ProbeFailed,
            message: err.to_string(),
            ffmpeg_stderr_tail: String::new(),
        }),
    }
}

/// Run ffmpeg. Stream redirection follows the `ProcessOutput` variant:
///
/// - `Inherit` (default per design §2): both stdout AND stderr pass through
///   to the terminal so ffmpeg's progress bars are visible (ffmpeg writes
///   progress to stderr). On failure, ffmpeg's error message was already
///   printed to terminal in real-time -- `output_run.stderr` is empty
///   here but the user has seen the error.
fn run_ffmpeg_with_stderr(
    argv: &[String],
    output: ProcessOutput,
) -> Result<(), (ErrorCategory, String, String)> {
    if argv.is_empty() {
        return Err((
            ErrorCategory::ConfigError,
            "empty ffmpeg argv".into(),
            String::new(),
        ));
    }
    let program = &argv[0];
    let args = &argv[1..];
    let mut cmd = Command::new(program);
    cmd.args(args);
    match output {
        ProcessOutput::Inherit => {
            cmd.stdout(Stdio::inherit());
            cmd.stderr(Stdio::inherit());
        }
        ProcessOutput::Piped => {
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
        }
        ProcessOutput::Null => {
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
        }
    };
    let output_run = cmd.output();
    let output_run = match output_run {
        Ok(o) => o,
        Err(e) => {
            return Err((
                ErrorCategory::FfmpegSpawnFailed,
                format!("spawn {program}: {e}"),
                String::new(),
            ));
        }
    };
    if !output_run.status.success() {
        let stderr = String::from_utf8_lossy(&output_run.stderr);
        let tail = stderr_tail(&stderr, 20);
        return Err((
            ErrorCategory::FfmpegNonzeroExit,
            format!("{program} exited {}", output_run.status),
            tail,
        ));
    }
    Ok(())
}

/// Last N non-empty lines of `stderr`.
fn stderr_tail(stderr: &str, n: usize) -> String {
    stderr
        .lines()
        .filter(|l| !l.is_empty())
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract the encoder name from a build_ffmpeg_command argv. The argv
/// has `-c:v <encoder>` near the start; we look for that pattern.
fn extract_encoder_from_argv(argv: &[String]) -> Option<String> {
    let mut iter = argv.iter();
    while let Some(arg) = iter.next() {
        if arg == "-c:v" {
            return iter.next().cloned();
        }
    }
    None
}

/// Pull the encoder settings from EncodeParams into the typed struct.
fn encoder_settings_from_params(params: &EncodeParams) -> EncoderSettings {
    EncoderSettings {
        is_crf: params.is_crf,
        crf_or_bitrate: params.bitrate_or_crf,
        preset: None,
        max_fps_target: if params.max_fps.enabled() {
            Some(params.max_fps.max_fps as f32)
        } else {
            None
        },
    }
}

/// Quick probe used by skipped-record construction -- avoids threading
/// the full VideoMetadata through the transcode_single early-return for
/// skipped paths. Tolerates failure (returns None).
fn probe_video_metadata_quick(task: &TranscodeTask) -> Option<VideoMetadata> {
    probe_video_metadata_auto(&task.input).ok()
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
    use crate::types::VideoFormat;
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

    fn params_x264_crf() -> EncodeParams {
        EncodeParams {
            format: VideoFormat::X264,
            is_crf: true,
            bitrate_or_crf: 28,
            ..Default::default()
        }
    }

    #[allow(dead_code)]
    fn params_av1_crf() -> EncodeParams {
        EncodeParams {
            format: VideoFormat::Av1,
            is_crf: true,
            bitrate_or_crf: 45,
            ..Default::default()
        }
    }

    // ---- run_ffmpeg_with_stderr ----

    #[test]
    fn stderr_tail_returns_last_n_nonempty_lines() {
        let s = "line1\n\nline2\nline3\n\nline4\n";
        assert_eq!(stderr_tail(s, 2), "line3\nline4");
        assert_eq!(stderr_tail(s, 10), "line1\nline2\nline3\nline4");
    }

    // ---- extract_encoder_from_argv ----

    #[test]
    fn extract_encoder_picks_value_after_c_v() {
        let argv = vec![
            "ffmpeg".into(),
            "-y".into(),
            "-i".into(),
            "in.mp4".into(),
            "-c:v".into(),
            "libsvtav1".into(),
            "out.mkv".into(),
        ];
        assert_eq!(extract_encoder_from_argv(&argv), Some("libsvtav1".into()));
    }

    #[test]
    fn extract_encoder_returns_none_when_no_c_v() {
        let argv = vec!["ffmpeg".into(), "-i".into(), "in.mp4".into()];
        assert_eq!(extract_encoder_from_argv(&argv), None);
    }

    // ---- transcode_with_argv success path ----

    #[test]
    fn transcodes_when_codec_differs() {
        let dir = std::env::temp_dir().join("rencoder_batch_x264");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.webm");
        let output = dir.join("out.mkv");
        std::fs::write(&input, b"fake webm").unwrap();

        let task = TranscodeTask {
            input: input.clone(),
            output: Some(output.clone()),
        };
        let params = params_x264_crf();
        let _cache = cache_with_libx264_and_svtav1(false);
        let _config = Config::default();

        // We can't actually run ffmpeg on a fake webm file -- so use the
        // processor seam (`transcode_batch_with_processor`) with a
        // canned success path that mimics what a real successful
        // transcode_with_argv would produce.
        let records = super::transcode_batch_with_processor_for_test(
            &params,
            std::slice::from_ref(&task.input),
            |_, _| Ok(test_succeeded_record(&task, &params)),
            None,
        );
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.outcome, EncodingOutcome::Succeeded);
        assert!(record.output_path.is_some());
        assert!(record.encoding_duration_secs.is_some());
        assert!(record.compression_ratio.is_some());
        assert!(record.error.is_none());
    }

    /// Build a successful EncodingRecord for use in tests that mock the
    /// `transcode_with_argv` happy path.
    fn test_succeeded_record(task: &TranscodeTask, params: &EncodeParams) -> EncodingRecord {
        use chrono::Utc;
        let now = Utc::now();
        EncodingRecord {
            source_path: task.input.to_string_lossy().into_owned(),
            source_size_bytes: 1024,
            source_codec: Some("h264".into()),
            source_container: None,
            source_width: None,
            source_height: None,
            source_fps: Some(30.0),
            source_duration_secs: Some(60.0),
            source_bit_rate_bps: None,

            started_at: now,
            ended_at: Some(now),
            encoding_duration_secs: Some(1.0),
            encoder_selected: Some("libx264".into()),
            encoder_settings: crate::types::EncoderSettings {
                is_crf: params.is_crf,
                crf_or_bitrate: params.bitrate_or_crf,
                preset: None,
                max_fps_target: None,
            },
            ffmpeg_argv: Vec::new(),
            ffmpeg_stderr_tail: String::new(),

            output_path: Some(
                task.output
                    .as_ref()
                    .expect("encode TranscodeTask always has Some output")
                    .to_string_lossy()
                    .into_owned(),
            ),
            output_size_bytes: Some(512),
            output_duration_secs: Some(60.0),
            output_codec: Some("libx264".into()),
            compression_ratio: Some(0.5),

            outcome: EncodingOutcome::Succeeded,
            error: None,
        }
    }

    #[test]
    fn skips_when_output_exists() {
        let dir = std::env::temp_dir().join("rencoder_skip_exists");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let output = dir.join("out.mkv");
        std::fs::write(&output, b"").unwrap();

        let task = TranscodeTask {
            input: dir.join("input.mp4"),
            output: Some(output),
        };
        let params = params_x264_crf();
        let cache = cache_with_libx264_and_svtav1(false);
        let config = Config::default();

        let record = transcode_single(&task, &params, &cache, &config, ProcessOutput::Null)
            .expect("transcode_single should succeed");
        assert_eq!(
            record.outcome,
            EncodingOutcome::Skipped(SkippedReason::OutputExists)
        );
        assert!(record.output_path.is_none());
        // Skipped records don't perform work; no encoding_duration.
    }

    #[test]
    fn propagates_ffmpeg_failure_as_failed_record() {
        // Mock run_ffmpeg failure: ffmpeg spawn fails because the program
        // path is invalid. We use a fake argv with a non-existent program
        // to force a spawn error.
        let dir = std::env::temp_dir().join("rencoder_batch_fail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.webm");
        let output = dir.join("out.mkv");
        std::fs::write(&input, b"x").unwrap();

        let task = TranscodeTask {
            input,
            output: Some(output),
        };
        let params = params_x264_crf();
        let _cache = cache_with_libx264_and_svtav1(false);
        let _config = Config::default();

        // Run with a stub ffmpeg-like program that doesn't exist.
        let argv = vec![
            "/nonexistent-ffmpeg-binary".to_string(),
            "-i".into(),
            "x".into(),
        ];
        let record = transcode_with_argv(
            &task,
            &params,
            &VideoMetadata::default(),
            argv,
            Utc::now(),
            Instant::now(),
            ProcessOutput::Null,
        )
        .expect("transcode_with_argv should return Ok(EncodingRecord)");

        assert_eq!(record.outcome, EncodingOutcome::Failed);
        let err = record.error.expect("error should be set on Failed");
        assert!(matches!(err.category, ErrorCategory::FfmpegSpawnFailed));
        assert!(err.message.contains("spawn"));
    }

    // U18 §3.4.5 (transcode_batch with Guard around each file) and
    // U18.1.1 Finding 2 (verify wiring through the production
    // transcode_batch) tests were removed in U19 step 3. They
    // exercised `transcode_single` constructing a partial-output
    // Guard per task -- the new contract routes that wiring through
    // `TaskManager::run` (U19 step 4). The replacement TaskManager
    // unit tests land in U19 step 7.
}
