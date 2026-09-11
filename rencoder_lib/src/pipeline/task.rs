//! Per-task orchestration scaffold: enumeration, lifecycle logging,
//! signal-handler Guard construction.
//!
//! ## Why this module exists
//!
//! `transcode_batch` and `analyze_records_with_probe` both walk a file
//! list and need the same scaffolding:
//!
//! 1. Build a per-task context (index / total / source / output).
//! 2. Emit a minimal `task N/M start` log line.
//! 3. Register the partial output with the signal-handler handle (when
//!    the task has an output that a SIGINT should clean up).
//! 4. Run the per-task processor closure.
//! 5. Emit a minimal `task N/M end` log line.
//!
//! Centralising this in [`TaskManager::run`] lets domain functions
//! (`transcode_single`, `analyze_records_with_probe`) stay focused on
//! their own work -- encode or probe -- without owning the lifecycle
//! metadata.
//!
//! ## Scaffold log content
//!
//! The scaffold log is intentionally minimal: just `task N/M` followed
//! by `start` or `end`. No path, no duration, no status, no error.
//! Per-task detail is recoverable from the per-run data record
//! (parquet/csv in `<app_dir>/data/`); the scaffold tells the operator
//! "this file is in flight", nothing more.
//!
//! ## Parallel analyze
//!
//! `analyze_records_parallel` (rayon-backed) intentionally does **not**
//! use `TaskManager`. rayon's worker pool has no ordering guarantee, so
//! emitting `task N/M` from worker threads would mislead post-mortem
//! analysis (two workers could log `task 3/8` simultaneously). The
//! serial `analyze_records_with_probe` path uses TaskManager; the
//! parallel path keeps its existing single-line `probe failed for X`
//! log on failure.
//!
//! U19 design: docs/design.md §3.4.6.

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
    /// Source (input) file path -- what the operator cares about.
    pub source: std::path::PathBuf,
    /// Output file path -- what the signal handler removes on SIGINT.
    /// `None` for read-only tasks (analyze); the signal handler sees
    /// `None` and skips the removal.
    pub output: Option<std::path::PathBuf>,
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
    /// Scaffold log is intentionally minimal: just `task N/M` at start
    /// and end of each task, no path / duration / status. Per-task
    /// detail (path, duration, codec, error) is recoverable from the
    /// per-run data record (parquet/csv in `<app_dir>/data/`).
    /// The scaffold tells the operator "this file is in flight",
    /// nothing more.
    ///
    /// The processor closure returns `Err` only for unrecoverable
    /// setup failures (the existing batch contract -- per-file probe
    /// or encode failures are encoded as `Ok(record)` with
    /// `Outcome::Failed`). `Err` from the closure is treated as a
    /// task-level failure: a `failed` log line is emitted and the
    /// iteration continues with the next task.
    ///
    /// `label` is currently unused by the scaffold (kept in the API
    /// for callers that may want to re-introduce domain tags).
    pub fn run<P, R>(&self, tasks: &[TranscodeTask], _label: &str, mut processor: P) -> Vec<R>
    where
        P: FnMut(&TranscodeTask, &TaskContext) -> Result<R, EncodeError>,
    {
        let total = tasks.len();
        let mut results = Vec::with_capacity(total);
        for (idx, task) in tasks.iter().enumerate() {
            let ctx = TaskContext {
                index: idx + 1,
                total,
                source: task.input.clone(),
                output: task.output.clone(),
            };
            log_task_marker(&ctx, "start");

            // The Guard lives only across the processor call so the
            // handle is cleared between tasks -- a SIGINT after the
            // processor returns cannot delete a successfully-written
            // file (the next task's Guard overwrites, or the manager
            // drops the handle when run() returns).
            let result = {
                let _guard = partial_output::Guard::new(self.handle.as_ref(), ctx.clone());
                processor(task, &ctx)
            };

            log_task_marker(&ctx, "end");

            results.push(result.unwrap_or_else(|e| {
                // Processors should map per-file failures to Ok(record);
                // this branch handles a true setup-level Err from the
                // closure. We have nothing useful to return, so panic
                // with a clear message rather than synthesising a fake R.
                panic!(
                    "TaskManager::run: processor returned Err for task {}/{}: {e}. \
                     Map per-file failures to Ok(record) so the batch can continue.",
                    ctx.index, ctx.total,
                );
            }));
        }
        results
    }
}

fn log_task_marker(ctx: &TaskContext, phase: &str) {
    // Minimal scaffold: `task N/M phase`. No path, no duration, no
    // status. Two lines per task (start, end). The parquet/CSV
    // record in `<app_dir>/data/` carries per-task detail; the log
    // is for "is this file in flight?" presence.
    log::info!("task {}/{} {}", ctx.index, ctx.total, phase);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_tasks(n: usize) -> Vec<TranscodeTask> {
        (0..n)
            .map(|i| TranscodeTask {
                input: PathBuf::from(format!("/in/{i}.mp4")),
                output: Some(PathBuf::from(format!("/out/{i}.mkv"))),
            })
            .collect()
    }

    #[test]
    fn run_returns_one_result_per_task() {
        let mgr = TaskManager::new(None);
        let tasks = make_tasks(3);
        let results = mgr.run(&tasks, "encoding", |_task, _ctx| {
            Ok::<usize, EncodeError>(42)
        });
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn run_passes_correct_task_context() {
        let mgr = TaskManager::new(None);
        let tasks = make_tasks(2);
        let mut seen: Vec<TaskContext> = Vec::new();
        mgr.run(&tasks, "probing", |_task, ctx| {
            seen.push(ctx.clone());
            Ok::<(), EncodeError>(())
        });
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].index, 1);
        assert_eq!(seen[0].total, 2);
        assert_eq!(seen[0].source, PathBuf::from("/in/0.mp4"));
        assert_eq!(seen[0].output, Some(PathBuf::from("/out/0.mkv")));
        assert_eq!(seen[1].index, 2);
        assert_eq!(seen[1].source, PathBuf::from("/in/1.mp4"));
    }

    #[test]
    fn run_with_no_handle_does_not_panic() {
        // Analyze-style call (no signal handler installed).
        let mgr = TaskManager::new(None);
        let tasks = make_tasks(1);
        // Should not panic; Guard is a no-op when handle is None.
        let results = mgr.run(&tasks, "probing", |_, _| Ok::<(), EncodeError>(()));
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn processor_returning_err_panics_with_task_index() {
        // Processor returns Err for one task; the batch cannot continue
        // (we have no R to return for the failing slot), so we panic
        // with a clear message. We verify by checking that the panic
        // message contains the failing task index.
        let mgr = TaskManager::new(None);
        let tasks = make_tasks(3);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            mgr.run(&tasks, "encoding", |task, _ctx| {
                if task.input.to_string_lossy().ends_with("1.mp4") {
                    Err(EncodeError::Ffmpeg("boom".into()))
                } else {
                    Ok::<(), EncodeError>(())
                }
            })
        }));
        let err = result.expect_err("expected panic on processor Err");
        let msg = err
            .downcast_ref::<String>()
            .map(|s| s.as_str())
            .or_else(|| err.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            msg.contains("task 2/3"),
            "panic message should name the failing task; got: {msg:?}"
        );
    }
}
