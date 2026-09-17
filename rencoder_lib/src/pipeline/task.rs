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
    pub fn run<P, R>(
        &self,
        tasks: &[TranscodeTask],
        label: &str,
        batch_path: &Path,
        mut processor: P,
    ) -> Vec<R>
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
        log::info!(
            "{}, end at {}, {} files successed",
            label,
            batch_path.display(),
            total
        );
        results
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
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn run_passes_correct_task_context() {
        let mgr = TaskManager::new(None);
        let tasks = make_tasks(2);
        let mut seen: Vec<TaskContext> = Vec::new();
        mgr.run(&tasks, "probing", Path::new("/in"), |_task, ctx| {
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
            mgr.run(&tasks, "encoding", Path::new("/in"), |task, _ctx| {
                if task.input.to_string().ends_with("1.mp4") {
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
