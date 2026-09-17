//! Per-task orchestration scaffold: enumeration, lifecycle logging,
//! signal-handler Guard construction.
//!
//! ## Why this module exists
//!
//! `transcode_batch` and `analyze_records_with_probe` both walk a file
//! list and need the same scaffolding:
//!
//! 1. Build a per-task context (index / total / source / output /
//!    batch label / batch path).
//! 2. Emit a batch-level `start at <path>, N files` line.
//! 3. For each task: emit a task-level `start` line, register the
//!    partial output Guard, run the processor, emit a task-level
//!    `end` line.
//! 4. Emit a batch-level `end at <path>, N files successed` line.
//!
//! Each level logs only the knowledge it owns (design rule): the
//! batch knows total + completed count + the path; each task knows
//! its index + total + the lifecycle event. No level leaks detail
//! from the other into the scaffold.
//!
//! On SIGINT, the signal handler reads the current TaskContext (with
//! the batch label + path) and emits:
//!   - a task-level `interrupt` line
//!   - a batch-level `interrupt at <path>, M files successed` line
//!
//! where M = tasks completed before interrupt (ctx.index - 1).
//!
//! ## Parallel analyze
//!
//! `analyze_records_parallel` (rayon-backed) intentionally does **not**
//! use `TaskManager`. rayon's worker pool has no ordering guarantee, so
//! emitting `task N/M` from worker threads would mislead post-mortem
//! analysis. The serial `analyze_records_with_probe` path uses
//! TaskManager; the parallel path keeps its single-line
//! `probe failed for X` log on failure.
//!
//! U19 design: docs/design.md §3.4.6.

use std::path::{Path, PathBuf};

use crate::error::EncodeError;
use crate::pipeline::partial_output::{self, PartialOutputHandle};
use crate::types::TranscodeTask;

/// Task metadata passed alongside the per-task closure. Mirrors what
/// the signal-handler reads from [`PartialOutputHandle`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskContext {
    /// 1-based index in the batch (`3` of `task 3/58`).
    pub index: usize,
    /// Total files in the batch (`58` of `task 3/58`).
    pub total: usize,
    /// Batch label (verb describing the batch, e.g. `"encoding"`,
    /// `"analyze"`). Carried into the scaffold log and into the
    /// signal handler so interrupt summaries use the same label.
    pub batch_label: String,
    /// Batch target path -- the directory or file the batch is
    /// processing. Appears in batch-level scaffold lines.
    pub batch_path: PathBuf,
    /// Source (input) file path -- what the operator cares about.
    pub source: crate::types::InputPath,
    /// Output file path -- what the signal handler removes on SIGINT.
    /// `None` for read-only tasks (analyze); the signal handler sees
    /// `None` and skips the removal.
    pub output: Option<PathBuf>,
}

/// Owns the [`PartialOutputHandle`] for the lifetime of a batch run.
/// One `TaskManager` per process; `run` may be called multiple times
/// serially (no parallel `run` calls -- the handle is process-singleton
/// per U18 §3.4.5).
pub struct TaskManager {
    handle: Option<PartialOutputHandle>,
}

impl TaskManager {
    /// Construct a manager bound to an optional signal-handler handle.
    /// Pass `None` for read-only task sets (analyze) where there is
    /// no partial file for SIGINT to remove.
    pub fn new(handle: Option<PartialOutputHandle>) -> Self {
        Self { handle }
    }

    /// Iterate `tasks`, emitting scaffold logs and (for tasks with an
    /// output) registering the partial-output Guard before each
    /// `processor` call. Returns one `R` per task.
    ///
    /// Log shape (batch contains N tasks):
    /// ```text
    /// {label} start at {batch_path}, {N} files
    ///     task 1/{N} start
    ///     task 1/{N} end
    ///     task 2/{N} start
    ///     task 2/{N} end
    ///     ...
    /// {label}, end at {batch_path}, {N} files successed
    /// ```
    ///
    /// On SIGINT (handled in `signal.rs`):
    /// ```text
    /// {label} start at {batch_path}, {N} files
    ///     task 1/{N} start
    ///     task 1/{N} end
    ///     task 2/{N} start
    ///     task 2/{N} interrupt
    /// {label} interrupt at {batch_path}, {M} files successed
    /// ```
    /// where M = ctx.index - 1 (tasks completed before the interrupt).
    ///
    /// Each level logs only what it knows (design rule): the batch
    /// doesn't name per-task detail; the task doesn't name batch
    /// summary.
    /// Run the batch through `processor`, returning either the full
    /// result list or an abort position + the last error.
    ///
    /// Review #120: this used to panic on processor `Err`, but
    /// `EncodingRecord` is a concrete `R` so the caller can choose
    /// between "synthesise a Failed record and keep going" and
    /// "abort the batch". The new return type lets the caller make
    /// that choice: `Ok(records)` means the batch ran to completion,
    /// `Err((idx, e))` means the processor asked to abort at task
    /// `idx` (0-indexed) with the last error `e`.
    ///
    /// Per-file failures (which the actor's processor turns into
    /// `Ok(Failed record)` via [`build_failed_record`]) never
    /// produce `Err` here. `Err` is reserved for the cancel / UI-
    /// disconnected paths.
    pub fn run<P, R>(
        &self,
        tasks: &[TranscodeTask],
        label: &str,
        batch_path: &Path,
        mut processor: P,
    ) -> Result<Vec<R>, (usize, EncodeError)>
    where
        P: FnMut(&TranscodeTask, &TaskContext) -> Result<R, EncodeError>,
    {
        let total = tasks.len();
        log::info!(
            "{} start at {}, {} files",
            label,
            batch_path.display(),
            total
        );
        let mut results = Vec::with_capacity(total);
        for (idx, task) in tasks.iter().enumerate() {
            let ctx = TaskContext {
                index: idx + 1,
                total,
                batch_label: label.to_string(),
                batch_path: batch_path.to_path_buf(),
                source: task.input.clone(),
                output: task.output.clone(),
            };
            // Task-level scaffold: filename + timestamp. The timestamp
            // lets post-mortem tools correlate with external wall-clock
            // events (system logs, ticket timelines). Filename tells
            // operators which file the in-flight row corresponds to
            // without parsing the batch label.
            log::info!(
                "    task {}/{} {} start",
                ctx.index,
                ctx.total,
                ctx.source.display()
            );

            // The Guard lives only across the processor call so the
            // handle is cleared between tasks -- a SIGINT after the
            // processor returns cannot delete a successfully-written
            // file (the next task's Guard overwrites, or the manager
            // drops the handle when run() returns).
            let result = {
                let _guard = partial_output::Guard::new(self.handle.as_ref(), ctx.clone());
                processor(task, &ctx)
            };

            log::info!(
                "    task {}/{} {} end",
                ctx.index,
                ctx.total,
                ctx.source.display()
            );

            match result {
                Ok(r) => results.push(r),
                Err(e) => {
                    // Review #120: instead of panicking, surface
                    // the abort position + error to the caller.
                    // The caller decides whether to abort or
                    // continue (the actor's processor turns
                    // per-file errors into Ok via
                    // `build_failed_record`, so this branch
                    // typically means cancel / UI disconnect).
                    log::warn!(
                        "    batch aborted at task {}/{}: {}",
                        ctx.index,
                        ctx.total,
                        e
                    );
                    return Err((idx, e));
                }
            }
        }
        log::info!(
            "{}, end at {}, {} files successed",
            label,
            batch_path.display(),
            total
        );
        Ok(results)
    }
}

/// Build a synthetic `EncodingRecord` for a task whose processor
/// returned `Err` (review #120). The actor's `Command::StartBatch`
/// closure catches `Err` from `transcode_single_with_progress`,
/// emits `Event::Error` to the UI, and returns this record so the
/// batch continues instead of aborting on the first failure.
///
/// Only `outcome = Failed` and `error` are meaningful; the
/// `source_*` / `output_*` fields reflect what the probe + CLI
/// knew before the encode started. `EncodingRecord::source_path`
/// is `task.input.to_string()`; `started_at` / `ended_at` are
/// `Utc::now()`; `encoding_duration_secs = None`.
///
/// Review delta #5 (delta-review fix): `encoder_settings` is now
/// derived from `params` (via `encoder_settings_from_params`) so
/// downstream Parquet analysis can correlate failed encodes with
/// their target config (CRF / bitrate / max-fps). Previously this
/// defaulted to all-zeros, which made Failed records invisible
/// to config-based queries.
pub fn build_failed_record(
    task: &TranscodeTask,
    err: EncodeError,
    params: &crate::types::EncodeParams,
) -> crate::types::EncodingRecord {
    use crate::data::record::encoder_settings_from_params;
    use crate::types::{EncodingOutcome, EncodingRecord, ErrorContext};
    use chrono::Utc;

    let category = err.category();
    let now = Utc::now();
    EncodingRecord {
        source_path: task.input.to_string(),
        source_size_bytes: 0,
        source_codec: None,
        source_container: None,
        source_width: None,
        source_height: None,
        source_fps: None,
        source_duration_secs: None,
        source_bit_rate_bps: None,
        started_at: now,
        ended_at: Some(now),
        encoding_duration_secs: None,
        encoder_selected: None,
        encoder_settings: encoder_settings_from_params(params),
        ffmpeg_argv: vec![],
        ffmpeg_stderr_tail: String::new(),
        output_path: task
            .output
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        output_size_bytes: None,
        output_duration_secs: None,
        output_codec: None,
        compression_ratio: None,
        outcome: EncodingOutcome::Failed,
        error: Some(ErrorContext {
            category,
            message: format!("{err}"),
            ffmpeg_stderr_tail: String::new(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_tasks(n: usize) -> Vec<TranscodeTask> {
        (0..n)
            .map(|i| TranscodeTask {
                input: crate::types::InputPath::new(&PathBuf::from(format!("/in/{i}.mp4")))
                    .unwrap(),
                output: Some(PathBuf::from(format!("/out/{i}.mkv"))),
            })
            .collect()
    }

    #[test]
    fn run_returns_one_result_per_task() {
        let mgr = TaskManager::new(None);
        let tasks = make_tasks(3);
        let results = mgr.run(&tasks, "encoding", Path::new("/in"), |_task, _ctx| {
            Ok::<usize, EncodeError>(42)
        });
        assert_eq!(results.unwrap().len(), 3);
    }

    #[test]
    fn run_passes_correct_task_context() {
        let mgr = TaskManager::new(None);
        let tasks = make_tasks(2);
        let mut seen: Vec<TaskContext> = Vec::new();
        let _ = mgr.run(&tasks, "probing", Path::new("/in"), |_task, ctx| {
            seen.push(ctx.clone());
            Ok::<(), EncodeError>(())
        });
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].index, 1);
        assert_eq!(seen[0].total, 2);
        assert_eq!(seen[0].batch_label, "probing");
        assert_eq!(seen[0].batch_path, PathBuf::from("/in"));
        assert_eq!(
            seen[0].source,
            crate::types::InputPath::new(&PathBuf::from("/in/0.mp4")).unwrap()
        );
        assert_eq!(seen[0].output, Some(PathBuf::from("/out/0.mkv")));
        assert_eq!(seen[1].index, 2);
        assert_eq!(
            seen[1].source,
            crate::types::InputPath::new(&PathBuf::from("/in/1.mp4")).unwrap()
        );
    }

    #[test]
    fn run_with_no_handle_does_not_panic() {
        // Analyze-style call (no signal handler installed).
        let mgr = TaskManager::new(None);
        let tasks = make_tasks(1);
        // Should not panic; Guard is a no-op when handle is None.
        let results = mgr.run(&tasks, "probing", Path::new("/in"), |_, _| {
            Ok::<(), EncodeError>(())
        });
        assert_eq!(results.unwrap().len(), 1);
    }

    #[test]
    fn processor_returning_err_returns_abort_position() {
        // Review #120: processor Err used to panic; now `run`
        // returns `Err((idx, e))` so the caller can decide. This
        // test verifies the abort position + the error round-trip.
        let mgr = TaskManager::new(None);
        let tasks = make_tasks(3);
        let result = mgr.run(&tasks, "encoding", Path::new("/in"), |task, _ctx| {
            if task.input.to_string().ends_with("1.mp4") {
                Err(EncodeError::Ffmpeg("boom".into()))
            } else {
                Ok::<(), EncodeError>(())
            }
        });
        let (idx, err) = result.expect_err("expected abort on processor Err");
        assert_eq!(idx, 1, "aborted at the failing task index (0-based)");
        // The error message should include the underlying error.
        let formatted = format!("{err}");
        assert!(
            formatted.contains("boom"),
            "abort error should carry the underlying error; got: {formatted:?}"
        );
    }

    #[test]
    fn processor_returning_err_aborts_at_task_index() {
        // Companion test: abort happens at the failing task, not
        // earlier or later. The first task succeeds, the second
        // fails, no further tasks run.
        let mgr = TaskManager::new(None);
        let tasks = make_tasks(3);
        let mut attempted = Vec::new();
        let result = mgr.run(&tasks, "encoding", Path::new("/in"), |task, _ctx| {
            attempted.push(task.input.to_string());
            if task.input.to_string().ends_with("1.mp4") {
                Err(EncodeError::Ffmpeg("boom".into()))
            } else {
                Ok::<(), EncodeError>(())
            }
        });
        assert!(result.is_err());
        assert_eq!(
            attempted.len(),
            2,
            "should attempt tasks 0 and 1, abort at 1; got: {attempted:?}"
        );
    }
}
