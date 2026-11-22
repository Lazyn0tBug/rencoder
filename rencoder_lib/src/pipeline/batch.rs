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

use crate::data::extract::probe_video_metadata_auto;
use crate::data::record::encoder_settings_from_params;
use crate::encoder::{command::build_ffmpeg_command, nvenc, select::select_encoder};
use crate::error::EncodeError;
use crate::pipeline::cancel::CancelToken;
use crate::pipeline::context::{BatchContext, FilePlan};
use crate::pipeline::task::BatchReport;
use crate::pipeline::task::TaskManager;
use crate::types::{
    EncodeParams, EncodingOutcome, EncodingRecord, ErrorCategory, ErrorContext, FpsDecision,
    ProcessOutput, TranscodeTask, VideoMetadata,
};

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
/// No `task_ctx` param. Lifecycle logging lives in
/// [`crate::pipeline::task::TaskManager::run`]; `transcode_single`
/// focuses on encode. Callers (CLI / tests) wrap the call via
/// `TaskManager` when they need scaffold logs.
///
/// `cancel` is the run's cancel token: the ffmpeg child is killed and
/// reaped when it is set (design doc §3.11.2).
pub fn transcode_single(
    task: &TranscodeTask,
    ctx: &mut BatchContext,
    output: ProcessOutput,
    cancel: &CancelToken,
) -> Result<EncodingRecord, EncodeError> {
    let started_at_meta = Utc::now();
    let started_at_instant = Instant::now();

    // CLI path. Shared prepare logic + sync ffmpeg execution.
    let prepare = encode_prepare(task, ctx, started_at_meta, started_at_instant);
    match prepare {
        EncodePrepareResult::Failed(r) => Ok(*r),
        EncodePrepareResult::Ready(plan) => transcode_with_argv(task, plan, ctx, output, cancel),
    }
}

/// "Encode one file" -- the step the GUI actor's tests replace.
///
/// The seam sits **above** the prepare step (it replaces the whole
/// per-file work), so a test double never has to provide a real media
/// file: it never probes, never builds an argv, never spawns ffmpeg.
/// The events around it (`FileStarted` / `Progress` / `FileCompleted`)
/// are emitted by the shared code, so the message protocol has exactly
/// one implementation (design doc §3.10.4).
///
/// `&self`, not `&mut self`: the actor shares one encoder across
/// batches, so the seam has to be shareable. A test double keeps its
/// state behind an `Arc<AtomicUsize>` or similar.
pub trait FileEncoder: Send + Sync + 'static {
    /// Encode one file, reporting progress through `on_progress`.
    fn encode(
        &self,
        task: &TranscodeTask,
        ctx: &mut BatchContext,
        cancel: &CancelToken,
        on_progress: &mut dyn FnMut(FfmpegProgress),
    ) -> Result<EncodingRecord, EncodeError>;
}

/// The production [`FileEncoder`]: encode with ffmpeg, streaming
/// progress and honouring the cancel token.
#[derive(Debug, Default)]
pub struct FfmpegFileEncoder;

impl FileEncoder for FfmpegFileEncoder {
    fn encode(
        &self,
        task: &TranscodeTask,
        ctx: &mut BatchContext,
        cancel: &CancelToken,
        on_progress: &mut dyn FnMut(FfmpegProgress),
    ) -> Result<EncodingRecord, EncodeError> {
        transcode_single_with_progress(task, ctx, cancel, on_progress)
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
    ctx: &mut BatchContext,
    cancel: &CancelToken,
    on_progress: &mut dyn FnMut(FfmpegProgress),
) -> Result<EncodingRecord, EncodeError> {
    let started_at_meta = Utc::now();
    let started_at_instant = Instant::now();

    let prepare = encode_prepare(task, ctx, started_at_meta, started_at_instant);
    match prepare {
        EncodePrepareResult::Failed(r) => Ok(*r),
        EncodePrepareResult::Ready(plan) => {
            transcode_with_progress(task, plan, ctx, cancel, on_progress)
        }
    }
}

/// Outcome of the common prepare step (probe + argv build). Either
/// ready to invoke ffmpeg, or a terminal record the caller returns
/// as-is.
///
/// There is no "skipped" outcome: an existing output is overwritten,
/// not treated as done (design doc §3.3.4 -- re-encoding is the rule).
enum EncodePrepareResult {
    /// Ready to encode: the plan carries the probe result, the argv,
    /// the output path and the start times.
    Ready(FilePlan),
    /// The file cannot be encoded (probe failed, no usable encoder).
    /// Boxed: a record is much bigger than a plan, and this arm is
    /// the rare one.
    Failed(Box<EncodingRecord>),
}

/// Shared by [`transcode_single`] (CLI) and [`transcode_single_with_progress`]
/// (GUI). Performs the probe + partial-output-detection + skip-decision +
/// argv-build steps so both paths agree on what work to do; only the
/// actual ffmpeg invocation differs.
///
/// Returns `EncodePrepareResult::{Ready, Failed}` so the caller can
/// build either a final `EncodingRecord` (failed) or proceed to invoke
/// ffmpeg.
fn encode_prepare(
    task: &TranscodeTask,
    ctx: &BatchContext,
    started_at_meta: chrono::DateTime<chrono::Utc>,
    started_at_instant: Instant,
) -> EncodePrepareResult {
    let params = ctx.params();
    // transcode_single(_with_progress) is only invoked via task graphs
    // that build `TranscodeTask { output: Some(...) }`. Treat `None`
    // as a programming error rather than a soft "skip" -- there's no
    // input-defined output path to compare against.
    let output_path = task
        .output
        .clone()
        .expect("transcode_single called with TranscodeTask.output = None");

    // Same-file guard (design doc §3.3 "同路径保护"). The output must
    // never point at the input: ffmpeg would truncate the source, and
    // the cancel cleanup (which removes `argv.last()`) would delete the
    // input instead of the partial output.
    //
    // `output_path_for` already avoids the obvious collision, but it
    // compares path strings -- on a case-insensitive filesystem
    // (macOS / Windows by default) `movie.MKV` and `movie.mkv` are the
    // same file, and neither ffmpeg nor a string comparison catches it.
    if crate::utils::fs::is_same_file(task.input.as_path(), &output_path) {
        tracing::error!(
            input = %task.input,
            output = %output_path.display(),
            "input and output paths must differ; skipping file"
        );
        return EncodePrepareResult::Failed(Box::new(make_failed_record(
            task,
            params,
            started_at_meta,
            None,
            &EncodeError::InvalidParam("input and output paths must differ".into()),
            ErrorCategory::ConfigError,
        )));
    }

    // Probe the source. A probe failure means the file cannot be
    // encoded (corrupt / unreadable) — a per-file failure, not a
    // reason to abort the batch.
    let metadata = match probe_video_metadata_auto(task.input.as_path()) {
        Ok(m) => m,
        Err(e) => {
            return EncodePrepareResult::Failed(Box::new(make_failed_record(
                task,
                params,
                started_at_meta,
                None,
                &e,
                ErrorCategory::ProbeFailed,
            )));
        }
    };

    // The output may already exist -- we overwrite it. Re-encoding is
    // the rule (design doc §3.3.4): the config is read once per
    // process, so a previous run's output may have been produced with
    // different settings, and the only safe answer is to redo it.
    let argv = match build_argv(task, ctx, &metadata) {
        Ok(argv) => argv,
        Err(e) => {
            // No usable encoder for this format. That is a per-file
            // failure, not a reason to abort the batch (design doc
            // §3.11.4 E1).
            return EncodePrepareResult::Failed(Box::new(make_failed_record(
                task,
                params,
                started_at_meta,
                None,
                &e,
                ErrorCategory::EncoderSelectionFailed,
            )));
        }
    };
    EncodePrepareResult::Ready(FilePlan::new(
        metadata,
        argv,
        output_path,
        started_at_meta,
        started_at_instant,
    ))
}

/// Shared body for the CLI + GUI transcode inner helpers (review #119).
///
/// Both `transcode_with_argv` (CLI, sync) and `transcode_with_progress`
/// (GUI, cancellable) used to repeat the same 10-line skip-check +
/// run-then-build-record boilerplate. This helper extracts the common
/// shape:
///
/// 1. Compute the frame-rate action from `metadata.cap_fps()`.
/// 2. Run ffmpeg (the output is overwritten if it exists).
/// 3. Call the caller-provided `run_ffmpeg` (sync vs cancellable
///    variant) to get a `Result<(), (ErrorCategory, String, String)>`.
/// 4. Delegate to [`build_encode_record`] for the final record.
///
/// Caller-specific behavior (sync ffmpeg.run vs cancellable
/// ffmpeg.run + on_progress streaming) lives in the `run_ffmpeg`
/// closure. The shared record construction + skip policy live here.
fn transcode_inner<F, R>(
    task: &TranscodeTask,
    mut plan: FilePlan,
    ctx: &mut BatchContext,
    mut run_ffmpeg: F,
    rebuild_argv: R,
) -> Result<EncodingRecord, EncodeError>
where
    F: FnMut(Vec<String>) -> Result<(), (ErrorCategory, String, String)>,
    // The rebuild closure re-selects the encoder, so it needs the
    // batch context; it gets it from here (a shared reborrow of the
    // `&mut` this function already owns).
    R: FnOnce(&BatchContext) -> Option<Vec<String>>,
{
    // Clone argv for the closure — build_encode_record needs it too.
    // Per-batch this is one ~20-element Vec<String> clone; trivial.
    let argv_for_run = plan.argv().to_vec();
    let mut run_result = run_ffmpeg(argv_for_run);

    // NVENC fallback. A failed NVENC encode does NOT prove the encoder
    // is unusable -- a full disk, a bad passthrough argument or a
    // missing output directory fails it too, and falling back on those
    // would double the work and hide the real problem. So check the
    // capability for real (a 1-frame test encode) before falling back.
    //
    // The CLI inherits ffmpeg's stderr, so the failure text is not
    // available to tell the two cases apart; the probe is the only
    // decisive check that works on every path.
    let mut fallback_note: Option<FallbackNote> = None;
    if run_result.is_err() {
        if let Some(encoder) = extract_encoder_from_argv(plan.argv()) {
            if nvenc::is_nvenc_encoder(&encoder)
                && !nvenc::verify_capability(ctx.capability_mut(), &encoder)
            {
                let (_, message, stderr_tail) = run_result.clone().unwrap_err();
                tracing::warn!(
                    encoder = %encoder,
                    error = %message,
                    stderr = %stderr_tail,
                    "NVENC encoder is not usable on this machine; falling back to software encoding"
                );
                fallback_note = Some(FallbackNote {
                    encoder: encoder.clone(),
                    message: message.clone(),
                    stderr_tail: stderr_tail.clone(),
                });
                if let Some(fallback_argv) = rebuild_argv(&*ctx) {
                    if !fallback_argv.is_empty() {
                        plan.set_argv(fallback_argv);
                        run_result = run_ffmpeg(plan.argv().to_vec());
                        if let Err((_, message, _)) = &run_result {
                            tracing::warn!(
                                encoder = %encoder,
                                error = %message,
                                "software fallback also failed"
                            );
                        }
                    }
                }
            }
        }
    }

    build_encode_record(task, plan, ctx, run_result, fallback_note)
}

/// What the first (NVENC) attempt left behind, when a software retry
/// replaced it. Carried into the record so the fallback is visible
/// after the fact, not only in the log.
struct FallbackNote {
    /// The encoder that failed.
    encoder: String,
    /// Its error message.
    message: String,
    /// Its stderr tail.
    stderr_tail: String,
}

impl FallbackNote {
    /// One-line summary, for a record's error message.
    fn summary(&self) -> String {
        format!("NVENC {} failed: {}", self.encoder, self.message)
    }

    /// Multi-line text, for a record's stderr tail.
    fn detail(&self) -> String {
        let mut text = format!("[nvenc fallback] {}", self.summary());
        if !self.stderr_tail.is_empty() {
            text.push('\n');
            text.push_str(&self.stderr_tail);
        }
        text
    }
}

/// Inner helper for [`transcode_single`] (CLI path). Invokes ffmpeg
/// with stderr-capture via [`run_ffmpeg_with_stderr`] and blocks until
/// exit. Then calls [`build_encode_record`] to construct the final
/// `EncodingRecord` from ffmpeg's exit result.
///
/// Review #119: thin wrapper around `transcode_inner` that supplies
/// the sync ffmpeg runner.
fn transcode_with_argv(
    task: &TranscodeTask,
    plan: FilePlan,
    ctx: &mut BatchContext,
    output: ProcessOutput,
    cancel: &CancelToken,
) -> Result<EncodingRecord, EncodeError> {
    let metadata = plan.metadata().clone();
    transcode_inner(
        task,
        plan,
        ctx,
        |argv| run_ffmpeg_with_stderr(&argv, output, cancel),
        |ctx| rebuild_argv_after_nvenc_failure(task, ctx, &metadata),
    )
}

/// Build a fresh argv after an NVENC failure: the encoder selection now
/// skips the encoder that just failed, so this picks the software one.
fn rebuild_argv_after_nvenc_failure(
    task: &TranscodeTask,
    ctx: &BatchContext,
    metadata: &VideoMetadata,
) -> Option<Vec<String>> {
    // A failed rebuild is not fatal here: the caller keeps the argv it
    // already has (and reports the original failure).
    match build_argv(task, ctx, metadata) {
        Ok(argv) if !argv.is_empty() => Some(argv),
        _ => None,
    }
}
// ^ argv moves into the closure; build_encode_record uses a fresh
// argv passed by the caller. The closure's FnOnce captures argv
// exactly once.

/// Inner helper for [`transcode_single_with_progress`] (GUI path).
/// Invokes ffmpeg via [`run_ffmpeg_with_progress`] which:
/// - Uses `cmd.spawn()` (not `cmd.output()`) — we hold a `Child`
///   handle and can kill it mid-stream.
/// - Polls `cancel.is_cancelled()` between progress reads; on detection
///   kills the child and returns `Err((Cancelled, ...))`.
/// - Streams progress snapshots to `on_progress` (one per ffmpeg
///   progress block, ~2/sec per active file).
///
/// Review #119: thin wrapper around `transcode_inner` that supplies
/// the cancellable ffmpeg runner with on_progress streaming.
fn transcode_with_progress(
    task: &TranscodeTask,
    plan: FilePlan,
    ctx: &mut BatchContext,
    cancel: &CancelToken,
    on_progress: &mut dyn FnMut(FfmpegProgress),
) -> Result<EncodingRecord, EncodeError> {
    let metadata = plan.metadata().clone();
    transcode_inner(
        task,
        plan,
        ctx,
        |argv| run_ffmpeg_with_progress(&argv, cancel, metadata.duration_secs, on_progress),
        |ctx| rebuild_argv_after_nvenc_failure(task, ctx, &metadata),
    )
}

/// Remove the output a failed attempt wrote.
///
/// Only when ffmpeg actually wrote it **during this attempt** (the
/// file is newer than the attempt's start): an attempt that failed
/// before opening the output leaves the previous result alone -- it is
/// still a complete product.
///
/// The mtime comparison is second-granular, so a file written in the
/// same second as the attempt's start counts as "not touched" -- the
/// safe direction: a leftover half-written file beats deleting a
/// complete one.
fn remove_failed_output(output: &Path, started_at: chrono::DateTime<chrono::Utc>) {
    let Ok(meta) = std::fs::metadata(output) else {
        return;
    };
    let Ok(modified) = meta.modified() else {
        return;
    };
    let modified_secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if modified_secs <= started_at.timestamp() {
        return;
    }
    match std::fs::remove_file(output) {
        Ok(()) => tracing::warn!(
            output = %output.display(),
            "the encode failed; removed the half-written output"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            output = %output.display(),
            error = %e,
            "the encode failed; could not remove the half-written output"
        ),
    }
}

/// Build the final EncodingRecord from ffmpeg's exit result. Shared
/// between CLI and GUI paths — both produce the same record shape;
/// the difference is upstream (sync vs cancellable ffmpeg run).
fn build_encode_record(
    task: &TranscodeTask,
    mut plan: FilePlan,
    ctx: &BatchContext,
    run_result: Result<(), (ErrorCategory, String, String)>,
    fallback_note: Option<FallbackNote>,
) -> Result<EncodingRecord, EncodeError> {
    let params = ctx.params();
    // The argv is moved out first (the record keeps it); the rest of
    // the plan is only read from here on.
    let argv = plan.take_argv();
    let metadata = plan.metadata();
    let output_path = plan.output_path();
    let started_at_meta = plan.started_at_meta();
    let ended_at_meta = Utc::now();
    let encoding_duration = plan.started_at_instant().elapsed().as_secs_f64();
    let encoder_selected = extract_encoder_from_argv(&argv);
    // `source_fps` in the record keeps the average (display semantics,
    // see design doc §2.6); the reduction decision uses the cap value.
    let src_fps = metadata.best_fps();
    let need_reduce = matches!(
        params.max_fps.decide(metadata.cap_fps()),
        FpsDecision::Reduce { .. }
    );

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
                            Ok(()) => tracing::warn!(
                                output = %output_path.display(),
                                "ffmpeg exit 0 but output was zero bytes; removed, recording Failed"
                            ),
                            Err(e) => tracing::warn!(
                                output = %output_path.display(),
                                error = %e,
                                "ffmpeg exit 0 but output was zero bytes and remove failed; recording Failed"
                            ),
                        }
                    } else {
                        tracing::warn!(
                            output = %output_path.display(),
                            "ffmpeg exit 0 but output missing; recording Failed"
                        );
                    }
                    (EncodingOutcome::Failed, None)
                }
                None => {
                    // Source duration unknown: the file is taken
                    // as-is. (Design doc §3.3.4: a file in the batch
                    // is transcribed, full stop -- there is no
                    // partial/complete judgement to make.)
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
                // A fallback leaves a trace here: the file was encoded,
                // but only after the NVENC attempt failed.
                ffmpeg_stderr_tail: fallback_note
                    .as_ref()
                    .map(|note| note.detail())
                    .unwrap_or_default(),

                output_path: Some(output_path.to_string_lossy().into_owned()),
                output_size_bytes: size_bytes,
                output_duration_secs: metadata.duration_secs,
                output_codec: encoder_selected,
                compression_ratio,

                outcome,
                error: None,
            })
        }
        Err((category, message, stderr_tail)) => {
            // A cancel is not a failed file: the batch is stopping, and
            // the report says so (design doc §3.11.2).
            if category == ErrorCategory::Cancelled {
                return Err(EncodeError::UserCancel);
            }
            // The failed attempt may have left a half-written file at
            // the output path. It goes: the target directory keeps
            // complete products only (design doc §3.11.2).
            remove_failed_output(output_path, started_at_meta);
            // When the NVENC attempt was replaced by a software retry,
            // the record has to carry both failures -- otherwise the
            // record blames the software encoder for a hardware
            // problem.
            let (message, stderr_tail) = match &fallback_note {
                Some(note) => {
                    let mut tail = note.detail();
                    if !stderr_tail.is_empty() {
                        tail.push('\n');
                        tail.push_str(&stderr_tail);
                    }
                    (
                        format!("{}; software fallback failed: {message}", note.summary()),
                        tail,
                    )
                }
                None => (message, stderr_tail),
            };
            Ok(EncodingRecord {
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
            })
        }
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
    files: &[crate::types::InputPath],
    ctx: &mut BatchContext,
    output: ProcessOutput,
    cancel: &CancelToken,
    result_callback: Option<&ResultCallback>,
) -> BatchReport<EncodingRecord> {
    // U19 step 4: route the batch through `TaskManager::run`. The
    // Review #122: hoist `format.default_extension()` out of the
    // per-task loop. The extension doesn't change across tasks;
    // computing it once saves one match per file. For a 1000-file
    // batch this is 1000 saved lookups.
    let params = ctx.params();
    let _ext = params.format.default_extension();
    let tasks: Vec<TranscodeTask> = files
        .iter()
        .map(|input| {
            // v1.2 follow-up #3: route through output_path_for
            // for the `_encoded` collision guard (X264/HEVC/AV1 +
            // .mkv input would otherwise overwrite input).
            TranscodeTask {
                input: input.clone(),
                output: Some(crate::utils::fs::output_path_for(
                    input.as_path(),
                    params.format,
                )),
            }
        })
        .collect();

    // The batch label + path form the batch-level scaffold header
    // (U19 step 10). The path is the user's target (file or dir),
    // surfaced from EncodeParams.target.
    let batch_path = batch_target_path(&params.target);
    let manager = TaskManager::new();
    // Review #120: TaskManager::run now returns
    // Result<Vec<R>, (usize, EncodeError)>. The CLI's `transcode_batch`
    // is best-effort: per-file failures are already encoded as
    // `Ok(record)` with `outcome = Failed` by `transcode_single`,
    // so an abort here means the batch stopped mid-flight (cancel,
    // or the UI disconnected). The report keeps the records of the
    // files that finished and says how the batch ended.
    let report = manager.run(&tasks, "encoding", &batch_path, cancel, |task, _ctx| {
        transcode_single(task, ctx, output, cancel)
    });
    if let crate::pipeline::task::Termination::Cancelled { completed, total } = report.termination {
        tracing::warn!(
            task_idx = completed,
            total,
            "batch stopped early; keeping the records that finished"
        );
    }
    if let Some(cb) = result_callback {
        for record in &report.records {
            cb(record);
        }
    }
    report
}

/// Dry-run a batch: same per-file `encode_prepare` (probe + partial
/// detection + skip decision) as `transcode_batch`, but **no ffmpeg
/// invocation**. Returns planned `EncodingRecord`s so the caller can
/// surface them in the UI without committing any work.
///
/// Each record's outcome reflects what `transcode_batch` *would* do
/// at the prepare step:
/// - `Ready` → `outcome = Succeeded` (encode would run)
/// - the file could not be prepared (probe failed) → record
///   as-is
/// - `FailedProbe` → record as-is
///
/// `encode.nv` (GPU enable) is ignored — it's only consulted when
/// actually running ffmpeg, which dry-run skips.
pub fn dry_run_batch<'a, I>(
    ctx: &BatchContext,
    files: I,
    cancel: &CancelToken,
) -> BatchReport<EncodingRecord>
where
    I: IntoIterator<Item = &'a crate::types::InputPath>,
{
    // Review #122: hoist extension out of the per-file loop.
    let params = ctx.params();
    let _ext = params.format.default_extension();
    let files: Vec<&crate::types::InputPath> = files.into_iter().collect();
    let total = files.len();
    let mut records = Vec::new();
    for file in files {
        // v1.2 follow-up #3: route through output_path_for
        // for the `_encoded` collision guard.
        let output_path = crate::utils::fs::output_path_for(file.as_path(), params.format);
        let task = TranscodeTask {
            input: file.clone(),
            output: Some(output_path),
        };
        let prepare = encode_prepare(&task, ctx, Utc::now(), Instant::now());
        match prepare {
            EncodePrepareResult::Ready(plan) => {
                // The plan already carries the probe result — the
                // dry run must not probe the same file twice.
                records.push(make_planned_ready_record(
                    &task,
                    params,
                    plan.metadata(),
                    plan.started_at_meta(),
                ));
            }
            EncodePrepareResult::Failed(r) => {
                records.push(*r);
            }
        }
        // The cancel check between files: a dry run is probe-only, so
        // there is no child to kill -- it just stops.
        if cancel.is_cancelled() {
            let done = records.len();
            return BatchReport::cancelled(records, done, total);
        }
    }
    BatchReport::completed(records)
}

/// Build a "dry-run planned success" record: the source fields are
/// populated (from the probe the prepare step already did) but the
/// outcome is `Succeeded` and `ended_at` is `None` (no actual run yet).
///
/// Review #124: source fields via `EncodingRecord::from_metadata`;
/// override only the bits this helper cares about (encoder_settings
/// from `params`, planned output path).
fn make_planned_ready_record(
    task: &TranscodeTask,
    params: &EncodeParams,
    metadata: &VideoMetadata,
    started_at: chrono::DateTime<chrono::Utc>,
) -> EncodingRecord {
    let source_size_bytes = std::fs::metadata(task.input.as_path())
        .map(|m| m.len())
        .unwrap_or(0);
    let mut record = EncodingRecord::from_metadata(&task.input, source_size_bytes, Some(metadata));
    record.started_at = started_at;
    record.encoder_settings = encoder_settings_from_params(params);
    record.output_path = task
        .output
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    record
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
    ctx: &BatchContext,
    metadata: &VideoMetadata,
) -> Result<Vec<String>, EncodeError> {
    let params = ctx.params();
    let src_fps = metadata.cap_fps();
    // The context is unpacked here: the leaf selector keeps flat
    // parameters (design doc §3.10.4) so `encoder` does not depend on
    // `pipeline`.
    let encoder = select_encoder(params.format, ctx.cache(), params, ctx.capability())?;
    Ok(build_ffmpeg_command(task, params, &encoder, src_fps))
}

/// Build a failed-probe record (used when probe_video_metadata_auto
/// returns Err inside transcode_single or transcode_batch_with_processor).
fn make_failed_record(
    task: &TranscodeTask,
    params: &EncodeParams,
    started_at: chrono::DateTime<chrono::Utc>,
    encoding_duration_secs: Option<f64>,
    err: &EncodeError,
    category: ErrorCategory,
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
            category,
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
    cancel: &CancelToken,
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
    if matches!(output, ProcessOutput::Inherit) {
        // The CLI path: the terminal sees ffmpeg's output, so the
        // child is supervised here instead of being captured. The
        // cancel token is polled; a cancel kills and reaps the child
        // and removes the half-written file (design doc §3.11.2).
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Err((
                    ErrorCategory::FfmpegSpawnFailed,
                    format!("spawn {program}: {e}"),
                    String::new(),
                ))
            }
        };
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        return Err((
                            ErrorCategory::FfmpegNonzeroExit,
                            format!("{program} exited {status}"),
                            String::new(),
                        ));
                    }
                    return Ok(());
                }
                Ok(None) => {
                    if cancel.is_cancelled() {
                        let _ = child.kill();
                        let _ = child.wait();
                        cleanup_partial_output_from_argv(argv);
                        return Err((
                            ErrorCategory::Cancelled,
                            "cancelled".to_string(),
                            String::new(),
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => {
                    return Err((
                        ErrorCategory::FfmpegSpawnFailed,
                        format!("wait {program}: {e}"),
                        String::new(),
                    ))
                }
            }
        }
    }
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
            // v1.2 follow-up #2: delete the partial output file
            // ffmpeg was writing when cancel hit. argv layout is
            // v1.2 follow-up #2: remove the partial output file
            // ffmpeg was writing when cancel hit. See
            // [`cleanup_partial_output_from_argv`] for the
            // tolerance contract (NotFound OK, other Errs logged).
            cleanup_partial_output_from_argv(argv);
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

/// Remove the temp file ffmpeg was writing when cancel hit
/// (v1.2 follow-up #2). Extracted so the cleanup is testable without
/// spawning a real ffmpeg.
///
/// Since the temp-file scheme (design doc §3.11.2), `argv.last()` is
/// the **temp** path, not the final output: the old result stays
/// untouched when an attempt is cancelled.
///
/// # Contract
///
/// - `argv.last()` is treated as the output path
///   (`[ffmpeg, -y, -i, INPUT, ...encoders..., OUTPUT]`).
/// - Empty argv (shouldn't happen — ffmpeg argv always has at
///   least 2 elements) → no-op.
/// - File missing → `NotFound` is silently ignored (already
///   cleaned up by something else; nothing to do).
/// - Any other I/O error → logged at warn, NOT propagated.
///   The cancel branch is already returning `Err(Cancelled)`;
///   we don't want to mask that with a cleanup error.
///
/// This is symmetric with the CLI path's `PartialOutputHandle`
/// Guard but doesn't share an implementation (the actor path
/// doesn't carry a handle — it's run on the actor thread
/// directly via `TaskManager::run`).
fn cleanup_partial_output_from_argv(argv: &[String]) {
    let Some(out) = argv.last() else {
        return;
    };
    // v1.2 follow-up #4 commit 2: structured fields. The output
    // path is the cleanest key to grep on when a user reports
    // "did cancel actually clean up the partial for THIS file?".
    match std::fs::remove_file(out) {
        Ok(()) => tracing::info!(output = %out, "cancel cleanup: removed partial"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            output = %out,
            error = %e,
            "cancel cleanup: failed to remove partial"
        ),
    }
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
    use crate::encoder::nvenc::Capability;
    use crate::types::EncoderCache;
    use crate::types::VideoFormat;
    use std::sync::Arc;

    /// Test helper: wrap a path literal in `InputPath`. Used at every
    /// site that constructs a `TranscodeTask` literal in tests.
    /// Production code goes through the boundary (`collect_files`),
    /// so this `.expect` is safe.
    fn ip(p: &Path) -> crate::types::InputPath {
        crate::types::InputPath::new(p).expect("test fixture")
    }

    /// Is ffmpeg available on this machine? The probe path needs a real
    /// (probe-able) video, which we generate with ffmpeg's lavfi source.
    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Generate a tiny real video so the probe has something to read.
    fn make_video(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=160x120:d=1",
                "-r",
                "30",
                "-c:v",
                "libx264",
                "-t",
                "1",
            ])
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("ffmpeg spawn failed");
        assert!(status.success(), "ffmpeg failed for {name}");
        path
    }

    fn cache_with_libx264_and_svtav1() -> EncoderCache {
        EncoderCache {
            encoders_output: "\
Encoders:
 V..... libx264              libx264 H.264
 V..... libsvtav1            libsvtav1 SVT-AV1
"
            .to_string(),
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
        let _cache = cache_with_libx264_and_svtav1();

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
        let manager = super::TaskManager::new();
        let records: Vec<EncodingRecord> = manager
            .run(
                &tasks,
                "encoding",
                Path::new("/test"),
                &CancelToken::new(),
                |_task, _ctx| Ok(test_succeeded_record(&task, &params)),
            )
            .records;
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
    fn re_encodes_when_output_exists() {
        // A file in the batch is transcribed, full stop (design doc
        // §3.3.4): the config is read once per process, so a previous
        // run's output may have been produced with different settings.
        // An existing output is overwritten, never treated as "already
        // done".
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = std::env::temp_dir().join("rencoder_reexists");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = make_video(&dir, "input.mp4");
        let output = make_video(&dir, "out.mkv");
        let before = std::fs::metadata(&output).unwrap().len();

        let task = TranscodeTask {
            input: ip(&input),
            output: Some(output.clone()),
        };
        let params = params_x264_crf();
        let cache = cache_with_libx264_and_svtav1();

        let capability = Capability::default();
        let mut ctx = BatchContext::new(params, Arc::new(cache), capability);
        let record = transcode_single(&task, &mut ctx, ProcessOutput::Null, &CancelToken::new())
            .expect("transcode_single should succeed");
        assert_eq!(record.outcome, EncodingOutcome::Succeeded);
        assert!(record.output_path.is_some(), "the record names the output");
        // The output was really (re)written, not left alone.
        let after = std::fs::metadata(&output).unwrap().len();
        assert!(after > 0);
        assert_ne!(
            before, 0,
            "the fixture output must have existed before the re-encode"
        );
    }

    #[test]
    fn fails_when_input_and_output_are_the_same_file() {
        // Same-file guard: ffmpeg must never be pointed at the source.
        let dir = std::env::temp_dir().join("rencoder_same_path");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("movie.mkv");
        std::fs::write(&input, b"x").unwrap();

        let task = TranscodeTask {
            input: ip(&input),
            output: Some(input.clone()),
        };
        let params = params_x264_crf();
        let cache = cache_with_libx264_and_svtav1();

        let capability = Capability::default();
        let mut ctx = BatchContext::new(params, Arc::new(cache), capability);
        let record = transcode_single(&task, &mut ctx, ProcessOutput::Null, &CancelToken::new())
            .expect("transcode_single should succeed");
        assert_eq!(record.outcome, EncodingOutcome::Failed);
        let err = record.error.expect("failed record must carry an error");
        assert!(
            err.message.contains("input and output paths must differ"),
            "unexpected error message: {}",
            err.message
        );
        assert_eq!(err.category, ErrorCategory::ConfigError);
        // The source file must be left untouched.
        assert_eq!(std::fs::read(&input).unwrap(), b"x");
    }

    #[test]
    fn nvenc_failure_falls_back_to_software_encoding() {
        // The stored capability was stale: the NVENC encode fails. The
        // file must be re-encoded with the software encoder instead of
        // being reported as a failure.
        let dir = std::env::temp_dir().join("rencoder_batch_nvenc_fallback");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.mp4");
        let output = dir.join("out.mkv");
        std::fs::write(&input, b"x").unwrap();

        let task = TranscodeTask {
            input: ip(&input),
            output: Some(output.clone()),
        };
        let params = params_x264_crf();
        // The cache has no usable NVENC encoder, so the rebuilt argv
        // picks the software default (libx264 for the X264 params).
        let cache = EncoderCache {
            encoders_output: "\
Encoders:
 V..... libx264              libx264 H.264
"
            .to_string(),
        };

        // A fake NVENC encoder name: `is_nvenc_encoder` only looks at
        // the suffix, and the run's verdict table stays out of the
        // real encoder names.
        //
        // The verdict is pre-seeded as "unusable" so the fallback path
        // does not have to spawn a real ffmpeg probe.
        let mut capability = Capability::default();
        capability.verdicts_mut().set("test_nvenc", false);
        let nvenc_argv = vec![
            "ffmpeg".to_string(),
            "-c:v".to_string(),
            "test_nvenc".to_string(),
        ];

        let mut ctx = BatchContext::new(params, Arc::new(cache), capability);
        let plan = FilePlan::new(
            VideoMetadata::default(),
            nvenc_argv,
            output.clone(),
            Utc::now(),
            Instant::now(),
        );

        let mut calls: Vec<Vec<String>> = Vec::new();
        let record = transcode_inner(
            &task,
            plan,
            &mut ctx,
            |argv| {
                calls.push(argv);
                Err((
                    ErrorCategory::FfmpegNonzeroExit,
                    "No NVENC capable devices found".to_string(),
                    String::new(),
                ))
            },
            |ctx| {
                Some(
                    rebuild_argv_after_nvenc_failure(&task, ctx, &VideoMetadata::default())
                        .unwrap(),
                )
            },
        )
        .expect("transcode_inner should return Ok(EncodingRecord)");

        // Two attempts: NVENC first, software second.
        assert_eq!(calls.len(), 2);
        assert!(calls[0].contains(&"test_nvenc".to_string()));
        assert!(calls[1].contains(&"libx264".to_string()));
        // The encoder that failed is remembered for this run.
        assert!(ctx.capability().verdicts().is_disabled("test_nvenc"));
        // The record carries the argv that actually ran.
        assert_eq!(record.ffmpeg_argv, calls[1]);
        // ... and the record says the NVENC attempt happened and why.
        assert!(
            record.ffmpeg_stderr_tail.contains("[nvenc fallback]"),
            "fallback must leave a trace in the record: {}",
            record.ffmpeg_stderr_tail
        );
        assert!(record
            .ffmpeg_stderr_tail
            .contains("NVENC test_nvenc failed"));
    }

    #[test]
    fn nvenc_failure_does_not_fall_back_when_the_encoder_is_usable() {
        // The encode failed, but the encoder itself works -- so the
        // failure was something else (disk full, bad arguments). Falling
        // back would double the work and hide the real problem.
        let mut capability = Capability::default();
        capability.verdicts_mut().set("usable_nvenc", true);

        let dir = std::env::temp_dir().join("rencoder_batch_nvenc_usable");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.mp4");
        let output = dir.join("out.mkv");
        std::fs::write(&input, b"x").unwrap();

        let task = TranscodeTask {
            input: ip(&input),
            output: Some(output.clone()),
        };
        let params = params_x264_crf();
        let cache = EncoderCache {
            encoders_output: "\
Encoders:
 V..... libx264              libx264 H.264
"
            .to_string(),
        };

        let mut ctx = BatchContext::new(params, Arc::new(cache), capability);
        let plan = FilePlan::new(
            VideoMetadata::default(),
            vec![
                "ffmpeg".to_string(),
                "-c:v".to_string(),
                "usable_nvenc".to_string(),
            ],
            output.clone(),
            Utc::now(),
            Instant::now(),
        );

        let mut calls: Vec<Vec<String>> = Vec::new();
        let record = transcode_inner(
            &task,
            plan,
            &mut ctx,
            |argv| {
                calls.push(argv);
                Err((
                    ErrorCategory::FfmpegNonzeroExit,
                    "No space left on device".to_string(),
                    String::new(),
                ))
            },
            |ctx| {
                Some(
                    rebuild_argv_after_nvenc_failure(&task, ctx, &VideoMetadata::default())
                        .unwrap(),
                )
            },
        )
        .expect("transcode_inner should return Ok(EncodingRecord)");

        // One attempt only: the encoder works, the failure is unrelated.
        assert_eq!(calls.len(), 1);
        assert_eq!(record.outcome, EncodingOutcome::Failed);
        // The encoder is not blacklisted -- it works.
        assert!(!ctx.capability().verdicts().is_disabled("usable_nvenc"));
    }

    #[test]
    fn failed_fallback_reports_both_failures() {
        // Both attempts fail: the record has to name both, otherwise it
        // blames the software encoder for a hardware problem.
        let mut capability = Capability::default();
        capability.verdicts_mut().set("bothfail_nvenc", false);

        let dir = std::env::temp_dir().join("rencoder_batch_nvenc_both_fail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.mp4");
        let output = dir.join("out.mkv");
        std::fs::write(&input, b"x").unwrap();

        let task = TranscodeTask {
            input: ip(&input),
            output: Some(output.clone()),
        };
        let params = params_x264_crf();
        let cache = EncoderCache {
            encoders_output: "\
Encoders:
 V..... libx264              libx264 H.264
"
            .to_string(),
        };

        let mut ctx = BatchContext::new(params, Arc::new(cache), capability);
        let plan = FilePlan::new(
            VideoMetadata::default(),
            vec![
                "ffmpeg".to_string(),
                "-c:v".to_string(),
                "bothfail_nvenc".to_string(),
            ],
            output.clone(),
            Utc::now(),
            Instant::now(),
        );
        let record = transcode_inner(
            &task,
            plan,
            &mut ctx,
            |_argv| {
                Err((
                    ErrorCategory::FfmpegNonzeroExit,
                    "No NVENC capable devices found".to_string(),
                    String::new(),
                ))
            },
            |ctx| {
                Some(
                    rebuild_argv_after_nvenc_failure(&task, ctx, &VideoMetadata::default())
                        .unwrap(),
                )
            },
        )
        .expect("transcode_inner should return Ok(EncodingRecord)");

        assert_eq!(record.outcome, EncodingOutcome::Failed);
        let err = record.error.expect("failed record must carry an error");
        assert!(
            err.message.contains("NVENC bothfail_nvenc failed"),
            "message must name the NVENC failure: {}",
            err.message
        );
        assert!(
            err.message.contains("software fallback failed"),
            "message must name the fallback failure: {}",
            err.message
        );
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
        let _cache = cache_with_libx264_and_svtav1();

        // Run with a stub ffmpeg-like program that doesn't exist.
        let argv = vec![
            "/nonexistent-ffmpeg-binary".to_string(),
            "-i".into(),
            "x".into(),
        ];
        let mut ctx = BatchContext::new(params, Arc::new(_cache), Capability::default());
        let plan = FilePlan::new(
            VideoMetadata::default(),
            argv,
            task.output.as_ref().unwrap().clone(),
            Utc::now(),
            Instant::now(),
        );
        let record = transcode_with_argv(
            &task,
            plan,
            &mut ctx,
            ProcessOutput::Null,
            &CancelToken::new(),
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

    // ---- cleanup_partial_output_from_argv (v1.2 follow-up #2) ----
    //
    // Tests verify the helper called from run_ffmpeg_with_progress's
    // cancel branch. The integration with the live ffmpeg spawn is
    // covered by the round-trip test below (script-as-ffmpeg).
    //
    // Each test creates a fresh tmp dir so parallel test runs don't
    // interfere (unique_tmp pattern from job.rs::mock_tests).

    fn cleanup_tmp(label: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!(
            "rencoder_cleanup_{label}_{}_{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn a_failed_attempt_removes_the_output_it_wrote() {
        // The output is ffmpeg's own writing (newer than the attempt's
        // start) -> the half-written file goes (design doc §3.11.2).
        let dir = cleanup_tmp("failed_output");
        let output = dir.join("out.mkv");
        std::fs::write(&output, b"half written").unwrap();

        let started_at = chrono::Utc::now() - chrono::Duration::seconds(5);
        super::remove_failed_output(&output, started_at);

        assert!(
            !output.exists(),
            "the half-written output must be gone: {}",
            output.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_attempt_keeps_a_result_it_never_touched() {
        // ffmpeg failed before opening the output: the previous result
        // is intact and must survive (design doc §3.11.2).
        let dir = cleanup_tmp("untouched_output");
        let output = dir.join("out.mkv");
        std::fs::write(&output, b"previous result").unwrap();
        // The attempt started after the file was written.
        let started_at = chrono::Utc::now() + chrono::Duration::seconds(5);

        super::remove_failed_output(&output, started_at);

        assert!(
            output.exists(),
            "the previous result must survive: {}",
            output.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_partial_output_removes_existing_file() {
        // v1.2 follow-up #2: when cancel hits mid-encode, the partial
        // .mkv ffmpeg was writing should be removed. The helper
        // extracts `argv.last()` as the output path and removes it.
        let tmp = cleanup_tmp("removes");
        let partial = tmp.join("partial.mkv");
        std::fs::write(&partial, b"fake partial bytes").unwrap();
        assert!(partial.exists());

        let argv = vec![
            "ffmpeg".into(),
            "-y".into(),
            "-i".into(),
            "in.mp4".into(),
            "-c:v".into(),
            "libsvtav1".into(),
            partial.to_string_lossy().into_owned(),
        ];
        cleanup_partial_output_from_argv(&argv);

        assert!(!partial.exists(), "partial output should be removed");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn cleanup_partial_output_tolerates_missing_file() {
        // NotFound is silently ignored — the cancel branch is
        // already returning Err(Cancelled); we don't want to
        // double-log or change the return shape.
        let tmp = cleanup_tmp("missing");
        let phantom = tmp.join("never_written.mkv");
        assert!(!phantom.exists());

        let argv = vec![
            "ffmpeg".into(),
            "-i".into(),
            "in.mp4".into(),
            phantom.to_string_lossy().into_owned(),
        ];
        // Must NOT panic.
        cleanup_partial_output_from_argv(&argv);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn cleanup_partial_output_empty_argv_is_noop() {
        // Defensive: empty argv (shouldn't happen, but the type
        // allows it via `&[String]`). No panic.
        cleanup_partial_output_from_argv(&[]);
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_partial_output_after_kill_removes_partial() {
        // Integration test: spawn a shell process that mimics
        // ffmpeg's `-progress pipe:1` stream — emits
        // `progress=continue` blocks every 100ms, and writes some
        // garbage to the output path so we have a partial file to
        // clean up. On the first progress block, the test cancels
        // the token; on the next block (within ~100ms), the cancel
        // poll at the top of the run loop fires, the child is
        // killed, the partial is cleaned up, and Err is returned.
        //
        // Why not just emit ONE progress block and sleep? Because
        // `BufReader::lines()` blocks waiting for the next line —
        // if no more lines arrive, the cancel check at the top of
        // the loop never runs. The cancel poll cadence is "once
        // per progress line" (not "once per X ms"), so we need
        // the fake-ffmpeg to keep emitting lines for the cancel
        // branch to actually be reached.
        let tmp = cleanup_tmp("kill");
        let partial = tmp.join("partial.mkv");
        std::fs::write(&partial, b"x").unwrap(); // pre-create so output path exists

        let cancel = CancelToken::new();
        let cancel_for_signal = cancel.clone();

        // Script: emit a progress block, write garbage, loop
        // ~forever. The shell's `printf` flushes per-call so lines
        // arrive promptly at the parent's BufReader.
        let script = format!(
            r#"
            i=0
            while [ $i -lt 200 ]; do
                printf 'frame=%d\nfps=30\nspeed=1.0x\nout_time_us=1000000\nprogress=continue\n' $i
                echo garbage >> {}
                sleep 0.1
                i=$((i+1))
            done
            "#,
            partial.display()
        );

        let argv = vec![
            "sh".into(),
            "-c".into(),
            script.clone(),
            partial.to_string_lossy().into_owned(),
        ];

        let mut progress = |p: FfmpegProgress| {
            // On the FIRST progress block, signal cancel. The
            // next iteration of the BufReader loop will detect
            // it and bail.
            cancel_for_signal.cancel();
            let _ = p.frame;
        };

        let result = run_ffmpeg_with_progress(&argv, &cancel, Some(60.0), &mut progress);

        // Should return Cancelled (cancel branch fires after the
        // next progress line, within ~100ms).
        assert!(
            matches!(result, Err((ErrorCategory::Cancelled, _, _))),
            "expected Cancelled err, got {result:?}"
        );
        assert!(
            !partial.exists(),
            "cancel cleanup should have removed the partial output"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
