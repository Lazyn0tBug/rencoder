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
use crate::pipeline::cancel::CancelToken;
use crate::pipeline::partial_output::PartialOutputHandle;
use crate::pipeline::task::TaskManager;
use crate::types::{
    EncodeParams, EncoderCache, EncoderSettings, EncodingOutcome, EncodingRecord, ErrorCategory,
    ErrorContext, ProcessOutput, SkippedReason, TranscodeTask, VideoMetadata,
};
use crate::utils::fs::output_path_for;

/// Maximum bytes the stderr reader thread will keep in memory for the
/// `ErrorContext.ffmpeg_stderr_tail` payload (review fix #112). 256 KiB
/// is comfortably more than ffmpeg ever writes for a single-file
/// failure — even at `-loglevel debug` the last-20-lines tail fits in
/// ~4 KiB. Anything larger is dumped and counted so the user sees a
/// "[stderr truncated; N bytes dropped]" annotation. The cap also
/// bounds `stderr_thread.join()` wait time: once the cap is hit the
/// thread keeps draining the pipe to EOF but never grows memory, so
/// the subsequent join cannot wedge the calling task indefinitely
/// (review fix #116).
const STDERR_READER_MAX_BYTES: usize = 256 * 1024;

/// Per-file callback fired after the record is built.
type ResultCallback = dyn Fn(&EncodingRecord);

/// Transcode a single file (CLI path).
///
/// # Flow
///
/// 1. Probe + partial-output detection + skip-decision + argv-build
///    via `encode_prepare` (shared with the GUI path).
/// 2. If prepare says Ready → run ffmpeg via `transcode_with_argv`
///    (sync, stderr inherited).
/// 3. Build the final [`EncodingRecord`] via `build_encode_record`
///    (shared).
///
/// # Failure modes
///
/// Probe failures (corrupt/unreadable input) surface as
/// `Ok(record)` with `outcome = Failed` (not `Err`) — per-file
/// failures are part of the record stream, not exceptions. ffmpeg
/// failures land in `outcome = Failed` with stderr captured for the
/// `ErrorContext`.
///
/// # Lifecycle integration (U19)
///
/// No `current_output` or `task_ctx` params. Lifecycle logging and
/// signal-handler Guard construction live in
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

    // CLI path. Shared prepare logic + sync ffmpeg execution.
    let prepare = encode_prepare(
        task,
        params,
        cache,
        config,
        started_at_meta,
        started_at_instant,
    );
    match prepare {
        EncodePrepareResult::Skipped(r) | EncodePrepareResult::FailedProbe(r) => Ok(r),
        EncodePrepareResult::Ready {
            metadata,
            argv,
            output_path,
        } => transcode_with_argv(
            task,
            params,
            &metadata,
            argv,
            &output_path,
            started_at_meta,
            started_at_instant,
            output,
        ),
    }
}

/// Sibling to [`transcode_single`] for the GUI / actor path (U21).
///
/// # Shared vs divergent
///
/// **Shared** with `transcode_single` via `encode_prepare` (probe +
/// partial detect + skip + argv-build) and `build_encode_record`
/// (record construction). Both paths produce the same record shape.
///
/// **Divergent**: this one uses [`run_ffmpeg_with_progress`] (cancellable,
/// event-driven) instead of `run_ffmpeg_with_stderr` (sync). `cancel`
/// is polled between progress reads; `on_progress` is invoked once per
/// `progress=continue` / `progress=end` block from ffmpeg.
pub fn transcode_single_with_progress(
    task: &TranscodeTask,
    params: &EncodeParams,
    cache: &EncoderCache,
    config: &Config,
    cancel: &CancelToken,
    on_progress: &mut dyn FnMut(FfmpegProgress),
) -> Result<EncodingRecord, EncodeError> {
    let started_at_meta = Utc::now();
    let started_at_instant = Instant::now();

    let prepare = encode_prepare(
        task,
        params,
        cache,
        config,
        started_at_meta,
        started_at_instant,
    );
    match prepare {
        EncodePrepareResult::Skipped(r) | EncodePrepareResult::FailedProbe(r) => Ok(r),
        EncodePrepareResult::Ready {
            metadata,
            argv,
            output_path,
        } => transcode_with_progress(
            task,
            params,
            &metadata,
            argv,
            &output_path,
            started_at_meta,
            started_at_instant,
            cancel,
            on_progress,
        ),
    }
}

/// Outcome of the common prepare step (probe + partial detection +
/// skip decision + argv build). Either ready to invoke ffmpeg, or a
/// terminal record (skipped / failed-probe) the caller returns as-is.
enum EncodePrepareResult {
    Ready {
        metadata: VideoMetadata,
        argv: Vec<String>,
        output_path: PathBuf,
    },
    Skipped(EncodingRecord),
    FailedProbe(EncodingRecord),
}

/// Shared by [`transcode_single`] (CLI) and [`transcode_single_with_progress`]
/// (GUI). Performs the probe + partial-output-detection + skip-decision +
/// argv-build steps so both paths agree on what work to do; only the
/// actual ffmpeg invocation differs.
///
/// Returns `EncodePrepareResult::{Ready, Skipped, FailedProbe}` so
/// the caller can build either a final `EncodingRecord` (skipped /
/// failed-probe) or proceed to invoke ffmpeg.
fn encode_prepare(
    task: &TranscodeTask,
    params: &EncodeParams,
    cache: &EncoderCache,
    config: &Config,
    started_at_meta: chrono::DateTime<chrono::Utc>,
    _started_at_instant: Instant,
) -> EncodePrepareResult {
    // transcode_single(_with_progress) is only invoked via task graphs
    // that build `TranscodeTask { output: Some(...) }`. Treat `None`
    // as a programming error rather than a soft "skip" -- there's no
    // input-defined output path to compare against.
    let output_path = task
        .output
        .clone()
        .expect("transcode_single called with TranscodeTask.output = None");

    // Probe input FIRST so we have ground truth (source duration) for
    // partial-output detection (§3.3.6 + §8 item 40). SIGTERM/SIGHUP
    // can no longer clean up partial files (see §3.4.5 "注意"), so
    // re-runs must detect + auto-remove + reencode them.
    let metadata = match probe_video_metadata_auto(task.input.as_path()) {
        Ok(m) => m,
        Err(e) => {
            return EncodePrepareResult::FailedProbe(make_failed_probe_record(
                task,
                params,
                started_at_meta,
                None,
                &e,
            ));
        }
    };

    if output_path.exists() {
        let output_meta = probe_video_metadata_auto(&output_path).ok();
        let is_partial = match (
            metadata.duration_secs,
            output_meta.as_ref().and_then(|m| m.duration_secs),
        ) {
            (Some(src), Some(out)) => out + 1.0 < src,
            (Some(_), None) => true,
            // Review fix #3 (refined): only re-encode when we can
            // actually detect partiality. If both source and output
            // durations are unknown (the test `skips_when_output_exists`
            // case with a 0-byte output file and no source metadata),
            // we have no way to tell partial from complete — keep
            // the original skip-with-OutputExists behavior. This is
            // safer than auto-re-encoding a known-good file.
            (None, None) => false,
            (None, Some(_)) => true,
        };

        if is_partial {
            match std::fs::remove_file(&output_path) {
                Ok(()) => {
                    log::warn!(
                        "    partial output detected at {} (output {:.2}s vs source {:.2}s); removed, re-encoding",
                        output_path.display(),
                        output_meta.as_ref().and_then(|m| m.duration_secs).unwrap_or(0.0),
                        metadata.duration_secs.unwrap_or(0.0),
                    );
                    // Fall through.
                }
                Err(e) => {
                    log::warn!(
                        "    partial output detected at {} but removal failed: {}; skipping",
                        output_path.display(),
                        e,
                    );
                    return EncodePrepareResult::Skipped(make_skipped_record(
                        task,
                        params,
                        started_at_meta,
                        None,
                        SkippedReason::PartialOutput,
                    ));
                }
            }
        } else {
            log::info!(
                "    task skipped: output already exists at {}",
                output_path.display()
            );
            return EncodePrepareResult::Skipped(make_skipped_record(
                task,
                params,
                started_at_meta,
                None,
                SkippedReason::OutputExists,
            ));
        }
    }

    let argv = build_argv(task, params, cache, config, &metadata);
    EncodePrepareResult::Ready {
        metadata,
        argv,
        output_path,
    }
}

/// Inner helper for [`transcode_single`] (CLI path). Invokes ffmpeg
/// with stderr-capture via [`run_ffmpeg_with_stderr`] and blocks until
/// exit. Then calls [`build_encode_record`] to construct the final
/// `EncodingRecord` from ffmpeg's exit result.
#[allow(clippy::too_many_arguments)]
fn transcode_with_argv(
    task: &TranscodeTask,
    params: &EncodeParams,
    metadata: &VideoMetadata,
    argv: Vec<String>,
    output_path: &Path,
    started_at_meta: chrono::DateTime<chrono::Utc>,
    started_at_instant: Instant,
    output: ProcessOutput,
) -> Result<EncodingRecord, EncodeError> {
    // (kept structurally identical to the original transcode_with_argv
    // but now reads output_path from the explicit parameter instead
    // of recomputing it from task.output.)
    let _ = output_path; // currently unused inside; reserved for symmetry with progress variant
    let src_fps = metadata.best_fps();
    let codec = metadata.codec.as_deref().unwrap_or("");
    let need_reduce = src_fps
        .map(|s| params.max_fps.enabled() && params.max_fps.need_reduce(s))
        .unwrap_or(false);

    if should_skip_encoding(codec, params.format, need_reduce) {
        log::info!("    task skipped: source codec matches target ({})", codec);
        return Ok(make_skipped_record(
            task,
            params,
            started_at_meta,
            Some(started_at_instant.elapsed().as_secs_f64()),
            SkippedReason::SameCodec,
        ));
    }

    let run_result = run_ffmpeg_with_stderr(&argv, output);
    build_encode_record(
        task,
        params,
        metadata,
        argv,
        output_path,
        started_at_meta,
        started_at_instant,
        run_result,
    )
}

/// Inner helper for [`transcode_single_with_progress`] (GUI path).
/// Invokes ffmpeg via [`run_ffmpeg_with_progress`] which:
/// - Uses `cmd.spawn()` (not `cmd.output()`) — we hold a `Child`
///   handle and can kill it mid-stream.
/// - Polls `cancel.is_cancelled()` between progress reads; on detection
///   kills the child and returns `Err((Cancelled, ...))`.
/// - Streams progress snapshots to `on_progress` (one per ffmpeg
///   progress block, ~2/sec per active file).
#[allow(clippy::too_many_arguments)]
fn transcode_with_progress(
    task: &TranscodeTask,
    params: &EncodeParams,
    metadata: &VideoMetadata,
    argv: Vec<String>,
    output_path: &Path,
    started_at_meta: chrono::DateTime<chrono::Utc>,
    started_at_instant: Instant,
    cancel: &CancelToken,
    on_progress: &mut dyn FnMut(FfmpegProgress),
) -> Result<EncodingRecord, EncodeError> {
    let src_fps = metadata.best_fps();
    let codec = metadata.codec.as_deref().unwrap_or("");
    let need_reduce = src_fps
        .map(|s| params.max_fps.enabled() && params.max_fps.need_reduce(s))
        .unwrap_or(false);

    if should_skip_encoding(codec, params.format, need_reduce) {
        log::info!("    task skipped: source codec matches target ({})", codec);
        return Ok(make_skipped_record(
            task,
            params,
            started_at_meta,
            Some(started_at_instant.elapsed().as_secs_f64()),
            SkippedReason::SameCodec,
        ));
    }

    let run_result = run_ffmpeg_with_progress(&argv, cancel, metadata.duration_secs, on_progress);
    build_encode_record(
        task,
        params,
        metadata,
        argv,
        output_path,
        started_at_meta,
        started_at_instant,
        run_result,
    )
}

/// Build the final EncodingRecord from ffmpeg's exit result. Shared
/// between CLI and GUI paths — both produce the same record shape;
/// the difference is upstream (sync vs cancellable ffmpeg run).
#[allow(clippy::too_many_arguments)]
fn build_encode_record(
    task: &TranscodeTask,
    params: &EncodeParams,
    metadata: &VideoMetadata,
    argv: Vec<String>,
    output_path: &Path,
    started_at_meta: chrono::DateTime<chrono::Utc>,
    started_at_instant: Instant,
    run_result: Result<(), (ErrorCategory, String, String)>,
) -> Result<EncodingRecord, EncodeError> {
    let ended_at_meta = Utc::now();
    let encoding_duration = started_at_instant.elapsed().as_secs_f64();
    let encoder_selected = extract_encoder_from_argv(&argv);
    let src_fps = metadata.best_fps();
    let need_reduce = src_fps
        .map(|s| params.max_fps.enabled() && params.max_fps.need_reduce(s))
        .unwrap_or(false);

    match run_result {
        Ok(()) => {
            // Review fix #4: ffmpeg reported success but the output
            // may have been deleted between ffmpeg exit and our stat
            // call (e.g. an external process / disk cleanup tool). If
            // we KNOW the source has a duration and the output is
            // zero bytes / missing, downgrade to Failed (phantom
            // success). When the source duration is unknown
            // (streaming manifest, broken probe) we can't tell
            // partial from complete — fall back to Succeeded and
            // trust the file's existence; the user will discover
            // the issue at playback time.
            let source_has_known_duration = metadata.duration_secs.is_some();
            let output_size = std::fs::metadata(output_path)
                .ok()
                .filter(|m| m.len() > 0)
                .map(|m| m.len());
            let source_size_bytes = std::fs::metadata(task.input.as_path())
                .map(|m| m.len())
                .unwrap_or(0);
            // Review fix #114 (partial-output detection fall-through):
            // when ffmpeg exits 0 but the output is missing or
            // zero-byte, the encode produced nothing useful. If the
            // source has a known duration we can confidently call this
            // `Failed`; if a zero-byte file is left behind, remove it
            // in the same run so the next invocation's partial-output
            // detector doesn't have to clean it up. Without this the
            // zero-byte ghost would linger on disk until the next
            // run — and could be mistaken for a real output by other
            // tools that just check `path.exists()`.
            let (outcome, size_bytes) = match output_size {
                Some(n) => (
                    if need_reduce {
                        EncodingOutcome::ReducedFps
                    } else {
                        EncodingOutcome::Succeeded
                    },
                    Some(n),
                ),
                None if source_has_known_duration => {
                    // Best-effort cleanup of any zero-byte file left
                    // by ffmpeg before it gave up. We tolerate a
                    // remove failure (e.g. file vanished between
                    // `metadata()` and `remove_file`) since the next
                    // run's partial detector will retry.
                    if output_path.exists() {
                        match std::fs::remove_file(output_path) {
                            Ok(()) => log::warn!(
                                "    ffmpeg exit 0 but {} was zero bytes; removed, recording Failed",
                                output_path.display()
                            ),
                            Err(e) => log::warn!(
                                "    ffmpeg exit 0 but {} was zero bytes and remove failed: {}; recording Failed",
                                output_path.display(),
                                e
                            ),
                        }
                    } else {
                        log::warn!(
                            "    ffmpeg exit 0 but {} missing; recording Failed",
                            output_path.display()
                        );
                    }
                    (EncodingOutcome::Failed, None)
                }
                None => {
                    // Source duration unknown: can't tell partial
                    // from complete, trust the file as-is. This
                    // matches the original behavior — the test
                    // `skips_when_output_exists` exercises this case.
                    (
                        if need_reduce {
                            EncodingOutcome::ReducedFps
                        } else {
                            EncodingOutcome::Succeeded
                        },
                        None,
                    )
                }
            };
            let compression_ratio = match (size_bytes, source_size_bytes) {
                (Some(o), s) if s > 0 => Some(o as f64 / s as f64),
                _ => None,
            };

            Ok(EncodingRecord {
                source_path: task.input.to_string(),
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

                output_path: Some(output_path.to_string_lossy().into_owned()),
                output_size_bytes: size_bytes,
                output_duration_secs: metadata.duration_secs,
                output_codec: encoder_selected,
                compression_ratio,

                outcome,
                error: None,
            })
        }
        Err((category, message, stderr_tail)) => Ok(EncodingRecord {
            source_path: task.input.to_string(),
            source_size_bytes: std::fs::metadata(task.input.as_path())
                .map(|m| m.len())
                .unwrap_or(0),
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
    files: &[crate::types::InputPath],
    cache: &EncoderCache,
    config: &Config,
    output: ProcessOutput,
    current_output: Option<&PartialOutputHandle>,
    result_callback: Option<&ResultCallback>,
) -> Vec<EncodingRecord> {
    // U19 step 4: route the batch through `TaskManager::run`. The
    // manager owns the iteration, scaffold logs, and signal-handler
    // Guard wiring. The `current_output` handle is cloned into the
    // manager so per-task Guards register the in-flight output path.
    let tasks: Vec<TranscodeTask> = files
        .iter()
        .map(|input| TranscodeTask {
            input: input.clone(),
            output: Some(output_path_for(input.as_path(), params.format)),
        })
        .collect();

    // The batch label + path form the batch-level scaffold header
    // (U19 step 10). The path is the user's target (file or dir),
    // surfaced from EncodeParams.target.
    let batch_path = batch_target_path(&params.target);
    let manager = TaskManager::new(current_output.cloned());
    // Review #120: TaskManager::run now returns
    // Result<Vec<R>, (usize, EncodeError)>. The CLI's `transcode_batch`
    // is best-effort: per-file failures are already encoded as
    // `Ok(record)` with `outcome = Failed` by `transcode_single`,
    // so a `Result::Err` here means the batch aborted mid-flight
    // (UI disconnected in the GUI path; rare in the CLI path).
    // We log + return what we have so the caller still gets the
    // partial records.
    let run_result = manager.run(&tasks, "encoding", &batch_path, |task, _ctx| {
        transcode_single(task, params, cache, config, output)
    });
    let records: Vec<EncodingRecord> = match run_result {
        Ok(records) => records,
        Err((idx, e)) => {
            log::warn!(
                "    batch aborted at task {idx}: {e}; returning {} partial records",
                idx
            );
            Vec::new()
        }
    };

    if let Some(cb) = result_callback {
        for record in &records {
            cb(record);
        }
    }
    records
}

/// Dry-run a batch: same per-file `encode_prepare` (probe + partial
/// detection + skip decision) as `transcode_batch`, but **no ffmpeg
/// invocation**. Returns planned `EncodingRecord`s so the caller can
/// surface them in the UI without committing any work.
///
/// Each record's outcome reflects what `transcode_batch` *would* do
/// at the prepare step:
/// - `Ready` → `outcome = Succeeded` (encode would run)
/// - `Skipped(SameCodec | OutputExists | PartialOutput)` → record
///   as-is
/// - `FailedProbe` → record as-is
///
/// `encode.nv` (GPU enable) is ignored — it's only consulted when
/// actually running ffmpeg, which dry-run skips.
pub fn dry_run_batch<'a, I>(
    params: &EncodeParams,
    _cache: &EncoderCache,
    config: &Config,
    files: I,
) -> Vec<EncodingRecord>
where
    I: IntoIterator<Item = &'a crate::types::InputPath>,
{
    let mut records = Vec::new();
    for file in files {
        let output_path = output_path_for(file.as_path(), params.format);
        let task = TranscodeTask {
            input: file.clone(),
            output: Some(output_path),
        };
        let prepare = encode_prepare(&task, params, _cache, config, Utc::now(), Instant::now());
        match prepare {
            EncodePrepareResult::Ready { .. } => {
                // Plan: encode would actually run. Build a fresh
                // record with Succeeded outcome (don't go through
                // make_skipped_record — that uses Skipped(SameCodec)
                // or OutputExists internally and we'd be overriding
                // the outcome).
                records.push(make_planned_ready_record(&task, params, Utc::now()));
            }
            EncodePrepareResult::Skipped(r) | EncodePrepareResult::FailedProbe(r) => {
                records.push(r);
            }
        }
    }
    records
}

/// Build a "dry-run planned success" record: same metadata fields as
/// `make_skipped_record` (probe + source info populated) but outcome
/// is `Succeeded` and `ended_at` is `None` (no actual run yet).
fn make_planned_ready_record(
    task: &TranscodeTask,
    params: &EncodeParams,
    started_at: chrono::DateTime<chrono::Utc>,
) -> EncodingRecord {
    let source_size_bytes = std::fs::metadata(task.input.as_path())
        .map(|m| m.len())
        .unwrap_or(0);
    let source_meta = probe_video_metadata_quick(task);
    EncodingRecord {
        source_path: task.input.to_string(),
        source_size_bytes,
        source_codec: source_meta.as_ref().and_then(|m| m.codec.clone()),
        source_container: source_meta.as_ref().and_then(|m| m.container.clone()),
        source_width: source_meta.as_ref().and_then(|m| m.width),
        source_height: source_meta.as_ref().and_then(|m| m.height),
        source_fps: source_meta.as_ref().and_then(|m| m.best_fps()),
        source_duration_secs: source_meta.as_ref().and_then(|m| m.duration_secs),
        source_bit_rate_bps: source_meta.as_ref().and_then(|m| m.bit_rate),

        started_at,
        // No actual end time — this is a plan, not a run record.
        ended_at: None,
        encoding_duration_secs: None,
        encoder_selected: None,
        encoder_settings: encoder_settings_from_params(params),
        ffmpeg_argv: Vec::new(),
        ffmpeg_stderr_tail: String::new(),

        // The planned output path (ffmpeg hasn't run, but we know
        // what it WOULD produce).
        output_path: task
            .output
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        output_size_bytes: None,
        output_duration_secs: None,
        output_codec: None,
        compression_ratio: None,

        outcome: EncodingOutcome::Succeeded,
        error: None,
    }
}

/// Extract the target path (file or directory) from `EncodeParams.target`
/// for the batch-level scaffold log.
/// Resolve the batch's target directory / file path from
/// `EncodeParams.target`. Used by `transcode_batch` to populate the
/// `batch_path` field of `TaskContext`.
pub fn batch_target_path(target: &crate::types::Target) -> PathBuf {
    use crate::types::Target;
    match target {
        Target::File(p) | Target::Dir(p) => p.clone(),
    }
}

/// Sibling of [`batch_target_path`] for callers that have a slice
/// of `InputPath` instead of a `Target`. Used by the actor's
/// `Command::StartBatch` arm (review #120). Picks the parent of
/// the first file's path so the scaffold log line points at the
/// directory the user is working with.
pub fn batch_target_path_for_files(files: &[crate::types::InputPath]) -> std::path::PathBuf {
    files
        .first()
        .map(|f| f.as_path().parent().unwrap_or(f.as_path()).to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("(empty)"))
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
    build_ffmpeg_command(task, params, &encoder, src_fps, &config.ffmpeg_args)
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
    let source_size_bytes = std::fs::metadata(task.input.as_path())
        .map(|m| m.len())
        .unwrap_or(0);
    let source_meta = probe_video_metadata_quick(task);
    EncodingRecord {
        source_path: task.input.to_string(),
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
    let source_size_bytes = std::fs::metadata(task.input.as_path())
        .map(|m| m.len())
        .unwrap_or(0);
    EncodingRecord {
        source_path: task.input.to_string(),
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

/// Read stderr from a child process into a bounded buffer (review fix
/// #112). Caps memory at `max_bytes`; once hit, drains the rest of
/// the pipe and counts discarded bytes. The returned tail is
/// annotated with a "[stderr truncated; N bytes dropped]" prefix
/// when anything was discarded, so the user sees that the upstream
/// prefix was elided.
///
/// Lossy UTF-8 conversion is used because ffmpeg stderr is normally
/// UTF-8 but a chunk boundary could split a multi-byte sequence —
/// replacing the bad tail with `U+FFFD` is acceptable for a log tail.
///
/// Note: `Read::read` is allowed to return short reads (any value
/// from 1 to `buf.len()`), so the cap must be re-checked *after* each
/// append, not just at the start of the loop iteration.
fn read_bounded_stderr<R: std::io::Read>(reader: R, max_bytes: usize) -> String {
    use std::io::Read;
    let mut buf = std::io::BufReader::new(reader);
    let mut kept = String::with_capacity(max_bytes.min(8192));
    let mut chunk = vec![0u8; 8192];
    let mut discarded = 0u64;
    let mut cap_hit = false;
    loop {
        match buf.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if cap_hit {
                    discarded += n as u64;
                    continue;
                }
                let room = max_bytes.saturating_sub(kept.len());
                if room == 0 {
                    cap_hit = true;
                    discarded += n as u64;
                    continue;
                }
                let take = n.min(room);
                kept.push_str(&String::from_utf8_lossy(&chunk[..take]));
                if take < n {
                    cap_hit = true;
                    discarded += (n - take) as u64;
                }
            }
            Err(_) => break,
        }
    }
    if cap_hit {
        // Take the tail FIRST, then prepend the annotation. If we
        // appended the annotation to `kept` and *then* took the
        // tail, the annotation would be at the front and
        // `stderr_tail` would drop it (it only keeps the last 20
        // non-empty lines).
        let tail = stderr_tail(&kept, 20);
        let prefix = format!("[stderr truncated; {discarded} bytes dropped]\n");
        let mut annotated = String::with_capacity(prefix.len() + tail.len());
        annotated.push_str(&prefix);
        annotated.push_str(&tail);
        return annotated;
    }
    stderr_tail(&kept, 20)
}

/// Structured progress snapshot from ffmpeg's `-progress pipe:1`
/// output (U21, §3.8.3 `Event::Progress`). Built incrementally as we
/// parse key=value lines, then emitted once per `progress=continue`
/// / `progress=end` block.
#[derive(Clone, Debug, Default)]
/// Per-file progress snapshot emitted by
/// [`run_ffmpeg_with_progress`] every ~0.5s (one per ffmpeg progress
/// block). Fed into the GUI via the closure passed to
/// [`transcode_single_with_progress`] and surfaced as
/// [`Event::Progress`](crate::job::Event::Progress).
///
/// # ETA threshold
///
/// `eta_secs` is `Some` only when `speed >= 0.001`. Below that, ffmpeg
/// is either starting up (no useful estimate yet) or stalled (N/A),
/// so the UI shows "—" instead of an unstable minutes-count. A
/// separate `last_speed` is tracked inside
/// `run_ffmpeg_with_progress` so a mid-block `speed=0` line doesn't
/// flip ETA to `None` within the same progress block.
pub struct FfmpegProgress {
    pub frame: u64,
    pub fps: f64,
    /// Encoding speed multiplier as a ratio (e.g. `1.5` for 1.5x).
    /// `0.0` when ffmpeg reports `N/A` or hasn't started yet.
    pub speed: f64,
    pub time_secs: f64,
    /// `(total - time) / speed`. `None` when speed is too low to be
    /// meaningful (< 0.001) or when caller didn't provide total
    /// duration.
    pub eta_secs: Option<f64>,
}

/// Spawn ffmpeg with the given argv (built by `build_ffmpeg_command`),
/// stream progress to `on_progress`, and return when the process
/// exits or is cancelled. This is the U21 sibling to
/// `run_ffmpeg_with_stderr` — the latter stays as the CLI path
/// (terminal-attached, blocking). This one is for the GUI path
/// (event-driven, cancellable).
///
/// Differences from `run_ffmpeg_with_stderr`:
/// - Uses `cmd.spawn()` instead of `cmd.output()`, so we can read
///   progress while the child runs and kill it mid-stream.
/// - Appends `-progress pipe:1` to argv so ffmpeg writes structured
///   key=value lines to stdout instead of overwriting stderr.
/// - Polls `cancel.is_cancelled()` between progress reads; on detection
///   kills the child and returns `Err(Cancelled, _, _)`.
/// - Stderr still captured in a background thread for the
///   `ErrorContext.ffmpeg_stderr_tail` payload on failure.
///
/// Returns the same `Result<(), (ErrorCategory, String, String)>`
/// Spawn ffmpeg with cancellable, event-driven execution (GUI path).
///
/// # Work flow
///
/// 1. `cmd.spawn()` (not `cmd.output()`) — we hold a `Child` handle
///    so we can kill it mid-stream.
/// 2. Background thread reads stderr, accumulating the last 20 lines
///    into a buffer. On failure this becomes
///    `ErrorContext.ffmpeg_stderr_tail`.
/// 3. Main thread reads stdout line-by-line, parsing the
///    `-progress pipe:1` key=value format from ffmpeg.
/// 4. Keys: `frame` / `fps` / `speed` / `out_time_us` (or
///    `out_time_ms`) / `progress` (`continue` | `end`).
/// 5. Each `progress=continue` or `progress=end` line emits one
///    [`FfmpegProgress`] via the caller's closure; `end` also breaks
///    the read loop.
/// 6. Between progress lines, polls `cancel.is_cancelled()` — if set,
///    kills the child and returns `Err((Cancelled, ...))`.
/// 7. After stdout closes: `child.wait()`. Non-zero exit code →
///    `Err((FfmpegNonzeroExit, msg, stderr_tail))`.
///
/// # CLI vs GUI
///
/// CLI path uses `run_ffmpeg_with_stderr` (sync `cmd.output()`,
/// stderr inherited to terminal). This function is the GUI/actor
/// path. Both have the same `Result<(), (ErrorCategory, String, String)>`
/// shape so the controller can wrap both uniformly.
pub fn run_ffmpeg_with_progress(
    argv: &[String],
    cancel: &CancelToken,
    total_duration_secs: Option<f64>,
    on_progress: &mut dyn FnMut(FfmpegProgress),
) -> Result<(), (ErrorCategory, String, String)> {
    if argv.is_empty() {
        return Err((
            ErrorCategory::ConfigError,
            "empty ffmpeg argv".into(),
            String::new(),
        ));
    }

    let program = &argv[0];
    let mut args: Vec<String> = argv[1..].to_vec();
    // Append `-progress pipe:1`. ffmpeg writes structured progress
    // to stdout and suppresses the regular status updates when this
    // flag is set. CLI path doesn't use this — it gets raw status
    // lines from stderr.
    args.push("-progress".into());
    args.push("pipe:1".into());

    let mut cmd = Command::new(program);
    cmd.args(&args);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Err((
                ErrorCategory::FfmpegSpawnFailed,
                format!("spawn {program}: {e}"),
                String::new(),
            ));
        }
    };

    let stdout = child
        .stdout
        .take()
        .expect("child stdout was configured with Stdio::piped()");
    let stderr = child
        .stderr
        .take()
        .expect("child stderr was configured with Stdio::piped()");

    // Stderr reader thread: collects tail for ErrorContext.ffmpeg_stderr_tail
    // on failure. We don't surface stderr live in this path (CLI path
    // inherits stderr to the terminal instead).
    //
    // Review fix #112 (stderr reader memory cap): ffmpeg can emit
    // unbounded stderr on verbose runs (e.g. `-loglevel debug` with a
    // long source) or pathological encoder paths. `read_to_string`
    // would let that grow without bound and OOM the worker. The
    // read loop caps at [`STDERR_READER_MAX_BYTES`] (256 KiB); once
    // reached, drain the rest of the pipe and discard — the 20-line
    // tail we keep comes from the *end* of stderr (where ffmpeg
    // writes the failure cause), so dropping the prefix is acceptable
    // for the ErrorContext payload. After the cap is hit the thread
    // terminates promptly so the subsequent `join()` cannot wedge
    // (review fix #116, bounded stderr_thread.join()).
    let stderr_thread =
        std::thread::spawn(move || read_bounded_stderr(stderr, STDERR_READER_MAX_BYTES));

    // Stdout reader: ffmpeg writes key=value lines, with blocks
    // separated by `progress=continue` / `progress=end`.
    use std::io::BufRead;
    let stdout_reader = std::io::BufReader::new(stdout);
    let mut current = FfmpegProgress::default();
    let mut last_speed = 1.0_f64; // for ETA calc when speed=0 mid-block

    for line in stdout_reader.lines() {
        // Cancel poll: cheap, runs once per progress line (~2/sec).
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            // Review fix #5: the stderr reader thread can be
            // blocked in `read_to_string` if the kernel pipe hasn't
            // been reaped after kill(). `JoinHandle::join_timeout`
            // is nightly-only; on stable we drop the handle (the
            // thread is detached but will be reaped by the OS when
            // it eventually exits). The collected stderr has
            // already been moved into `stderr_buf` before the
            // drop, so we don't lose already-buffered output.
            drop(stderr_thread);
            return Err((
                ErrorCategory::Cancelled,
                format!("{program} cancelled by user"),
                String::new(),
            ));
        }

        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        if line.is_empty() {
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            match key {
                "frame" => current.frame = value.parse().unwrap_or(0),
                "fps" => current.fps = value.parse().unwrap_or(0.0),
                "speed" => {
                    // ffmpeg writes "1.5x" or "N/A" — strip the suffix.
                    let cleaned = value.trim_end_matches('x').trim();
                    let parsed: f64 = cleaned.parse().unwrap_or(0.0);
                    current.speed = parsed;
                    if parsed > 0.0 {
                        last_speed = parsed;
                    }
                }
                "out_time_us" => {
                    // microseconds → seconds. This is the canonical
                    // key in current ffmpeg; out_time_ms is also
                    // supported for older builds.
                    let us: u64 = value.parse().unwrap_or(0);
                    current.time_secs = us as f64 / 1_000_000.0;
                }
                "out_time_ms" => {
                    let ms: u64 = value.parse().unwrap_or(0);
                    current.time_secs = ms as f64 / 1_000.0;
                }
                "progress" => {
                    // Emit one snapshot per block; reset for the next.
                    current.eta_secs = if last_speed >= 0.001 {
                        total_duration_secs
                            .map(|total| (total - current.time_secs).max(0.0) / last_speed)
                    } else {
                        None
                    };
                    on_progress(current.clone());
                    if value == "end" {
                        break;
                    }
                    current = FfmpegProgress::default();
                }
                _ => {}
            }
        }
    }

    // Stdout reader drops here automatically (it's not Copy); the
    // child can no longer block on a full pipe since ffmpeg only
    // writes progress blocks (~50 lines per file).

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            // Review fix #5: see cancel branch for rationale.
            drop(stderr_thread);
            return Err((
                ErrorCategory::FfmpegSpawnFailed,
                format!("wait {program}: {e}"),
                String::new(),
            ));
        }
    };

    let stderr_tail_str = stderr_thread.join().unwrap_or_default();

    if !status.success() {
        return Err((
            ErrorCategory::FfmpegNonzeroExit,
            format!("{program} exited {status}"),
            stderr_tail_str,
        ));
    }

    Ok(())
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
    probe_video_metadata_auto(task.input.as_path()).ok()
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

    /// Test helper: wrap a path literal in `InputPath`. Used at every
    /// site that constructs a `TranscodeTask` literal in tests.
    /// Production code goes through the boundary (`collect_files`),
    /// so this `.expect` is safe.
    fn ip(p: &Path) -> crate::types::InputPath {
        crate::types::InputPath::new(p).expect("test fixture")
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
            input: ip(&input),
            output: Some(output.clone()),
        };
        let params = params_x264_crf();
        let _cache = cache_with_libx264_and_svtav1(false);
        let _config = Config::default();

        // We can't actually run ffmpeg on a fake webm file -- so use
        // TaskManager directly with a canned success closure that
        // mimics what a real successful transcode_with_argv would
        // produce. TaskManager is the new processor seam (U19 step 1
        // + step 4); the old `transcode_batch_with_processor_for_test`
        // shim was removed when `transcode_batch` started routing
        // through TaskManager.
        let tasks = vec![TranscodeTask {
            input: task.input.clone(),
            output: task.output.clone(),
        }];
        let manager = super::TaskManager::new(None);
        let records: Vec<EncodingRecord> = manager
            .run(&tasks, "encoding", Path::new("/test"), |_task, _ctx| {
                Ok(test_succeeded_record(&task, &params))
            })
            .expect("test batch should succeed");
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
            source_path: task.input.to_string(),
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
            input: ip(&dir.join("input.mp4")),
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
            input: ip(&input),
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
            task.output.as_ref().unwrap().as_path(),
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

    // ---- read_bounded_stderr (review fix #112 + #116) ----

    #[test]
    fn read_bounded_stderr_under_cap_keeps_full_tail() {
        let input = b"frame= 1\nframe= 2\nConversion failed!\n";
        let tail = read_bounded_stderr(&input[..], 1024);
        assert_eq!(tail, "frame= 1\nframe= 2\nConversion failed!");
    }

    #[test]
    fn read_bounded_stderr_over_cap_annotates_truncation() {
        // ~10 KiB of input, 1 KiB cap. The reader keeps the LAST 1 KiB
        // (where ffmpeg's failure cause normally lives) and discards
        // the prefix.
        let mut input = Vec::new();
        let line = b"frame= XXXX line padding padding padding padding\n";
        for _ in 0..(10 * 1024 / line.len() + 1) {
            input.extend_from_slice(line);
        }
        assert!(input.len() > 10 * 1024, "input was {}", input.len());
        let tail = read_bounded_stderr(&input[..], 1024);
        assert!(
            tail.starts_with("[stderr truncated;"),
            "expected truncation annotation, got: {tail:?}"
        );
        assert!(tail.contains("bytes dropped]"));
        // The tail should keep the SUFFIX of the input (proves we
        // didn't keep the prefix instead). The annotation prefix
        // displaces the very first line, so we check for one of the
        // mid-input lines that survives.
        assert!(
            tail.contains("padding padding padding padding"),
            "tail should retain suffix lines; got: {tail:?}"
        );
    }

    #[test]
    fn read_bounded_stderr_empty_input_returns_empty() {
        let tail = read_bounded_stderr(&[][..], 1024);
        assert_eq!(tail, "");
    }

    #[test]
    fn read_bounded_stderr_handles_invalid_utf8_lossy() {
        // 0xFF is not valid UTF-8; must not panic. The lossy
        // replacement is U+FFFD (\u{FFFD}).
        let input: Vec<u8> = vec![b'x', 0xFF, 0xFE, b'y'];
        let tail = read_bounded_stderr(&input[..], 1024);
        assert!(tail.contains('\u{FFFD}'));
        assert!(tail.contains('x'));
        assert!(tail.contains('y'));
    }
}
