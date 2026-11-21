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
use crate::pipeline::cancel::CancelToken;
use crate::types::TranscodeTask;

/// Task metadata passed alongside the per-task closure.
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
    /// Output file path -- what a cancelled task leaves behind, and
    /// what the lib removes when it stops.
    /// `None` for read-only tasks (analyze).
    pub output: Option<PathBuf>,
}

/// Runs a batch of tasks.
///
/// One `TaskManager` per batch; `run` may be called multiple times
/// serially.
pub struct TaskManager;

impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskManager {
    /// Construct a manager.
    pub fn new() -> Self {
        Self
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
        cancel: &CancelToken,
        mut processor: P,
    ) -> BatchReport<R>
    where
        P: FnMut(&TranscodeTask, &TaskContext) -> Result<R, EncodeError>,
    {
        let total = tasks.len();
        tracing::info!(
            label = %label,
            batch_path = %batch_path.display(),
            total_files = total,
            "batch start"
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
            tracing::info!(
                task_idx = ctx.index,
                total = ctx.total,
                source = %ctx.source.display(),
                "task start"
            );

            // Cancel check between files: the in-flight task is
            // stopped by the processor (it polls the token), and the
            // batch stops here. The records of the files that
            // finished are kept (design doc §3.11.4).
            if cancel.is_cancelled() {
                tracing::warn!(
                    task_idx = ctx.index,
                    total = ctx.total,
                    source = %ctx.source.display(),
                    "batch cancelled at task"
                );
                return BatchReport::cancelled(results, idx, total);
            }

            let result = processor(task, &ctx);

            tracing::info!(
                task_idx = ctx.index,
                total = ctx.total,
                source = %ctx.source.display(),
                "task end"
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
                    tracing::warn!(
                        task_idx = ctx.index,
                        total = ctx.total,
                        source = %ctx.source.display(),
                        error = %e,
                        "batch aborted at task"
                    );
                    // The records of the files that finished are
                    // kept: a stopped batch still produced work
                    // (design doc §3.11.4).
                    return match e {
                        // The user stopped it.
                        EncodeError::UserCancel => BatchReport::cancelled(results, idx, total),
                        // Something else ended the batch -- the
                        // reason travels with the report.
                        other => BatchReport::failed(results, other.to_string()),
                    };
                }
            }
        }
        tracing::info!(
            label = %label,
            batch_path = %batch_path.display(),
            total_files = total,
            successful = results.len(),
            "batch end"
        );
        BatchReport::completed(results)
    }
}

/// How a batch ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Termination {
    /// Every file was attempted.
    Completed,
    /// The batch was stopped before the last file.
    ///
    /// The files that finished are in the report -- a cancelled batch
    /// still produced work.
    Cancelled {
        /// How many files were finished (0-based index of the one
        /// that was in flight when it stopped).
        completed: usize,
        /// How many files the batch had.
        total: usize,
    },
    /// The batch could not run at all.
    Failed(String),
}

/// What a batch produced, and how it ended.
///
/// One shape for every batch, whoever ran it: the CLI consumes it
/// directly, the actor turns it into the terminal event (design doc
/// §3.11.4). "This batch is over" has one meaning, not one per
/// frontend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchReport<R> {
    /// The records the batch produced (all of them, or the ones that
    /// finished before it stopped).
    pub records: Vec<R>,
    /// How it ended.
    pub termination: Termination,
}

impl<R> BatchReport<R> {
    /// A batch that ran to the end.
    pub fn completed(records: Vec<R>) -> Self {
        Self {
            records,
            termination: Termination::Completed,
        }
    }

    /// A batch that was stopped before the last file.
    pub fn cancelled(records: Vec<R>, completed: usize, total: usize) -> Self {
        Self {
            records,
            termination: Termination::Cancelled { completed, total },
        }
    }

    /// A batch that could not run to the end.
    ///
    /// The records of the files that finished are kept -- the batch
    /// did produce work before it stopped.
    pub fn failed(records: Vec<R>, error: impl Into<String>) -> Self {
        Self {
            records,
            termination: Termination::Failed(error.into()),
        }
    }

    /// Did the batch run to the end?
    pub fn is_completed(&self) -> bool {
        matches!(self.termination, Termination::Completed)
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
        let mgr = TaskManager::new();
        let tasks = make_tasks(3);
        let results = mgr.run(
            &tasks,
            "encoding",
            Path::new("/in"),
            &CancelToken::new(),
            |_task, _ctx| Ok::<usize, EncodeError>(42),
        );
        assert_eq!(results.records.len(), 3);
    }

    #[test]
    fn run_passes_correct_task_context() {
        let mgr = TaskManager::new();
        let tasks = make_tasks(2);
        let mut seen: Vec<TaskContext> = Vec::new();
        let _ = mgr.run(
            &tasks,
            "probing",
            Path::new("/in"),
            &CancelToken::new(),
            |_task, ctx| {
                seen.push(ctx.clone());
                Ok::<(), EncodeError>(())
            },
        );
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
        let mgr = TaskManager::new();
        let tasks = make_tasks(1);
        // Should not panic; Guard is a no-op when handle is None.
        let results = mgr.run(
            &tasks,
            "probing",
            Path::new("/in"),
            &CancelToken::new(),
            |_, _| Ok::<(), EncodeError>(()),
        );
        assert_eq!(results.records.len(), 1);
    }

    #[test]
    fn a_cancelled_batch_stops_and_keeps_what_it_finished() {
        // The cancel contract: the batch stops at the next file
        // boundary, and the records of the files that finished are
        // kept (design doc §3.11.2/§3.11.4).
        let mgr = TaskManager::new();
        let tasks = make_tasks(3);
        let cancel = CancelToken::new();
        let mut attempted = 0;

        let report = mgr.run(&tasks, "encoding", Path::new("/in"), &cancel, |_t, _c| {
            attempted += 1;
            if attempted == 2 {
                // The cancel lands while the second file runs.
                cancel.cancel();
            }
            Ok::<usize, EncodeError>(42)
        });

        assert!(!report.is_completed());
        // The first two files finished (the cancel landed while the
        // second one ran) -- both records are kept.
        assert_eq!(
            report.records.len(),
            2,
            "the files that finished before the cancel must be kept"
        );
        match report.termination {
            Termination::Cancelled { completed, total } => {
                assert_eq!(completed, 2, "stopped before the third file");
                assert_eq!(total, 3);
            }
            other => panic!("expected a cancelled termination, got {other:?}"),
        }
    }

    #[test]
    fn a_cancelled_batch_before_the_first_file_does_nothing() {
        let mgr = TaskManager::new();
        let tasks = make_tasks(3);
        let cancel = CancelToken::new();
        cancel.cancel();

        let report = mgr.run(
            &tasks,
            "encoding",
            Path::new("/in"),
            &cancel,
            |_t, _c| -> Result<usize, EncodeError> {
                panic!("no task may run after the cancel");
            },
        );

        assert!(report.records.is_empty());
        assert!(matches!(
            report.termination,
            Termination::Cancelled {
                completed: 0,
                total: 3
            }
        ));
    }

    #[test]
    fn processor_returning_err_returns_abort_position() {
        // Review #120: processor Err used to panic; now `run`
        // returns `Err((idx, e))` so the caller can decide. This
        // test verifies the abort position + the error round-trip.
        let mgr = TaskManager::new();
        let tasks = make_tasks(3);
        let result = mgr.run(
            &tasks,
            "encoding",
            Path::new("/in"),
            &CancelToken::new(),
            |task, _ctx| {
                if task.input.to_string().ends_with("1.mp4") {
                    Err(EncodeError::Ffmpeg("boom".into()))
                } else {
                    Ok::<(), EncodeError>(())
                }
            },
        );
        // The abort is not a cancel: it carries the reason.
        let Termination::Failed(err) = result.termination else {
            panic!(
                "expected a failed termination, got {:?}",
                result.termination
            );
        };
        // The error message should include the underlying error.
        let formatted = err.to_string();
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
        let mgr = TaskManager::new();
        let tasks = make_tasks(3);
        let mut attempted = Vec::new();
        let result = mgr.run(
            &tasks,
            "encoding",
            Path::new("/in"),
            &CancelToken::new(),
            |task, _ctx| {
                attempted.push(task.input.to_string());
                if task.input.to_string().ends_with("1.mp4") {
                    Err(EncodeError::Ffmpeg("boom".into()))
                } else {
                    Ok::<(), EncodeError>(())
                }
            },
        );
        assert!(!result.is_completed());
        assert_eq!(
            attempted.len(),
            2,
            "should attempt tasks 0 and 1, abort at 1; got: {attempted:?}"
        );
    }
}
