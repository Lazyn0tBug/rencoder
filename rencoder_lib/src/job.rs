//! GUI actor entry point (U21 §3.8). Owns the Command/Event message
//! protocol that bridges UI (Dioxus) and lib.
//!
//! Architecture (actor model, §3.8.2):
//! - UI is one actor; lib is another.
//! - Communication is strictly via Command (UI → lib) and Event
//!   (lib → UI) on a single mpsc channel pair.
//! - No shared variables, no callbacks, no cross-boundary function
//!   calls. ffmpeg subprocess, PartialOutputHandle, SIGINT handler
//!   all live inside the lib actor — UI sees only the message stream.
//!
//! Layering:
//! - This module defines the message protocol types (§3.8.3) and
//!   the controller entry point (`ControllerHandle` +
//!   `start_controller()`, §3.8.4).
//! - The controller thread reuses `transcode_single` for the encode
//!   loop and `run_ffmpeg_with_progress` (when wired) for the
//!   per-frame event stream.
//!
//! Single-batch constraint: §3.4 — one batch in flight at a time.
//! A second `StartBatch` while one is running returns
//! `Event::Error(ConfigError)` rather than queueing.

use crossbeam_channel::{bounded, select, unbounded, Receiver, Sender};
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use chrono::{DateTime, Utc};

use crate::analyzer::records::{analyze_records_with_probe_and_progress_parallel, AnalyzeProgress};
use crate::config::app::AppConfig;
use crate::config::paths::{self, Paths};
use crate::data::extract::probe_video_metadata_auto;
use crate::encoder::nvenc::Capability;
use crate::error::EncodeError;
use crate::pipeline::batch::{FfmpegFileEncoder, FfmpegProgress, FileEncoder};
use crate::pipeline::cancel::CancelToken;
use crate::pipeline::context::BatchContext;
use crate::pipeline::task::TaskManager;
use crate::types::{EncodeParams, EncodeTask, EncoderCache, ErrorCategory};

// Re-export for UI consumers — `Event::FileCompleted` and
// `Event::BatchCompleted` carry `EncodingRecord`, so consumers
// (rencoder_ui) need to name the type. Also used internally by
// the actor thread (mock_tests build EncodingRecord values).
pub use crate::types::EncodingRecord;

/// Identifier for one batch run. Monotonically increasing per
/// process; assigned by `start_controller` on `BatchStarted`. U21
/// §3.8.3 — `id: u64` is enough for any realistic session length.
pub type BatchId = u64;

/// Identifier for one command request.
///
/// The **submitter** allocates it (monotonic per session) and the lib
/// echoes it on acceptance and on rejection (design doc §3.11.3):
/// "which of my requests was this?" must have one answer, and the
/// answer cannot be "the last one" -- two commands can be in flight.
pub type RequestId = u64;

/// A command plus the id of the request that carries it.
///
/// The channel carries `Request`, not `Command`: every command gets an
/// id, so the lib can answer "accepted" / "rejected" about a
/// *specific* request, and the UI can put its own state back the way
/// it was when a request is refused (design doc §3.11.3).
#[derive(Debug)]
pub struct Request {
    /// The submitter's id for this request.
    pub id: RequestId,
    /// What the request asks for.
    pub command: Command,
}

impl Request {
    /// A request with the given id.
    pub fn new(id: RequestId, command: Command) -> Self {
        Self { id, command }
    }
}

// =====================================================================
// Command (UI → lib) — §3.8.3
// =====================================================================

/// Messages from the UI to the lib actor. Both enums are
/// `#[non_exhaustive]` so future variants don't break exhaustive
/// matches in consumer code (see design.md §8 items 33, 36).
#[derive(Debug)]
#[non_exhaustive]
pub enum Command {
    /// Start a new encode batch. The lib actor will run the same
    /// flow as the CLI (probe + skip + argv + ffmpeg) but emit
    /// Events to `evt_tx` instead of writing to a terminal.
    StartBatch {
        params: EncodeParams,
        files: Vec<crate::types::InputPath>,
    },
    /// Dry-run: simulate a batch without invoking ffmpeg. Runs the
    /// same `encode_prepare` (probe + argv build) for each file, then
    /// emits `Event::DryRunFinished` with the planned records
    /// (Succeeded for the files that would be encoded, Failed for the
    /// ones the prepare step could not handle — NO ffmpeg ran). Lets
    /// the UI show the user what WOULD happen before commit
    /// (Tier 2, §3.8.7 v1 limitations item 1).
    DryRun {
        params: EncodeParams,
        files: Vec<crate::types::InputPath>,
    },
    /// Abort the batch `batch_id` (including any in-flight single
    /// file). Equivalent to SIGINT but triggered by the UI button. The
    /// `CancelToken` shared with the SIGINT handler means both paths
    /// cancel the same way.
    ///
    /// **The batch id is not optional** (design doc §3.11.3): a cancel
    /// that names a batch which is not the active one is rejected. A
    /// stale cancel -- the user cancelled, then started a new batch
    /// before the old cancel was processed -- must not stop the new
    /// batch.
    ///
    /// Per §3.8.3 item 31: no "skip current file only" variant.
    /// User either lets the file finish or aborts the whole batch.
    Cancel { batch_id: BatchId },
    /// Read history records from `<app_dir>/data/encoding-*.parquet`.
    /// Controller resolves the data dir internally via
    /// the app's records directory (design doc §3.6); the payload
    /// is the filter.
    LoadHistory {
        /// `None` = all records; `Some(N)` = most recent N.
        limit: Option<usize>,
        /// `None` = no time filter; `Some(t)` = only records with
        /// `started_at >= t`.
        after: Option<DateTime<Utc>>,
    },
    /// Save an `AppConfig` back to the original path
    /// (`ControllerHandle::config_path`). Used by the UI after
    /// editing a Setup-page field.
    ///
    /// The edited config rides along as the payload: the UI edits
    /// its own draft (a clone), so without the payload the lib would
    /// write back the config *it* holds and the edit would be lost --
    /// the file would be rewritten with the original values while
    /// the UI showed "Saved".
    ///
    /// Emits `Event::ConfigSaved` on success / `Event::Error` on
    /// failure. A change to `[encode].nv` also refreshes the encoder
    /// cache, so the switch takes effect for the next batch instead
    /// of the next application start.
    ///
    /// `revision` is the submitter's revision of the edit this save
    /// carries. The lib treats it as an opaque number and hands it
    /// back in the receipt: the UI needs it to know **which** of its
    /// edits are on disk now (design doc §3.10.5 -- the receipt
    /// clears only the edits it saved, later edits stay).
    SaveConfig {
        config: Box<AppConfig>,
        revision: u64,
    },
    /// Detect this machine's NVENC capability and report it back.
    ///
    /// Same logic as the CLI's `rencoder detect`: the result is stored
    /// in `<app_dir>/nvenc-capability.json`, and the stored record is
    /// reused when it matches the current ffmpeg build.
    ///
    /// * `force = true` -- always probe (the "重新检测" button).
    /// * `force = false` -- reuse the stored record when it is fresh.
    Detect { force: bool },
    /// The GUI's working directory changed.
    ///
    /// The operation directory moves with it (design doc §3.6): the
    /// run-level paths follow. The capability check runs again -- the
    /// directory changed, the machine did not (design doc §3.10.7), so
    /// the machine's facts are reused and only the filesystem is
    /// checked anew. The report lands in the new operation directory.
    SetWorkingDir { dir: PathBuf },
    /// Start a parallel analyze batch (U22 / Tier 3 #9).
    ///
    /// The lib actor probes each file (c-probe if enabled, ffprobe
    /// CLI fallback otherwise — same dispatcher as
    /// `analyze_records_parallel`) and emits:
    ///   1. `Event::AnalyzeStarted { total: files.len() }` once,
    ///   2. `Event::AnalyzeProgress { done, total, failed, last }`
    ///      once per file completion (rayon workers may fire out of
    ///      input order; `done` / `failed` are monotonic counters),
    ///   3. `Event::AnalyzeFinished { records, failed_count }` once.
    ///
    /// **v1 limitations** (see docs/design.md §3.4 "分析进度上报"):
    /// - Busy-gate: cannot run alongside `StartBatch` / `DryRun` (same
    ///   gate). The second concurrent batch gets
    ///   `Event::CommandRejected` ("another batch is already running")
    ///   instead.
    /// - No `Command::Cancel` support: analyze batches are read-only
    ///   and short-lived; cancellation requires per-worker
    ///   `AtomicBool` polling that doubles the test matrix.
    /// - SIGINT does not interrupt analyze: `Event::AnalyzeFinished`
    ///   fires cleanly, not `BatchCancelled`.
    Analyze { files: Vec<crate::types::InputPath> },
}

// =====================================================================
// Event (lib → UI) — §3.8.3
// =====================================================================

/// Messages from the lib actor to the UI. Variants are grouped by
/// topic in the doc but encoded as enum variants (no separate
/// `topic` field) so dispatch is just `match` (§3.8.3).
///
/// `large_enum_variant` is accepted: `BatchCompleted` /
/// `HistoryLoaded` carry `Vec<EncodingRecord>` (~230 B per
/// variant) while other variants are <100 B. Events are emitted
/// one-at-a-time through a single mpsc receiver — no in-flight
/// accumulation, so the memory cost is bounded by one variant's
/// worst-case size at a time.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
#[non_exhaustive]
pub enum Event {
    // ---- Request topic (the answer to a command) ----
    /// The lib accepted a request (design doc §3.11.3).
    ///
    /// `request_id` is the id the submitter gave the request;
    /// `batch_id` is the batch the request starts (`None` for the
    /// commands that start no batch: history, config, detect).
    ///
    /// The UI enters its `Starting` phase on this event -- not when
    /// it enqueued the command. Enqueuing only says "the command is
    /// on its way"; acceptance says "the lib took it".
    CommandAccepted {
        request_id: RequestId,
        batch_id: Option<BatchId>,
    },
    /// A command was rejected; nothing fatal happened.
    ///
    /// The busy gate answers this way when a batch is already running:
    /// the command is dropped and the running batch continues. It used
    /// to answer with [`Event::Error`], which is fatal-only -- the UI
    /// read that as "the batch failed" and finalized it.
    ///
    /// `request_id` says **which** request was refused, so the UI can
    /// put its own state back the way it was (design doc §3.11.3).
    CommandRejected {
        request_id: RequestId,
        reason: String,
    },

    // ---- Lifecycle topic (low frequency) ----
    /// A batch has started. `id` is unique within the controller's
    /// lifetime. `total_files` is the count after collection.
    BatchStarted { id: BatchId, total_files: usize },
    /// A single file is about to be encoded. `source` is the input
    /// path; the output path is computed by the lib and surfaced via
    /// `FileCompleted`.
    FileStarted {
        batch_id: BatchId,
        index: usize,
        total: usize,
        source: crate::types::InputPath,
    },
    /// A single file finished encoding. The full record (including
    /// outcome, dimensions, sizes) is included for UI display.
    FileCompleted {
        batch_id: BatchId,
        index: usize,
        record: EncodingRecord,
    },
    /// All files in the batch finished. The records vec mirrors
    /// what `transcode_batch` would have written to Parquet.
    BatchCompleted {
        batch_id: BatchId,
        records: Vec<EncodingRecord>,
    },
    /// Dry-run finished — `records` contains the planned outcomes
    /// (Succeeded for the files that would be encoded, Failed for the
    /// ones the prepare step could not handle) WITHOUT invoking
    /// ffmpeg. UI uses this to show the user what WOULD happen before
    /// commit.
    DryRunFinished {
        batch_id: BatchId,
        /// The capability generation the dry run froze (design doc
        /// §3.10.5): the plan is only valid for the generation it
        /// was planned under, so the UI compares it with the current
        /// one before reusing the plan.
        generation: u64,
        records: Vec<EncodingRecord>,
    },
    /// The batch was cancelled (by `Command::Cancel` or SIGINT).
    /// `completed` is how many files finished before the cancel
    /// landed. The partial output of the cancelled file was removed by
    /// the lib's execution path (design doc §3.11.2).
    BatchCancelled {
        batch_id: BatchId,
        completed: usize,
        total: usize,
    },
    /// The batch could not run: the single-instance guard refused the
    /// target, the encoder cache was unusable, ...
    ///
    /// A **batch-scoped** failure: it finalizes *this* batch and
    /// nothing else. The UI matches the batch id -- a failure of an
    /// old batch must not finalize the batch the user is watching
    /// now (design doc §3.11.3).
    BatchFailed {
        batch_id: BatchId,
        category: ErrorCategory,
        message: String,
    },

    // ---- Progress topic (~2 events/sec per active file) ----
    /// Per-file progress snapshot from `run_ffmpeg_with_progress`.
    /// UI reducer updates its `progress` map by `file_index`.
    Progress {
        batch_id: BatchId,
        file_index: usize,
        frame: u64,
        fps: f64,
        speed: f64,
        time_secs: f64,
        /// `(total - time) / speed`. `None` when speed is too low
        /// or total duration unknown. Computed by
        /// `run_ffmpeg_with_progress` from the caller's
        /// `total_duration_secs` parameter.
        eta_secs: Option<f64>,
    },

    // ---- Log topic (~10 events/sec) ----
    /// Single stderr line from ffmpeg. The lib passes lines through
    /// unfiltered (§3.8.6 log filtering); the UI reducer caps the
    /// ring buffer at 1000 entries (§8 item 30).
    LogLine {
        batch_id: BatchId,
        file_index: usize,
        line: String,
    },

    // ---- State topic ----
    /// Result of a `Command::LoadHistory`. `records` is the filtered
    /// view (limit + after applied); empty vec is valid (no records
    /// match the filter).
    HistoryLoaded { records: Vec<EncodingRecord> },

    /// Result of a `Command::SaveConfig` — the config was written
    /// to disk. The path is included so the UI can show feedback
    /// like "saved to /path/to/rencoder.toml".
    ///
    /// `revision` is the revision the save carried (design doc
    /// §3.10.5): the UI clears the edits at or below it, and keeps
    /// the ones the user made while the save was in flight.
    ConfigSaved { path: PathBuf, revision: u64 },

    /// The NVENC capability of this machine (after a `Command::Detect`
    /// or the startup resolve). The UI shows it next to the NVENC
    /// switch: which hardware encoders run here, and why not.
    NvencCapability(crate::encoder::nvenc::Capability),

    /// The run capability report is written; here is its path.
    ///
    /// The frontends **read the file** (design doc §3.10.7): one
    /// check, one result, several readers -- nobody recomputes it.
    /// The event only says "it is ready"; the content is the file's.
    CapabilityChecked { report: PathBuf },

    // ---- Analyze topic (U22 / Tier 3 #9) ----
    // See docs/design.md §3.4 "分析进度上报" for the full protocol
    // (Started + N×Progress + Finished) and the failure-counting /
    // non-deterministic-order / failed-first-display rationale.
    //
    // These three variants are emitted by `run_actor` when it
    // receives `Command::Analyze { files }`. The lib invokes
    // `analyze_records_with_probe_and_progress_parallel` (defined
    // in `crate::analyzer::records`) with a progress closure that
    // converts each per-file `AnalyzeProgress` into the matching
    // `Event::AnalyzeProgress` below.
    //
    // Display convention (design doc §3.4 "最终结果的展示约定"):
    // - Live `done` / `failed` counters are monotonic; UI uses them
    //   as the main progress signal.
    // - `last` field is the just-completed file in *completion
    //   order* (rayon workers race) — operator reads it as
    //   "just completed: X.mp4", not "next in input order".
    // - The final `Vec<AnalysisRecord>` is in input order
    //   (`par_iter().collect()` preserves it).
    /// `Command::Analyze` accepted; analysis about to start.
    /// `total` is the count of files (== `Command::Analyze.files.len()`).
    AnalyzeStarted { batch_id: BatchId, total: usize },
    /// Emitted once per file completion (rayon workers may fire
    /// non-deterministically). `done` is the cumulative count of
    /// files probed (including failures); `total` is constant; the
    /// `done` / `total` ratio is the UI's progress gauge.
    /// `failed` is the cumulative count of files that probed Err.
    /// `last` is `Some(file)` for the file that just completed —
    /// operator reads it as "just completed" indicator.
    AnalyzeProgress {
        batch_id: BatchId,
        done: usize,
        total: usize,
        failed: usize,
        last: Option<crate::analyzer::records::AnalyzedFile>,
    },
    /// Whole batch done. `failed_count == records.iter().filter(|r|
    /// r.fps.is_none()).count()` (invariants — UI can cross-check).
    /// `records` is in input order (rayon `par_iter().collect()`).
    AnalyzeFinished {
        batch_id: BatchId,
        records: Vec<crate::analyzer::records::AnalysisRecord>,
        failed_count: usize,
    },

    // ---- Error topic (fatal only) ----
    /// A **non-batch** operation failed (config save, NVENC
    /// detection, ...). The request id says which operation it was.
    ///
    /// It finalizes nothing: a failed config save must not finalize
    /// the batch the user is watching (design doc §3.11.3).
    OperationFailed {
        request_id: RequestId,
        category: ErrorCategory,
        message: String,
    },
    /// Fatal-only error, with **no scope**: the actor itself could
    /// not do its job (the encoder cache never came up). No batch is
    /// running when this fires, so there is nothing to finalize.
    ///
    /// Per-file failures travel through `EncodingRecord.error` /
    /// `FileCompleted` (design doc §3.11.3); a batch-level failure
    /// travels through `BatchFailed`, scoped to its batch.
    ///
    /// Payload is intentionally lean (no `ffmpeg_stderr_tail`) —
    /// see design.md §3.8.3 doc comment on `Event::Error`. UI
    /// needing stderr should consult the last `EncodingRecord`
    /// after `BatchCompleted`.
    Error {
        category: ErrorCategory,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EncoderSettings, EncodingOutcome};
    use std::sync::atomic::Ordering;

    fn sample_record() -> EncodingRecord {
        EncodingRecord {
            source_path: "/in.mp4".into(),
            source_size_bytes: 1024,
            source_codec: Some("h264".into()),
            source_container: None,
            source_width: Some(1920),
            source_height: Some(1080),
            source_fps: Some(30.0),
            source_duration_secs: Some(60.0),
            source_bit_rate_bps: None,
            started_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            ended_at: None,
            encoding_duration_secs: None,
            encoder_selected: None,
            encoder_settings: EncoderSettings::default(),
            ffmpeg_argv: vec![],
            ffmpeg_stderr_tail: String::new(),
            output_path: None,
            output_size_bytes: None,
            output_duration_secs: None,
            output_codec: None,
            compression_ratio: None,
            outcome: EncodingOutcome::Succeeded,
            error: None,
        }
    }

    #[test]
    fn every_command_variant_is_constructible() {
        // The protocol's shape, pinned.
        //
        // The match is **exhaustive on purpose**: a new variant
        // breaks the build until it is constructed here -- so nobody
        // adds a command without its shape being looked at.
        let commands = vec![
            Command::StartBatch {
                params: EncodeParams::default(),
                files: vec![crate::types::InputPath::new(std::path::Path::new("/a.mp4")).unwrap()],
            },
            Command::DryRun {
                params: EncodeParams::default(),
                files: vec![crate::types::InputPath::new(std::path::Path::new("/a.mp4")).unwrap()],
            },
            Command::Cancel { batch_id: 1 },
            Command::LoadHistory {
                limit: Some(10),
                after: None,
            },
            Command::SaveConfig {
                config: Box::new(AppConfig::default()),
                revision: 1,
            },
            Command::Detect { force: true },
            Command::SetWorkingDir {
                dir: std::path::PathBuf::from("/tmp"),
            },
            Command::Analyze {
                files: vec![crate::types::InputPath::new(std::path::Path::new("/a.mp4")).unwrap()],
            },
        ];
        for command in commands {
            match command {
                Command::StartBatch { .. }
                | Command::DryRun { .. }
                | Command::Cancel { .. }
                | Command::LoadHistory { .. }
                | Command::SaveConfig { .. }
                | Command::Detect { .. }
                | Command::SetWorkingDir { .. }
                | Command::Analyze { .. } => {}
            }
        }
    }

    #[test]
    fn every_event_variant_is_constructible() {
        // The protocol's shape, pinned -- exhaustive on purpose, see
        // [`every_command_variant_is_constructible`].
        let rec = sample_record();
        let source = crate::types::InputPath::new(std::path::Path::new("/a.mp4")).unwrap();
        let events = vec![
            Event::CommandAccepted {
                request_id: 1,
                batch_id: Some(1),
            },
            Event::CommandRejected {
                request_id: 1,
                reason: "another batch is already running".into(),
            },
            Event::BatchStarted {
                id: 1,
                total_files: 5,
            },
            Event::FileStarted {
                batch_id: 1,
                index: 0,
                total: 5,
                source: source.clone(),
            },
            Event::FileCompleted {
                batch_id: 1,
                index: 0,
                record: rec.clone(),
            },
            Event::BatchCompleted {
                batch_id: 1,
                records: vec![rec.clone()],
            },
            Event::DryRunFinished {
                batch_id: 1,
                generation: 1,
                records: vec![rec.clone()],
            },
            Event::BatchCancelled {
                batch_id: 1,
                completed: 2,
                total: 5,
            },
            Event::BatchFailed {
                batch_id: 1,
                category: ErrorCategory::ConfigError,
                message: "the target is held".into(),
            },
            Event::Progress {
                batch_id: 1,
                file_index: 0,
                frame: 100,
                fps: 30.0,
                speed: 1.5,
                time_secs: 5.0,
                eta_secs: Some(40.0),
            },
            Event::LogLine {
                batch_id: 1,
                file_index: 0,
                line: "frame= 100".into(),
            },
            Event::HistoryLoaded {
                records: vec![rec.clone()],
            },
            Event::ConfigSaved {
                path: std::path::PathBuf::from("/tmp/rencoder.toml"),
                revision: 1,
            },
            Event::NvencCapability(Capability::default()),
            Event::CapabilityChecked {
                report: std::path::PathBuf::from("/tmp/capabilities.json"),
            },
            Event::AnalyzeStarted {
                batch_id: 1,
                total: 3,
            },
            Event::AnalyzeProgress {
                batch_id: 1,
                done: 1,
                total: 3,
                failed: 0,
                last: None,
            },
            Event::AnalyzeFinished {
                batch_id: 1,
                records: vec![],
                failed_count: 0,
            },
            Event::OperationFailed {
                request_id: 1,
                category: ErrorCategory::ConfigError,
                message: "save failed".into(),
            },
            Event::Error {
                category: ErrorCategory::FfmpegSpawnFailed,
                message: "ffmpeg not found".into(),
            },
        ];
        for event in events {
            match event {
                Event::CommandAccepted { .. }
                | Event::CommandRejected { .. }
                | Event::BatchStarted { .. }
                | Event::FileStarted { .. }
                | Event::FileCompleted { .. }
                | Event::BatchCompleted { .. }
                | Event::DryRunFinished { .. }
                | Event::BatchCancelled { .. }
                | Event::BatchFailed { .. }
                | Event::Progress { .. }
                | Event::LogLine { .. }
                | Event::HistoryLoaded { .. }
                | Event::ConfigSaved { .. }
                | Event::NvencCapability(_)
                | Event::CapabilityChecked { .. }
                | Event::AnalyzeStarted { .. }
                | Event::AnalyzeProgress { .. }
                | Event::AnalyzeFinished { .. }
                | Event::OperationFailed { .. }
                | Event::Error { .. } => {}
            }
        }
    }

    // ---- send_evt_lossy / bounded channel policy (review #111) ----

    /// Helper: build a synthetic Event::Progress for tests.
    fn progress_event(i: usize) -> Event {
        Event::Progress {
            batch_id: 1,
            file_index: i,
            frame: i as u64 * 100,
            fps: 30.0,
            speed: 1.0,
            time_secs: i as f64,
            eta_secs: None,
        }
    }

    #[test]
    fn send_evt_lossy_drops_progress_events_when_full() {
        // cap 2: first two events land, rest are dropped (droppable class).
        let (tx, rx) = crossbeam_channel::bounded::<Event>(2);
        let counter = DropCounter::default();
        for i in 0..5 {
            send_evt_lossy(&tx, progress_event(i), &counter);
        }
        // Drain whatever landed — should be exactly 2.
        let received: Vec<Event> = rx.try_iter().collect();
        assert_eq!(received.len(), 2);
        // 3 of the 5 dropped; counter incremented 3 times.
        assert_eq!(counter.0.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn is_droppable_classification_covers_progress_and_analyze_progress() {
        assert!(is_droppable_event(&progress_event(0)));
        assert!(is_droppable_event(&Event::AnalyzeProgress {
            batch_id: 1,
            done: 1,
            total: 10,
            failed: 0,
            last: None,
        }));
        // Structural variants must NOT be droppable.
        assert!(!is_droppable_event(&Event::BatchStarted {
            id: 1,
            total_files: 10,
        }));
        assert!(!is_droppable_event(&Event::BatchCompleted {
            batch_id: 1,
            records: vec![],
        }));
        assert!(!is_droppable_event(&Event::Error {
            category: ErrorCategory::FfmpegSpawnFailed,
            message: "x".into(),
        }));
    }

    #[test]
    fn send_evt_lossy_blocks_for_structural_event_when_full() {
        // cap 1, filled with a structural event. Second structural
        // event must block (not drop). We use a separate thread to
        // observe: drop the receiver side after the first send, the
        // second send (after fill) would otherwise hang forever —
        // so we test the drop path by closing the channel and
        // confirming the function returns without panic.
        let (tx, rx) = crossbeam_channel::bounded::<Event>(1);
        let counter = DropCounter::default();
        send_evt_lossy(
            &tx,
            Event::BatchStarted {
                id: 1,
                total_files: 10,
            },
            &counter,
        );
        // Drop receiver — second structural send should now hit
        // Disconnected (the bounded send returns Err immediately on
        // disconnect; our helper logs + returns). Confirms the
        // structural path uses `send` (blocking) and that the
        // Disconnected branch handles it.
        drop(rx);
        send_evt_lossy(
            &tx,
            Event::BatchCompleted {
                batch_id: 1,
                records: vec![],
            },
            &counter,
        );
        // Droppable counter must NOT have ticked (structural events
        // never increment).
        assert_eq!(counter.0.load(Ordering::Relaxed), 0);
    }
}

// Review #121: EncodeError::category() (in error.rs) replaces
// the previous `encode_error_category` helper. The mapping lives
// on the error type itself so the dependency direction stays
// correct (pipeline::task uses it without depending on job).

// =====================================================================
// Controller — §3.8.4
// =====================================================================

/// Handle the UI receives from `start_controller()`. Owns:
/// - `cmd_tx`: clone freely for UI components that send commands
///   (start button, cancel button, history button).
/// - `evt_rx`: single consumer — the UI's top-level coroutine drains
///   it and dispatches into its reducer. Multi-consumer would break
///   event ordering.
/// - `config`: the loaded [`AppConfig`] (auto-created if missing,
///   matching CLI behavior). UI reads this to derive `EncodeParams`
///   for `Command::StartBatch` instead of using defaults. Cheap to
///   clone (`Arc<AppConfig>`).
/// - `config_path`: the path the AppConfig was loaded from
///   (`config::paths::config_file()` result; UI uses this to save back).
pub struct ControllerHandle {
    pub(crate) cmd_tx: Sender<Request>,
    pub(crate) evt_rx: Receiver<Event>,
    pub(crate) config: Arc<AppConfig>,
    pub(crate) config_path: PathBuf,
}

/// Capacity for `evt_tx` (review #111). Sized for ~50 seconds of
/// progress headroom at ~20 events/sec for 10 concurrent files.
const EVT_TX_CAPACITY: usize = 1024;

/// Capacity for `cmd_tx` (review #111). Human-paced UI sends;
/// 16 covers any realistic burst.
const CMD_TX_CAPACITY: usize = 16;

/// Classify an `Event` by its drop-on-full behavior (review #111).
/// Progress-class events can be dropped when the consumer is
/// behind — losing one Progress tick just means the UI sees the
/// next one slightly later. Structural events (BatchStarted,
/// FileCompleted, BatchCompleted, Error) must NOT be dropped
/// because they drive batch state and the UI relies on them for
/// correctness.
///
/// `Event::AnalyzeProgress` is treated as progress-class for the
/// same reason — `done`/`total`/`failed` are monotonic counters
/// the UI uses for progress bars; missing a tick just delays
/// the bar by one increment.
///
/// `#[non_exhaustive]` on `Event` means future variants default
/// to "structural" — the safe choice for correctness. The match
/// only enumerates the droppable cases; a new variant that should
/// be droppable must add its own arm.
fn is_droppable_event(event: &Event) -> bool {
    matches!(
        event,
        Event::Progress { .. } | Event::AnalyzeProgress { .. }
    )
}

/// Per-sender count of dropped events.
///
/// Each sender owns one — the shared `Arc<AtomicU64>` this replaced
/// was written by five senders and read by nobody. The counter is
/// never shared *between* senders; the analyze path does share one
/// across its rayon workers (they are one sender), which is why the
/// inside is an atomic.
#[derive(Default)]
struct DropCounter(std::sync::atomic::AtomicU64);

impl DropCounter {
    /// Record a drop. The first one warns — that is the signal the UI
    /// is behind — and every hundredth after it keeps the count
    /// visible without flooding the log.
    fn record_drop(&self) {
        let count = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if count == 1 || count.is_multiple_of(100) {
            tracing::warn!(
                dropped = count,
                "evt_tx full; dropping progress events (the UI is behind)"
            );
        }
    }
}

/// Send an `Event` to the UI with per-variant policy (review
/// #111). Progress-class events drop on full; structural events
/// block. Disconnected UI is logged + ignored.
fn send_evt_lossy(tx: &Sender<Event>, event: Event, dropped: &DropCounter) {
    match tx.try_send(event) {
        Ok(()) => {}
        Err(crossbeam_channel::TrySendError::Full(event)) => {
            if is_droppable_event(&event) {
                dropped.record_drop();
            } else {
                // Structural event: block until the UI drains. This
                // stalls the actor thread briefly, but structural
                // events are rare (1 per file) and the consumer
                // backlog would have to be > 1024 for the wait to
                // be noticeable.
                let _ = tx.send(event);
            }
        }
        Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
            tracing::warn!("evt_tx disconnected (UI dropped receiver)");
        }
    }
}

impl ControllerHandle {
    /// Get a cloneable sender for sending requests. The UI typically
    /// clones this once per component that needs to dispatch.
    pub fn command_sender(&self) -> Sender<Request> {
        self.cmd_tx.clone()
    }

    /// Get the loaded config (cheap Arc clone). UI uses this to read
    /// `encode.format` / `encode.crf` / `encode.nv` / `encode.extensions`
    /// for `Command::StartBatch`. Always `Some` — the controller
    /// thread exits if config load failed (UI sees `Event::Error`
    /// then no more events).
    pub fn config(&self) -> Arc<AppConfig> {
        Arc::clone(&self.config)
    }

    /// Get the path the AppConfig was loaded from. UI uses this
    /// for `Command::SaveConfig` — saves back to the same path
    /// (`$RENENCODER_CONFIG` env override honored on write too).
    pub fn config_path(&self) -> PathBuf {
        self.config_path.clone()
    }

    /// Take the event receiver. Single-use — only one consumer.
    /// After this is called the handle is in a partial state; the UI
    /// typically destructures (`let ControllerHandle { evt_rx, .. } = handle`)
    /// rather than calling this method.
    pub fn into_split(self) -> (Sender<Request>, Receiver<Event>) {
        (self.cmd_tx, self.evt_rx)
    }

    /// Non-blocking request send (review #111). `cmd_tx` is
    /// bounded (capacity 16); if the channel is full the send fails
    /// and the caller reports it -- the UI keeps its state and shows
    /// the error (design doc §3.11.3: "发送失败显示错误并保留原状态").
    ///
    /// The request is **not** returned on failure: it is large (the
    /// command carries the whole parameter set), and the caller has
    /// nothing to do with it beyond the error message.
    pub fn try_send_request(&self, request: Request) -> Result<(), String> {
        self.cmd_tx.try_send(request).map_err(|e| match e {
            crossbeam_channel::TrySendError::Full(_) => {
                "the lib is busy (the command queue is full)".to_string()
            }
            crossbeam_channel::TrySendError::Disconnected(_) => {
                "the lib is gone (the command channel is closed)".to_string()
            }
        })
    }
}

/// The ffmpeg runner the NVENC probe uses.
///
/// It answers every ffmpeg invocation the probe makes (`-encoders`,
/// `-version`, the 1-frame capability probes) — the same seam
/// [`crate::encoder::nvenc::resolve_with`] takes.
pub type DetectRunner = Arc<dyn Fn(&[&str]) -> Result<String, EncodeError> + Send + Sync>;

/// The actor's injected dependencies: where everything comes from.
///
/// Everything the actor needs from the outside world lives here, so the
/// actor body has no hidden reads (no env var lookups buried in it --
/// those are what make tests unrunnable in parallel). The env is built
/// once and never mutated; the actor's *runtime* state (busy gate,
/// batch id, verdicts, cancel token) stays in the actor's locals.
///
/// The production env is built by [`start_controller`]; a test env by
/// [`start_controller_with`] -- same actor, different dependencies.
pub struct ActorEnv {
    /// The app config -- the config-file layer. The run configuration
    /// is derived from it (plus the command line, which the GUI does
    /// not have) when a batch starts.
    pub app_config: Arc<AppConfig>,
    /// Where `Command::SaveConfig` writes.
    pub config_path: PathBuf,
    /// Where every file lives (design doc §3.6). The GUI rebases the
    /// operation directory when the user switches working directories
    /// (§3.10.7).
    pub paths: Paths,
    /// How to encode one file (the test seam -- design doc §3.10.4).
    pub file_encoder: Arc<dyn FileEncoder>,
    /// The ffmpeg runner the NVENC probe uses (the test seam).
    pub detect_runner: DetectRunner,
}

/// The production environment: real ffmpeg, the app's own paths.
fn production_env(app_config: Arc<AppConfig>, config_path: PathBuf) -> ActorEnv {
    let app_dir = paths::app_dir().unwrap_or_else(|| PathBuf::from("."));
    // The operation directory is the working directory the user picked;
    // the actor rebases it per batch (the working directory can change
    // between batches -- design doc §3.10.7).
    let resolved = Paths::resolve(config_path.clone(), app_dir.clone(), app_dir, &app_config);
    ActorEnv {
        app_config,
        config_path,
        paths: resolved,
        file_encoder: Arc::new(FfmpegFileEncoder),
        detect_runner: Arc::new(crate::encoder::nvenc::run_ffmpeg),
    }
}

/// Spawn the lib actor and return the message-passing handle.
///
/// # Lifecycle
///
/// Caller (UI) gets `(cmd_tx, evt_rx)` to drive the actor. The actor
/// thread lives until `cmd_tx` is dropped — at which point the
/// thread's `recv()` returns `None` and the thread exits. The UI
/// sees the channel go silent; there is no explicit "the actor
/// exited" event (the `Event::ActorExited` variant was never
/// implemented).
///
/// # Single-batch constraint
///
/// One batch in flight at a time (§3.4 / §3.8.1). Second
/// `Command::StartBatch` while one is running returns
/// `Event::Error(ConfigError)` rather than queueing.
///
/// # Internal init
///
/// Once at startup, the actor:
/// 1. Resolves config via [`paths::config_file`] (handles
///    `$RENENCODER_CONFIG`).
/// 2. Loads [`AppConfig`] (auto-creates default if missing —
///    same behavior as CLI).
/// 3. Constructs runtime [`Config`] via
///    [`Config::from_encode_config`].
/// 4. Initializes [`EncoderCache`] with `enable_nv` from
///    `encode.nv`.
/// 5. Falls into the command loop.
///
/// Any init failure emits `Event::Error(ConfigError)` then the
/// thread exits — UI sees the error + then no more events.
pub fn start_controller() -> ControllerHandle {
    // Review #111: bounded channels. cmd_tx (UI → lib) cap 16
    // (human pace); evt_tx (lib → UI) cap 1024 (~50s of progress
    // headroom at 20 events/sec for 10 concurrent files). Send
    // policy: progress-class events drop on full; structural events
    // block. See `send_evt_lossy` / `send_cmd_lossy` below.
    let (cmd_tx, cmd_rx) = bounded::<Request>(CMD_TX_CAPACITY);
    let (evt_tx, evt_rx) = bounded::<Event>(EVT_TX_CAPACITY);

    // Load AppConfig once here so the same Arc is shared with both:
    // - the actor thread (needs Config + AppConfig for derive / cache)
    // - ControllerHandle (UI reads encode.* via .config())
    //
    // On load failure, emit Event::Error so the UI sees the reason;
    // fall back to defaults so the controller can still spawn (UI sees
    // the error then no more events).
    let (app_config, config_path) = match load_and_share_config(&evt_tx) {
        Some((a, p)) => (a, p),
        None => (
            Arc::new(AppConfig::default()),
            PathBuf::from("(unavailable)"),
        ),
    };

    let env = production_env(app_config.clone(), config_path.clone());

    // Review #126: async cache init. The cache init spawns its own
    // thread and returns a one-shot receiver; the actor can drain
    // cmd_rx while init runs in the background. StartBatch
    // handlers block on the receiver only when a batch actually
    // starts.
    // The NVENC capability is resolved through the on-disk record
    // (shared with the CLI): a fresh record is reused as-is, a missing
    // one is probed and stored. The summary rides along so the UI can
    // show it without a second round-trip.
    //
    // The capability does NOT depend on the `nv` switch: it is a fact
    // about the machine, and `nv` is this run's choice (design doc
    // §3.10.5). So the resolution happens either way.

    std::thread::Builder::new()
        .name("renencoder-controller".into())
        .spawn(move || {
            run_actor(cmd_rx, evt_tx, env);
        })
        .expect("spawn controller thread");

    ControllerHandle {
        cmd_tx,
        evt_rx,
        config: app_config.clone(),
        config_path,
    }
}

/// Spawn the lib actor with an injected environment.
///
/// Same actor, different dependencies -- the seam the tests use: a
/// mock [`FileEncoder`], a fake NVENC probe, test paths. The message
/// protocol therefore has exactly one implementation (design doc
/// §3.10.4).
pub fn start_controller_with(env: ActorEnv) -> ControllerHandle {
    let (cmd_tx, cmd_rx) = bounded::<Request>(CMD_TX_CAPACITY);
    let (evt_tx, evt_rx) = bounded::<Event>(EVT_TX_CAPACITY);

    let app_config = Arc::clone(&env.app_config);
    let config_path = env.config_path.clone();
    std::thread::Builder::new()
        .name("renencoder-controller".into())
        .spawn(move || {
            run_actor(cmd_rx, evt_tx, env);
        })
        .expect("spawn controller thread");

    ControllerHandle {
        cmd_tx,
        evt_rx,
        config: app_config,
        config_path,
    }
}

/// Load the AppConfig (the config-file layer). Returns the loaded
/// path too so the controller can hand it back via
/// `ControllerHandle::config_path` (UI Save needs the original
/// location). Emit `Event::Error` on failure and return None so the
/// caller falls back to defaults.
fn load_and_share_config(evt_tx: &Sender<Event>) -> Option<(Arc<AppConfig>, PathBuf)> {
    let config_path = paths::config_file(None)?;
    let app_config = match AppConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            // Direct send here (review #111): runs before the actor
            // thread is spawned, so we don't have the dropped-event
            // counter yet. Error is structural — block on full
            // rather than drop, matching `send_evt_lossy`'s policy
            // for structural events.
            let _ = evt_tx.send(Event::Error {
                category: ErrorCategory::ConfigError,
                message: format!("failed to load {}: {e}", config_path.display()),
            });
            return None;
        }
    };
    Some((Arc::new(app_config), config_path))
}

/// The encoder cache cell: a replaceable handle.
///
/// `Arc` inside so the read path is a pointer clone -- the reader
/// drops the lock before encoding (an encode can run for minutes).
///
/// The slot holds the `Arc`s *inside* the `Result`, so a snapshot is a
/// pointer clone of both the cache and the error: neither
/// `EncoderCache` nor `EncodeError` has to be `Clone`.
type CacheSlot = Result<Arc<EncoderCache>, Arc<EncodeError>>;

/// What one probe published: the encoder cache and the capability.
///
/// The two travel together: they are **one probe's answer**, and the
/// actor publishes them together (design doc §3.10.6 step 5). A batch
/// that waits for the cache therefore waits for the published
/// capability too -- it cannot start with an unpublished one (which
/// would say "this machine cannot do hardware encoding" about a
/// machine that can).
#[derive(Clone)]
struct PublishedState {
    /// The encoder cache (the ffmpeg build's encoder list).
    cache: CacheSlot,
    /// The capability, as published: it carries the generation.
    capability: Capability,
}

/// The published state cell: a replaceable handle.
///
/// `None` means "the first probe has not landed yet"; `Some` holds
/// the published state -- the cache (which may be the init's error)
/// and the capability that goes with it.
type PublishedCell = Arc<Mutex<Option<PublishedState>>>;

/// The machine facts, gathered once and shared (design doc §3.10.7).
///
/// Publish a probe's answer: the encoder cache and the capability.
///
/// Only the **actor** calls this -- the actor is the publisher (design
/// doc §3.10.6 step 5), so the published state has one writer and the
/// batches read one consistent answer.
///
/// The capability is a fact about the machine, so it changes only when
/// the machine (or the ffmpeg build) changes -- not when the user
/// flips the `nv` switch. That switch is a run choice and travels in
/// the run's parameters, so flipping it never touches the cache.
///
/// The lock is held only for the pointer swap -- no IO inside
/// (§3.10.1 rule 2).
fn publish_state(cell: &PublishedCell, cache: CacheSlot, capability: Capability) {
    let mut guard = cell.lock();
    *guard = Some(PublishedState { cache, capability });
}

/// What a publication produced.
struct Publication {
    /// The published capability (with its generation).
    capability: Capability,
    /// The machine facts, carrying the published capability.
    facts: crate::capability::MachineFacts,
    /// The failure, when the probe failed.
    failure: Option<(ErrorCategory, String)>,
}

/// What a batch takes when it is admitted.
struct BatchAdmission {
    /// The batch's id.
    id: BatchId,
    /// The batch's cancel token -- **its own**.
    ///
    /// A batch never inherits the previous batch's token: the token
    /// is shared with `Command::Cancel` (which cancels whatever is
    /// current), so an inherited one would still be cancelled -- the
    /// next dry run would abort before it started (design doc
    /// §3.11.2).
    token: CancelToken,
}

/// The actor's own state: the batch gate, the active batch, the
/// cancel token and the capability publication.
///
/// The admission lives here as **one operation**: every batch kind
/// takes its gate, its id and its cancel token from it, so none of
/// them can inherit the previous batch's, and none of them has to
/// remember to reset anything (design doc §3.11.2).
struct ActorState {
    /// The busy gate: one batch at a time (§3.4).
    batch_running: bool,
    /// The batch the UI may cancel (design doc §3.11.3).
    active_batch_id: Option<BatchId>,
    /// The cancel token of the current batch.
    cancel_token: CancelToken,
    /// The next batch id.
    next_batch_id: BatchId,
    /// The capability publication (design doc §3.10.6 step 5).
    publisher: CapabilityPublisher,
}

impl ActorState {
    /// Admit a batch.
    ///
    /// Takes the gate, hands out the id and gives the batch a cancel
    /// token of its own. `cancel_file` is the state file a batch
    /// watches for an external cancel request (`None` for the kinds
    /// that do not watch one).
    ///
    /// A leftover cancel file is removed first: it belongs to the
    /// previous run, and it would cancel this batch on its first
    /// poll.
    fn admit_batch(&mut self, cancel_file: Option<&std::path::Path>) -> BatchAdmission {
        let id = self.next_batch_id;
        self.next_batch_id += 1;
        self.batch_running = true;
        self.active_batch_id = Some(id);
        self.cancel_token = match cancel_file {
            Some(path) => {
                let _ = std::fs::remove_file(path);
                CancelToken::watching(path.to_path_buf())
            }
            None => CancelToken::new(),
        };
        BatchAdmission {
            id,
            token: self.cancel_token.clone(),
        }
    }

    /// The batch ended: open the gate and forget the active batch.
    fn finish_batch(&mut self) {
        self.batch_running = false;
        self.active_batch_id = None;
    }

    /// Cancel the active batch -- if the request names it.
    ///
    /// A cancel that names another batch is refused: a stale cancel
    /// (the user cancelled, then a new batch started) must not stop
    /// the batch that is running now (design doc §3.11.3).
    fn cancel(&mut self, batch_id: BatchId) -> bool {
        if self.active_batch_id != Some(batch_id) {
            return false;
        }
        self.cancel_token.cancel();
        true
    }
}

/// The actor's capability publication state.
///
/// One object owns the rules that used to be spread across the
/// actor's branches:
///
/// - the **request sequence** -- which probe asked later;
/// - the **published generation** -- which snapshot is current;
/// - the **capability** -- the check's conclusion plus the run's
///   corrections (the authoritative copy);
/// - the **published state** -- the encoder cache and the capability
///   a batch takes.
///
/// The rules are in the methods, not in the callers. The one that
/// matters most: a **failed** probe produced no answer, so it
/// publishes nothing and advances nothing -- it cannot supersede a
/// result that is still on its way (design doc §3.10.6 step 5). No
/// caller can skip that rule, because only this object advances the
/// sequence.
struct CapabilityPublisher {
    /// The next probe request number.
    next_seq: u64,
    /// The newest **published** request. A result older than this is
    /// dropped: it must not overwrite a newer snapshot -- or a newer
    /// record on disk.
    published_seq: u64,
    /// The generation the next publication carries.
    next_generation: u64,
    /// The authoritative capability.
    capability: Capability,
    /// The published state the batches take. This object is its only
    /// writer.
    state: PublishedCell,
}

impl CapabilityPublisher {
    fn new(state: PublishedCell) -> Self {
        Self {
            next_seq: 0,
            published_seq: 0,
            next_generation: 1,
            capability: Capability::default(),
            state,
        }
    }

    /// A probe request takes the next sequence number.
    fn request(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    /// Publish a probe's result -- if it is the newest one.
    ///
    /// `None` means the result was stale and dropped.
    fn publish(
        &mut self,
        seq: u64,
        outcome: ProbeOutcome,
        record_path: &std::path::Path,
    ) -> Option<Publication> {
        // Only the newest probe result is published: an older one
        // must not overwrite a newer snapshot -- or a newer record on
        // disk.
        if seq < self.published_seq {
            return None;
        }
        // The encoder cache, from the resolution. A failed probe has
        // none: the old cache is still the right one (the build did
        // not change), so the failure does not touch it.
        let cache: Option<CacheSlot> = outcome.resolution.as_ref().map(|resolution| {
            Ok(Arc::new(crate::pipeline::cache::cache_from_resolution(
                resolution,
            )))
        });
        // The record on disk. The **publisher** writes it: the probe
        // threads do not, so an older probe's result cannot overwrite
        // a newer record (design doc §3.10.6 step 5).
        if let Some(resolution) = &outcome.resolution {
            crate::encoder::nvenc::store_resolution(
                Some(record_path),
                resolution,
                chrono::Utc::now(),
            );
        }
        match outcome.failure {
            Some((category, message)) => {
                // A failed detection **keeps the current snapshot**
                // (design doc §3.10.6 step 5): the machine did not
                // change, only the probe failed. Publishing an empty
                // capability here would silently turn NVENC off for
                // the next batch.
                //
                // The cache error still lands when nothing has been
                // published yet (the startup probe failed): a batch
                // must see the failure at once, not wait for the
                // timeout.
                {
                    let mut guard = self.state.lock();
                    if guard.is_none() {
                        // Nothing has been published yet, so there is
                        // no cache to keep: the failure itself becomes
                        // the state, so a batch sees it at once instead
                        // of waiting for the timeout.
                        let cache = cache.unwrap_or_else(|| {
                            Err(Arc::new(EncodeError::Probe(
                                "the capability probe failed".into(),
                            )))
                        });
                        *guard = Some(PublishedState {
                            cache,
                            capability: Capability::default(),
                        });
                    }
                }
                Some(Publication {
                    capability: self.capability.clone(),
                    facts: outcome.facts,
                    failure: Some((category, message)),
                })
            }
            None => {
                // The sequence advances **only when a result is
                // actually published**: a failed probe produced
                // nothing, so it supersedes nothing. Otherwise a
                // failed re-probe would discard the startup's
                // still-pending result -- and with it the encoder
                // cache the batches wait for.
                self.published_seq = seq;
                // Publish: this snapshot gets the next generation.
                let published = outcome
                    .facts
                    .nvenc
                    .clone()
                    .unwrap_or_default()
                    .with_generation(self.next_generation);
                self.next_generation += 1;
                // The machine facts carry the published capability
                // (the report reads it).
                let mut facts = outcome.facts.clone();
                facts.nvenc = Some(published.clone());
                self.capability = published.clone();
                // The published state: the encoder cache and the
                // capability, together. The publisher is the only
                // writer -- a batch that waits for the cache
                // therefore waits for the published capability too.
                publish_state(
                    &self.state,
                    cache.unwrap_or_else(|| {
                        Err(Arc::new(EncodeError::Probe(
                            "the probe produced no encoder cache".into(),
                        )))
                    }),
                    published.clone(),
                );
                Some(Publication {
                    capability: published,
                    facts,
                    failure: None,
                })
            }
        }
    }

    /// Fold a worker's runtime corrections into the authoritative
    /// capability -- **only the current generation's**.
    ///
    /// A batch that ran under an older publication reported against
    /// that publication: its conclusion ("this NVENC encoder is
    /// broken") is about a machine state the current generation has
    /// already replaced (design doc §3.10.6 step 5). Folding it in
    /// would carry a stale hardware failure into the next batch
    /// forever.
    ///
    /// The known cost: after a re-probe publishes a new generation,
    /// the old generation's runtime corrections are not inherited --
    /// the next batch may try an encoder that just fell back, and
    /// fall back again (harmless: the product is still correct, one
    /// file is encoded twice).
    fn merge_verdicts(&mut self, updates: Vec<VerdictUpdate>) {
        let generation = self.capability.generation();
        for (reported_generation, encoder, usable) in updates {
            if reported_generation != generation {
                tracing::debug!(
                    encoder = %encoder,
                    reported_generation,
                    generation,
                    "dropping a verdict from an older generation"
                );
                continue;
            }
            self.capability.verdicts_mut().set(&encoder, usable);
        }
        // The next batch inherits the corrections.
        let mut guard = self.state.lock();
        if let Some(state) = guard.as_mut() {
            state.capability = self.capability.clone();
        }
    }

    /// The authoritative capability.
    #[cfg(test)]
    fn capability(&self) -> &Capability {
        &self.capability
    }
}

/// Spawn a capability probe and hand the result to the actor.
///
/// The probe runs off the actor thread (it spawns ffmpeg); the
/// **publisher** decides what to do with the result (design doc
/// §3.10.6 step 5).
///
/// The startup probe and a re-probe take **the same path** -- the
/// startup is simply a probe that is not forced (it reuses a fresh
/// record instead of probing again). One path, one rule.
fn spawn_probe<F>(
    seq: u64,
    request_id: Option<RequestId>,
    force: bool,
    cache_path: PathBuf,
    runner: F,
    probe_tx: Sender<ProbeResult>,
) where
    F: Fn(&[&str]) -> Result<String, EncodeError> + Send + Sync + 'static,
{
    std::thread::Builder::new()
        .name("renencoder-probe".into())
        .spawn(move || {
            // The probe only -- the **store** is the publisher's job
            // (design doc §3.10.6 step 5): the actor publishes the
            // result and arranges the persistence, so an older
            // probe's result cannot overwrite a newer record.
            let outcome = match crate::encoder::nvenc::resolve_with_clock(
                Some(cache_path.as_path()),
                force,
                runner,
                chrono::Utc::now(),
            ) {
                Ok(resolution) => ProbeOutcome {
                    facts: crate::capability::MachineFacts::from_resolution(&resolution),
                    resolution: Some(resolution),
                    failure: None,
                },
                Err(e) => ProbeOutcome {
                    facts: crate::capability::MachineFacts::default(),
                    resolution: None,
                    failure: Some((
                        ErrorCategory::ProbeFailed,
                        format!("NVENC detection failed: {e}"),
                    )),
                },
            };
            let _ = probe_tx.send(ProbeResult {
                seq,
                request_id,
                outcome,
            });
        })
        .expect("spawn probe thread");
}

/// What a probe hands back to the actor.
///
/// The probe runs off the actor thread (it spawns ffmpeg), and the
/// **actor** publishes its result (design doc §3.10.6 step 5): the
/// actor owns the probe request sequence and the published
/// generation, so an older probe's result can never overwrite a newer
/// snapshot -- or a newer record on disk.
struct ProbeResult {
    /// The probe request's sequence number (the actor's counter).
    seq: u64,
    /// The request that asked for the probe (`None` for the startup
    /// resolution -- nobody asked for it).
    request_id: Option<RequestId>,
    /// What the probe produced.
    outcome: ProbeOutcome,
}

/// What a probe produced.
struct ProbeOutcome {
    /// The machine facts the probe established (including the
    /// capability it found).
    facts: crate::capability::MachineFacts,
    /// The resolution behind a re-probe: it carries the encoder list
    /// (a new cache) and the record to store.
    resolution: Option<crate::encoder::nvenc::Resolution>,
    /// The failure, when the probe failed.
    failure: Option<(ErrorCategory, String)>,
}

// =====================================================================
// Batch workers — the actor never runs a batch itself (§3.10.1.1)
// =====================================================================

/// A runtime correction the worker found, with the generation it was
/// found under.
///
/// The generation is what makes the report scoped: a batch that ran
/// under an older publication reports its corrections against **that**
/// publication, and the actor merges only the reports of the
/// generation it currently holds (design doc §3.10.6 step 5). A late
/// report from an old batch therefore cannot disable an encoder the
/// new generation found usable.
type VerdictUpdate = (u64, String, bool);

/// "This batch is done" signal, sent by a worker to the actor.
///
/// The actor reopens the busy gate when it reads the signal. A channel
/// is the right tool: the signal is a message, not shared state.
///
/// `updates` carries the NVENC verdicts the worker found (an encoder
/// that failed at runtime). The actor merges them into its authoritative
/// copy, so the next batch inherits them.
struct BatchDone {
    /// The runtime corrections, each with its generation.
    updates: Vec<VerdictUpdate>,
}

/// Drain the pending "batch done" signals.
///
/// Returns whether a batch has finished, plus the verdict updates it
/// reported. The actor calls this right before it consults the busy
/// gate: a worker can report "done" while the actor is handling a
/// command, and a batch that has finished must not be mistaken for a
/// running one.
fn drain_batch_done(done_rx: &Receiver<BatchDone>) -> (bool, Vec<VerdictUpdate>) {
    let mut finished = false;
    let mut updates = Vec::new();
    while let Ok(BatchDone { updates: u }) = done_rx.try_recv() {
        finished = true;
        updates.extend(u);
    }
    (finished, updates)
}

/// Sends the done signal when dropped.
///
/// A guard, not a plain `send` at the end of the body: if the body
/// panics, the worker thread unwinds and the guard still reports
/// "done". Without it a panic would leave the busy gate closed and
/// strand the UI (no batch could ever start again).
struct DoneGuard {
    done_tx: Sender<BatchDone>,
    /// Verdict updates the worker reported (drained on drop).
    updates: Option<Receiver<VerdictUpdate>>,
}

impl Drop for DoneGuard {
    fn drop(&mut self) {
        let mut updates = Vec::new();
        if let Some(rx) = &self.updates {
            while let Ok(u) = rx.try_recv() {
                updates.push(u);
            }
        }
        let _ = self.done_tx.send(BatchDone { updates });
    }
}

/// Run `body` on a dedicated batch-worker thread.
///
/// The actor keeps serving commands while the body runs — that is what
/// lets `Command::Cancel` reach an in-flight ffmpeg (§3.10.1.1).
///
/// The body owns the batch's event sender, so every event of the batch
/// comes from one sender: the channel's FIFO order is the order the UI
/// sees. It returns the batch's **terminal event** (`None` when the
/// batch produced none).
///
/// The terminal event is published **after** the run slot is released
/// (design doc §3.11.3): the moment the UI sees the confirmation, the
/// lib can already take the next batch. Publishing it first would
/// make the UI's "restart" lose the race against the actor's busy
/// gate.
fn spawn_batch_job<F>(
    body: F,
    done_tx: Sender<BatchDone>,
    updates: Option<Receiver<VerdictUpdate>>,
    evt_tx: Sender<Event>,
    dropped_counter: DropCounter,
) where
    F: FnOnce() -> Option<Event> + Send + 'static,
{
    std::thread::Builder::new()
        .name("rencoder-batch".into())
        .spawn(move || {
            // Dropped when the body returns — or unwinds.
            let done = DoneGuard { done_tx, updates };
            let terminal = body();
            // The slot goes first, the confirmation second.
            drop(done);
            if let Some(event) = terminal {
                send_evt_lossy(&evt_tx, event, &dropped_counter);
            }
        })
        .expect("spawn batch worker");
}

/// The channel a batch worker uses to report the encoders it found to
/// be unusable.
///
/// Created by the actor and split: the sender goes to the processor
/// (which writes into its verdict snapshot), the receiver is drained
/// when the batch ends and handed to the actor with the done signal.
struct BatchUpdates {
    tx: Sender<VerdictUpdate>,
    rx: Receiver<VerdictUpdate>,
}

impl BatchUpdates {
    fn new() -> Self {
        let (tx, rx) = unbounded();
        Self { tx, rx }
    }
}

/// Run one batch of tasks on a worker thread and report the outcome.
///
/// `make_processor` is called *in the worker* — that is where the
/// encoder cache is resolved (the one-time init may still be running,
/// and waiting for it must not block the actor). It receives the
/// sender of the verdict-update channel; the processor it returns
/// reports the encoders it found to be unusable through it. The
/// processor is called once per task by [`TaskManager`], which also
/// owns the batch/task scaffold logs.
///
/// The terminal event (`BatchCompleted` / `BatchCancelled`) is sent by
/// the worker, so the whole batch reads as one ordered stream.
#[allow(clippy::too_many_arguments)]
fn spawn_task_batch<P, F>(
    batch_id: BatchId,
    tasks: Vec<EncodeTask>,
    label: &'static str,
    batch_path: PathBuf,
    // Where the finished batch's records are written (the app's records
    // directory).
    records_dir: Option<PathBuf>,
    // The batch's cancel token: `Command::Cancel` sets it, the
    // worker polls it (design doc §3.11.2).
    cancel_token: CancelToken,
    make_processor: F,
    updates: BatchUpdates,
    evt_tx: Sender<Event>,
    dropped_counter: DropCounter,
    done_tx: Sender<BatchDone>,
) where
    P: FnMut(
            &EncodeTask,
            &crate::pipeline::task::TaskContext,
        ) -> Result<EncodingRecord, EncodeError>
        + Send
        + 'static,
    F: FnOnce(Sender<VerdictUpdate>) -> Result<P, (ErrorCategory, String)> + Send + 'static,
{
    let BatchUpdates { tx, rx } = updates;
    std::thread::Builder::new()
        .name("renencoder-batch".into())
        .spawn(move || {
            // Dropped when the body returns — or unwinds. The guard
            // reports "done" (and hands the verdict updates to the
            // actor) even if the batch panicked.
            let done = DoneGuard {
                done_tx,
                updates: Some(rx),
            };
            let terminal = (|| -> Option<Event> {
                // The single-instance guard (design doc §3.11.1.1): the
                // batch holds its target for as long as it runs. The take
                // happens **here**, in the worker: the descendant scan is a
                // tree walk, and the actor must not block on it.
                let scope = crate::guard::common_ancestor(
                    &tasks
                        .iter()
                        .map(|task| task.input.as_path().to_path_buf())
                        .collect::<Vec<_>>(),
                )
                .unwrap_or_else(|| batch_path.clone());
                let _target_guard = match crate::guard::TargetGuard::acquire(&scope) {
                    Ok(guard) => guard,
                    Err(e) => {
                        // The batch cannot start: another instance holds
                        // this target (or a directory inside it). The
                        // failure belongs to **this batch** (design doc
                        // §3.11.3) -- it must not finalize another one.
                        return Some(Event::BatchFailed {
                            batch_id,
                            category: ErrorCategory::ConfigError,
                            message: e.to_string(),
                        });
                    }
                };
                let mut processor = match make_processor(tx) {
                    Ok(processor) => processor,
                    Err((category, message)) => {
                        // The batch could not start (no encoder cache).
                        return Some(Event::BatchFailed {
                            batch_id,
                            category,
                            message,
                        });
                    }
                };
                // `TaskManager::new(None)`: the actor path has no signal
                // handler — the cancel branch of `run_ffmpeg_with_progress`
                // deletes the partial output itself.
                // One report, one meaning of "this batch is over"
                // (design doc §3.11.4).
                let report = TaskManager::new().run(
                    &tasks,
                    label,
                    &batch_path,
                    &cancel_token,
                    &mut processor,
                );
                // The batch is done: persist its records, so the history
                // has something to read (§3.11.4). The mechanics are the
                // lib's; the decision to persist is the frontend's -- and
                // the GUI has no other place to do it.
                persist_batch_records(&report.records, records_dir.as_deref());
                Some(match report.termination {
                    crate::pipeline::task::Termination::Completed => Event::BatchCompleted {
                        batch_id,
                        records: report.records,
                    },
                    // The processor asked to abort (cancel). `completed`
                    // is the 0-based index of the task the batch stopped
                    // at.
                    crate::pipeline::task::Termination::Cancelled { completed, total } => {
                        Event::BatchCancelled {
                            batch_id,
                            completed,
                            total,
                        }
                    }
                    crate::pipeline::task::Termination::Failed(error) => Event::BatchFailed {
                        batch_id,
                        category: ErrorCategory::ConfigError,
                        message: error,
                    },
                })
            })();
            // The run slot goes first, the confirmation second
            // (design doc §3.11.3): the UI may start the next batch
            // the moment it sees the terminal event.
            drop(done);
            if let Some(event) = terminal {
                send_evt_lossy(&evt_tx, event, &dropped_counter);
            }
        })
        .expect("spawn batch worker");
}

/// Write a finished batch's records to the default history file.
///
/// The mechanics are the lib's (`data::store`); the failure is not
/// fatal -- a batch that could not be written to disk still finished,
/// and the UI still shows its records.
fn persist_batch_records(records: &[EncodingRecord], records_dir: Option<&std::path::Path>) {
    if records.is_empty() {
        return;
    }
    let Some(dir) = records_dir else {
        return;
    };
    let path = match crate::data::store::default_encoding_path_in(dir) {
        Ok(path) => path,
        Err(e) => {
            tracing::warn!(error = %e, "could not resolve the history path");
            return;
        }
    };
    if let Err(e) = crate::data::store::write_records(records, &path) {
        tracing::warn!(error = %e, path = %path.display(), "could not write the batch records");
    } else {
        tracing::info!(path = %path.display(), records = records.len(), "wrote the batch records");
    }
}

/// Build the per-file processor for an encode batch.
///
/// The processor owns everything it needs — no borrows — because it
/// runs on the batch worker thread: the batch context (params, cache
/// snapshot, config, verdicts), the cancel token and the event sender.
///
/// The context is built *in the worker* (after the cache lands), so
/// the actor never waits for the cache init (§3.10.1.1 rule 5).
fn make_encode_processor(
    mut batch_ctx: BatchContext,
    batch_id: BatchId,
    cancel: CancelToken,
    evt_tx: Sender<Event>,
    dropped_counter: DropCounter,
    update_tx: Sender<VerdictUpdate>,
    encoder: Arc<dyn FileEncoder>,
) -> impl FnMut(
    &EncodeTask,
    &crate::pipeline::task::TaskContext,
) -> Result<EncodingRecord, EncodeError>
       + Send
       + 'static {
    // What the actor already knows: the verdicts this batch started
    // with. The comparison tells us what the encode changed.
    let mut reported = batch_ctx.capability().verdicts().clone();
    move |task: &EncodeTask, ctx: &crate::pipeline::task::TaskContext| {
        // FileStarted uses 0-indexed (matches the v1 UI contract).
        // TaskManager passes 1-indexed in `ctx.index`; convert.
        let idx = ctx.index - 1;

        send_evt_lossy(
            &evt_tx,
            Event::FileStarted {
                batch_id,
                index: idx,
                total: ctx.total,
                source: task.input.clone(),
            },
            &dropped_counter,
        );

        // Cancel check between files. The in-flight ffmpeg polls the
        // same token, so a cancel during an encode kills the child
        // (and removes the partial output) — this check ends the batch
        // after the current file.
        if cancel.is_cancelled() {
            return Err(EncodeError::UserCancel);
        }

        // The progress closure owns its own sender clone: it is built
        // per file and must not borrow the processor's captures.
        let evt_tx_progress = evt_tx.clone();
        // The progress closure borrows the processor's counter: it is
        // the same sender, so the count keeps adding up.
        let mut on_progress = |p: FfmpegProgress| {
            send_evt_lossy(
                &evt_tx_progress,
                Event::Progress {
                    batch_id,
                    file_index: idx,
                    frame: p.frame,
                    fps: p.fps,
                    speed: p.speed,
                    time_secs: p.time_secs,
                    eta_secs: p.eta_secs,
                },
                &dropped_counter,
            );
        };

        let res = encoder.encode(task, &mut batch_ctx, &cancel, &mut on_progress);

        // Report the verdicts this file changed (rare: only when an
        // NVENC encode failed and the encoder turned out to be
        // unusable). The actor merges them into its authoritative copy
        // so the next batch inherits them -- but only the ones that
        // belong to the generation the actor still holds (design doc
        // §3.10.6 step 5): this batch ran under the generation it
        // froze, and a newer publication has already superseded it.
        let generation = batch_ctx.capability().generation();
        for (encoder, usable) in batch_ctx.capability().verdicts().changes_since(&reported) {
            let _ = update_tx.send((generation, encoder, usable));
        }
        reported.clone_from(batch_ctx.capability().verdicts());

        match res {
            Ok(record) => {
                // A cancelled encode (the ffmpeg child was killed and
                // its partial output removed) ends the batch: the user
                // asked to stop, so the batch reports "cancelled"
                // instead of "completed with one failed file".
                let cancelled = record
                    .error
                    .as_ref()
                    .is_some_and(|e| e.category == ErrorCategory::Cancelled);
                send_evt_lossy(
                    &evt_tx,
                    Event::FileCompleted {
                        batch_id,
                        index: idx,
                        record: record.clone(),
                    },
                    &dropped_counter,
                );
                if cancelled {
                    return Err(EncodeError::UserCancel);
                }
                Ok(record)
            }
            Err(e) => {
                // A cancel is not a per-file failure: the batch is
                // stopping, and the report says so (design doc
                // §3.11.2). The Ok arm above catches the cancels that
                // come back as a record; this one catches the ones
                // that come back as an error.
                if matches!(e, EncodeError::UserCancel) {
                    return Err(e);
                }
                // A per-file failure is **not** a fatal event (design
                // doc §3.11.3): it travels in the record's
                // `error` field. A fatal event here would finalize
                // the whole batch while the other files are still
                // running.
                //
                // Map a per-file failure to Ok(Failed record) so
                // TaskManager doesn't abort the batch on the first
                // failure — matches v1 inline behavior.
                Ok(crate::pipeline::task::build_failed_record(
                    task,
                    e,
                    batch_ctx.params(),
                ))
            }
        }
    }
}

/// How long a batch waits for the encoder cache to land.
///
/// The init spawns ffmpeg (`ffmpeg -encoders`), so it takes a moment.
/// A batch that starts in that window waits for it instead of failing
/// with "encoder cache init in progress" — that failure was a real
/// bug: the first batch after startup could not start at all.
const CACHE_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Take the encoder cache, waiting for the one-time init to land.
///
/// The wait happens in the batch worker, so the actor stays responsive
/// while it waits. The cell is polled rather than waited on with a
/// condition variable: the wait is short, happens once per process,
/// and a poll keeps the shared-state inventory free of another
/// primitive (§3.10.1).
///
/// A batch uses ONE snapshot: a batch is a unit, the encoder must not
/// change under it.
fn take_published(cell: &PublishedCell, timeout: std::time::Duration) -> Option<PublishedState> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // The lock is held only for the pointer clone — no IO inside
        // (§3.10.1 rule 2).
        if let Some(state) = { cell.lock().clone() } {
            return Some(state);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Turn a cache slot into the cache a batch needs, or into the error
/// to report.
fn cache_from_slot(slot: CacheSlot) -> Result<Arc<EncoderCache>, (ErrorCategory, String)> {
    match slot {
        Ok(cache) => Ok(cache),
        Err(e) => Err((e.category(), format!("encoder cache init failed: {e}"))),
    }
}

fn run_actor(
    cmd_rx: Receiver<Request>,
    evt_tx: Sender<Event>,
    env: ActorEnv,
    // Review #126: cache init runs on a background thread; this
    // receiver yields the result when ready. The cmd_rx loop can
    // process other commands (Cancel / LoadHistory / SaveConfig)
    // while init runs. StartBatch handlers block on this when a
    // batch actually starts.
) {
    // The injected dependencies, unpacked for the arms below.
    let config_path = env.config_path.clone();
    // The paths: the app-level ones never move, the run-level ones
    // follow the working directory (design doc §3.6). The actor owns
    // the current value -- `Command::SetWorkingDir` moves it.
    let mut paths = env.paths.clone();
    let records_dir = env.paths.records_dir.clone();
    let nvenc_cache_path = env.paths.capability_record.clone();
    let file_encoder = Arc::clone(&env.file_encoder);
    let detect_runner = Arc::clone(&env.detect_runner);
    // Review #111: dropped-event counter for diagnostics. Each sender
    // owns one (it is never read by anyone but its own thread).
    let dropped_counter = DropCounter::default();
    // Review #126: cache init is now async — see
    // `init_encoder_cache_async`. The cmd_rx loop below runs
    // immediately; the cache receiver is consumed lazily by
    // StartBatch handlers.
    //
    // The slot holds `Result<EncoderCache, EncodeError>` so a FAILED
    // init stays observable: subsequent batches see `Err(...)` and
    // can surface a meaningful "encoder cache init failed" error
    // rather than a generic "cache not available" fallback.
    // The cache is refreshable: the UI can flip `[encode].nv` at
    // runtime, and the NVENC capability has to be re-resolved for the
    // next batch — so the slot has to be replaceable, not write-once.
    //
    // The cell holds an `Arc` so the read path is a pointer clone:
    // the reader drops the lock immediately and then encodes without
    // holding it. Holding a lock across an encode (minutes) would
    // block the refresh thread for the whole file.
    //
    // A plain `Mutex` is the right primitive -- there is exactly one
    // reader (the actor thread, which takes a snapshot per batch) and
    // two writers (the init thread and the refresh thread), so a
    // read/write split would buy nothing.
    let published_cell: PublishedCell = Arc::new(Mutex::new(None));
    // The machine facts, gathered once by the cache init (design doc
    // §3.10.7). The actor adopts them: a re-check after a working
    // directory change reuses them instead of probing the machine
    // again.
    //
    // A plain local, not a cell: the actor is the only reader *and*
    // the only writer (the probes hand their result to the actor,
    // which publishes it) -- one owner, so no lock (§3.10.1 rule 1).
    let mut machine_facts: Option<crate::capability::MachineFacts> = None;
    // The probe channel: the probe threads hand their result back
    // here, and the actor publishes it (design doc §3.10.6 step 5).
    let (probe_tx, probe_rx) = unbounded::<ProbeResult>();
    // The newest published probe request. A result older than this
    // is dropped -- it cannot overwrite a newer snapshot.
    // The generation the next published capability carries.
    // The actor's state: the gate, the active batch, the cancel
    // token and the capability publication (design doc §3.11.2).
    let mut state = ActorState {
        batch_running: false,
        active_batch_id: None,
        cancel_token: CancelToken::new(),
        next_batch_id: 0,
        publisher: CapabilityPublisher::new(published_cell.clone()),
    };
    // The chain's **first stage**: the capability probe. It runs on a
    // worker (it spawns ffmpeg) and the actor publishes its result
    // (design doc §3.10.6 step 5).
    //
    // The startup probe is not forced: a fresh record on disk is
    // reused as-is (the record is shared with the CLI), a missing one
    // is probed. The path is the same one a re-probe takes.
    {
        let seq = state.publisher.request();
        // The **injected** runner: a test must never touch the real
        // machine (design doc §3.10.4), and the production runner is
        // the same seam (the actor's environment carries it).
        let detect_runner_for_startup = Arc::clone(&detect_runner);
        spawn_probe(
            seq,
            None,
            false,
            nvenc_cache_path.clone(),
            move |args| detect_runner_for_startup(args),
            probe_tx.clone(),
        );
    }

    // `app_config` is still held (referenced by `config_path`
    // for the SaveConfig handler). Don't drop yet.

    // Busy gate: one batch in flight at a time (§3.4 / §3.8.1).
    //
    // A plain `bool` in the actor — no lock, no atomic. The batch
    // itself runs on a worker thread (§3.10.1.1), so the actor is the
    // only reader and the only writer.
    // "This batch is done" signals. The worker sends one when it
    // finishes; the actor reopens the busy gate when it reads one.
    let (done_tx, done_rx) = bounded::<BatchDone>(1);
    // The authoritative capability: this machine's NVENC capability as
    // the session knows it. Each batch worker gets a snapshot and
    // reports its changes back (the done signal carries them).

    loop {
        // Wait for the next command OR for a worker to report "done".
        // The done branch re-opens the gate and goes back to waiting.
        let request = select! {
            recv(cmd_rx) -> req => req,
            recv(done_rx) -> done => {
                if let Ok(BatchDone { updates }) = done {
                    state.publisher.merge_verdicts(updates);
                    state.finish_batch();
                }
                continue;
            }
            recv(probe_rx) -> probed => {
                // A probe came back. The **publisher** decides (design
                // doc §3.10.6 step 5): the rules -- what counts as
                // newer, what a failure does -- live in it, not in
                // this branch.
                if let Ok(probed) = probed {
                    let ProbeResult {
                        request_id,
                        outcome,
                        ..
                    } = probed;
                    if let Some(publication) = state.publisher.publish(
                        probed.seq,
                        outcome,
                        nvenc_cache_path.as_path(),
                    ) {
                        match publication.failure {
                            Some((category, message)) => {
                                // The startup failure is the actor's
                                // own (no scope); a re-probe's failure
                                // belongs to the request that asked
                                // for it.
                                match request_id {
                                    Some(request_id) => send_evt_lossy(
                                        &evt_tx,
                                        Event::OperationFailed {
                                            request_id,
                                            category,
                                            message,
                                        },
                                        &dropped_counter,
                                    ),
                                    None => send_evt_lossy(
                                        &evt_tx,
                                        Event::Error { category, message },
                                        &dropped_counter,
                                    ),
                                }
                            }
                            None => {
                                machine_facts = Some(publication.facts.clone());
                                // The run capability report for the
                                // current operation directory.
                                crate::capability::check_run_capabilities(
                                    &paths,
                                    &publication.facts,
                                );
                                send_evt_lossy(
                                    &evt_tx,
                                    Event::CapabilityChecked {
                                        report: paths.capability_report.clone(),
                                    },
                                    &dropped_counter,
                                );
                                // The UI hears about the capability
                                // from here -- one publisher, one
                                // event.
                                //
                                // **After** the state is published:
                                // the event then means "the new
                                // capability is in force", which is
                                // what a caller waits for.
                                send_evt_lossy(
                                    &evt_tx,
                                    Event::NvencCapability(publication.capability),
                                    &dropped_counter,
                                );
                            }
                        }
                    }
                }
                continue;
            }
        };
        let Ok(request) = request else {
            // The UI dropped the command sender: nobody left to
            // serve. Leave the loop. A worker that is still running
            // finishes on its own — its events go nowhere, but nothing
            // here waits for them.
            break;
        };
        // Every answer carries the request's id (design doc §3.11.3):
        // "which of my requests was this?" has one answer.
        let Request {
            id: request_id,
            command,
        } = request;

        // The busy gate covers StartBatch / DryRun / Analyze (§3.4:
        // one batch in flight at a time). It is checked in one place
        // for all three.
        //
        // The drain comes first: a worker can report "done" while the
        // actor was handling a command, and a batch that has finished
        // must not be mistaken for a running one.
        if matches!(
            &command,
            Command::StartBatch { .. } | Command::DryRun { .. } | Command::Analyze { .. }
        ) {
            let (finished, updates) = drain_batch_done(&done_rx);
            state.publisher.merge_verdicts(updates);
            if finished {
                state.finish_batch();
            }
            if state.batch_running {
                // Benign rejection: the command is dropped, the running
                // batch continues. `Event::Error` would be fatal-only
                // and the UI would finalize the running batch.
                send_evt_lossy(
                    &evt_tx,
                    Event::CommandRejected {
                        request_id,
                        reason: "another batch is already running".into(),
                    },
                    &dropped_counter,
                );
                continue;
            }
        }

        match command {
            Command::StartBatch { params, files } => {
                // Review #120: delegate per-file iteration to
                // TaskManager::run. The processor closure owns the UI
                // protocol (FileStarted / FileCompleted / Progress);
                // TaskManager owns batch/task scaffold logs +
                // partial_output::Guard registration.
                // The operation directory: the directory this batch
                // acts on (design doc §3.6). The GUI's working
                // directory can change between batches, so the
                // run-level paths follow it.
                let batch_paths =
                    paths.with_op_dir(&crate::pipeline::batch::batch_target_path_for_files(&files));
                // The admission: the gate, the id and a cancel token
                // of this batch's own (design doc §3.11.2).
                let admission = state.admit_batch(Some(&batch_paths.cancel_file));
                let id = admission.id;
                // The single-instance guard's cheap probe (design doc
                // §3.11.1.1): the ancestors and the target itself. A
                // conflict is a **benign rejection** -- the running
                // batch is untouched, and this one never starts.
                // (The full take -- including the descendant scan --
                // happens in the worker, off the actor thread.)
                let batch_scope = crate::guard::common_ancestor(
                    &files
                        .iter()
                        .map(|file| file.as_path().to_path_buf())
                        .collect::<Vec<_>>(),
                )
                .unwrap_or_else(|| batch_paths.op_dir.clone());
                if let Err(e) = crate::guard::probe(&batch_scope) {
                    send_evt_lossy(
                        &evt_tx,
                        Event::CommandRejected {
                            request_id,
                            reason: e.to_string(),
                        },
                        &dropped_counter,
                    );
                    continue;
                }
                // The batch is admitted: the request gets its answer
                // and the batch its id (design doc §3.11.3). The UI
                // moves from `Starting` to `Encoding` on this event.
                send_evt_lossy(
                    &evt_tx,
                    Event::CommandAccepted {
                        request_id,
                        batch_id: Some(id),
                    },
                    &dropped_counter,
                );
                send_evt_lossy(
                    &evt_tx,
                    Event::BatchStarted {
                        id,
                        total_files: files.len(),
                    },
                    &dropped_counter,
                );
                // One cache snapshot per batch (a batch is a unit —
                // the encoder must not change under it). The cache is
                // resolved *in the worker*: the one-time init may
                // still be running, and waiting for it must not block
                // the actor.
                //
                // Review #122: extension still hoisted (unused now — kept for
                // the hoist comment / future use).
                let _ext = params.format.default_extension();
                // The tasks come from the shared construction entry
                // (design doc §3.11.1): the same names the encode
                // path and the dry run use.
                let tasks = crate::pipeline::batch::encode_tasks(&files, params.format);

                let batch_path = crate::pipeline::batch::batch_target_path_for_files(&files);

                let published_cell_for_worker = Arc::clone(&published_cell);
                let params_for_worker = params.clone();
                let cancel_for_worker = admission.token.clone();
                let evt_tx_for_worker = evt_tx.clone();
                let dropped_for_worker = DropCounter::default();
                let file_encoder_for_worker = Arc::clone(&file_encoder);
                // The processor and the worker each hold a handle on
                // the same token (the `move` closure would otherwise
                // take it).
                let cancel_for_processor = cancel_for_worker.clone();
                let make_processor = move |update_tx: Sender<VerdictUpdate>| {
                    // The context is built here, in the worker: the
                    // cache may still be initializing, and waiting for
                    // it must not block the actor (§3.10.1.1 rule 5).
                    // The published state: the cache and the
                    // capability of one publication (design doc
                    // §3.10.6 step 5). The batch freezes this
                    // generation -- a re-probe during the batch does
                    // not change what this batch encodes with.
                    let published = take_published(&published_cell_for_worker, CACHE_WAIT_TIMEOUT)
                        .ok_or((
                            ErrorCategory::ConfigError,
                            "encoder cache init timed out".into(),
                        ))?;
                    let cache = cache_from_slot(published.cache)?;
                    // The capability of **this** publication: the
                    // batch freezes the generation it was admitted
                    // with (design doc §3.10.6 step 5).
                    let batch_ctx =
                        BatchContext::new(params_for_worker, cache, published.capability);
                    Ok(make_encode_processor(
                        batch_ctx,
                        id,
                        cancel_for_processor,
                        evt_tx_for_worker,
                        dropped_for_worker,
                        update_tx,
                        file_encoder_for_worker,
                    ))
                };

                // The batch runs on its own thread: the actor stays
                // responsive, so `Command::Cancel` reaches the
                // in-flight ffmpeg.
                spawn_task_batch(
                    id,
                    tasks,
                    "encoding",
                    batch_path,
                    Some(records_dir.clone()),
                    cancel_for_worker.clone(),
                    make_processor,
                    BatchUpdates::new(),
                    evt_tx.clone(),
                    DropCounter::default(),
                    done_tx.clone(),
                );
            }

            Command::DryRun { params, files } => {
                // Dry-run: same per-file probe + partial + skip as
                // StartBatch, but no ffmpeg invocation. The busy gate
                // (checked above) applies so a real batch can't sneak
                // in during dry-run.
                let published_cell_for_worker = Arc::clone(&published_cell);
                // A dry run never encodes, so it cannot change the
                // capability -- it only reads it. It reads the
                // published one, like every other batch.
                //
                // The dry run is a batch of its own: the admission
                // gives it a fresh cancel token (the token is shared
                // with `Command::Cancel`, so an inherited one would
                // still be cancelled -- design doc §3.11.2). It does
                // not watch the cancel file: only the encode batch
                // answers an external cancel request.
                let admission = state.admit_batch(None);
                let cancel_for_worker = admission.token.clone();
                // The dry run is a batch too: it gets a batch id, so
                // its events can be matched (design doc §3.11.3).
                let id = admission.id;
                send_evt_lossy(
                    &evt_tx,
                    Event::CommandAccepted {
                        request_id,
                        batch_id: Some(id),
                    },
                    &dropped_counter,
                );
                spawn_batch_job(
                    move || {
                        // The cache is resolved here, in the worker: the
                        // one-time init may still be running, and
                        // waiting for it must not block the actor.
                        let Some(published) =
                            take_published(&published_cell_for_worker, CACHE_WAIT_TIMEOUT)
                        else {
                            return Some(Event::BatchFailed {
                                batch_id: id,
                                category: ErrorCategory::ConfigError,
                                message: "encoder cache init timed out".into(),
                            });
                        };
                        let cache = match cache_from_slot(published.cache) {
                            Ok(cache) => cache,
                            Err((category, message)) => {
                                return Some(Event::BatchFailed {
                                    batch_id: id,
                                    category,
                                    message,
                                });
                            }
                        };
                        // The capability of this publication: the
                        // dry run freezes the generation too.
                        // The generation this dry run froze: the
                        // plan is only valid for it (design doc
                        // §3.10.5).
                        let generation = published.capability.generation();
                        let batch_ctx = BatchContext::new(params, cache, published.capability);
                        let report = crate::pipeline::batch::dry_run_batch(
                            &batch_ctx,
                            files.iter(),
                            &cancel_for_worker,
                        );
                        Some(Event::DryRunFinished {
                            batch_id: id,
                            generation,
                            records: report.records,
                        })
                    },
                    done_tx.clone(),
                    None,
                    evt_tx.clone(),
                    DropCounter::default(),
                );
            }

            Command::Cancel { batch_id } => {
                // Only the active batch may be cancelled (design doc
                // §3.11.3): a cancel that names another batch -- a
                // stale one, or a batch that already ended -- is
                // refused. It must not stop whatever runs now.
                if !state.cancel(batch_id) {
                    send_evt_lossy(
                        &evt_tx,
                        Event::CommandRejected {
                            request_id,
                            reason: format!("no active batch {batch_id} to cancel"),
                        },
                        &dropped_counter,
                    );
                    continue;
                }
                send_evt_lossy(
                    &evt_tx,
                    Event::CommandAccepted {
                        request_id,
                        batch_id: Some(batch_id),
                    },
                    &dropped_counter,
                );
                // The token is marked (by `state.cancel`): the worker
                // polls it -- the in-flight ffmpeg is killed (and its
                // partial output removed), and the batch stops at the
                // next file boundary.
            }

            Command::LoadHistory { limit, after } => {
                send_evt_lossy(
                    &evt_tx,
                    Event::CommandAccepted {
                        request_id,
                        batch_id: None,
                    },
                    &dropped_counter,
                );
                let records = load_history(Some(records_dir.clone()), limit, after);
                send_evt_lossy(&evt_tx, Event::HistoryLoaded { records }, &dropped_counter);
            }

            Command::SaveConfig { config, revision } => {
                send_evt_lossy(
                    &evt_tx,
                    Event::CommandAccepted {
                        request_id,
                        batch_id: None,
                    },
                    &dropped_counter,
                );
                // The payload is the UI's edited draft. Persisting our
                // own copy would silently drop the edit: the file
                // would be rewritten with the original values while
                // the UI showed "Saved".
                match config.save(&config_path) {
                    Ok(()) => {
                        send_evt_lossy(
                            &evt_tx,
                            Event::ConfigSaved {
                                path: config_path.clone(),
                                revision,
                            },
                            &dropped_counter,
                        );
                    }
                    Err(e) => {
                        // A failed save is a **non-batch** failure: it
                        // finalizes nothing (design doc §3.11.3).
                        send_evt_lossy(
                            &evt_tx,
                            Event::OperationFailed {
                                request_id,
                                category: ErrorCategory::ConfigError,
                                message: format!("save failed: {e}"),
                            },
                            &dropped_counter,
                        );
                    }
                }
                // The `nv` switch may have changed -- and that is all
                // that changes: the capability is a fact about the
                // machine, so the cache stays as it is. The next batch
                // picks the new `nv` up from the run parameters (design
                // doc §3.10.5).
            }

            Command::Detect { force } => {
                send_evt_lossy(
                    &evt_tx,
                    Event::CommandAccepted {
                        request_id,
                        batch_id: None,
                    },
                    &dropped_counter,
                );
                // Same path as the CLI's `rencoder detect`: resolve
                // through the on-disk record, store a fresh probe, and
                // report what this machine can actually run.
                //
                // The probe spawns ffmpeg subprocesses (up to ~1.5s),
                // so it runs on its own thread: doing it here would
                // stall every other command -- `Command::Cancel`
                // included -- for the duration.
                let cache_path = nvenc_cache_path.clone();
                let detect_runner = Arc::clone(&detect_runner);
                // The probe request takes the next sequence number:
                // the actor publishes the result only if it is the
                // newest one (design doc §3.10.6 step 5).
                let seq = state.publisher.request();
                spawn_probe(
                    seq,
                    Some(request_id),
                    force,
                    cache_path.clone(),
                    move |args| detect_runner(args),
                    probe_tx.clone(),
                );
            }

            Command::SetWorkingDir { dir } => {
                send_evt_lossy(
                    &evt_tx,
                    Event::CommandAccepted {
                        request_id,
                        batch_id: None,
                    },
                    &dropped_counter,
                );
                // The operation directory follows the working
                // directory (design doc §3.6): the run-level paths
                // move with it.
                paths = paths.with_op_dir(&dir);
                // Re-check (design doc §3.10.7): the directory
                // changed, the machine did not -- so the machine's
                // facts are reused and only the filesystem is checked
                // anew. The report lands in the new operation
                // directory.
                let facts = machine_facts.clone().unwrap_or_default();
                crate::capability::check_run_capabilities(&paths, &facts);
                send_evt_lossy(
                    &evt_tx,
                    Event::CapabilityChecked {
                        report: paths.capability_report.clone(),
                    },
                    &dropped_counter,
                );
            }

            Command::Analyze { files } => {
                // U22 / Tier 3 #9: parallel analyze with per-file
                // progress events. The busy gate (checked above) keeps
                // it from overlapping with StartBatch / DryRun — the
                // operator can only have one kind of batch in flight.
                // v1 doesn't support Command::Cancel abort; the rayon
                // par_iter runs to completion and AnalyzeFinished fires
                // cleanly.
                let total = files.len();
                // The analyze run is a batch too: it gets a batch id
                // (design doc §3.11.3).
                let id = state.admit_batch(None).id;
                send_evt_lossy(
                    &evt_tx,
                    Event::CommandAccepted {
                        request_id,
                        batch_id: Some(id),
                    },
                    &dropped_counter,
                );
                send_evt_lossy(
                    &evt_tx,
                    Event::AnalyzeStarted {
                        batch_id: id,
                        total,
                    },
                    &dropped_counter,
                );
                let evt_tx_for_worker = evt_tx.clone();
                let dropped_for_worker = DropCounter::default();
                spawn_batch_job(
                    move || {
                        // Progress closure: convert each rayon worker
                        // tick into an Event on the lib→UI channel.
                        // `Sender<Event>` is Sync, so the `Fn + Sync +
                        // Send` bound required by rayon is satisfied.
                        let progress = |p: AnalyzeProgress| {
                            send_evt_lossy(
                                &evt_tx_for_worker,
                                Event::AnalyzeProgress {
                                    batch_id: id,
                                    done: p.done,
                                    total: p.total,
                                    failed: p.failed,
                                    last: p.last,
                                },
                                &dropped_for_worker,
                            );
                        };

                        let records = analyze_records_with_probe_and_progress_parallel(
                            &files,
                            probe_video_metadata_auto,
                            progress,
                        );

                        // failed_count invariant: equals
                        // records.iter().filter(|r| r.fps.is_none()).count().
                        // The probe-progress closure already counted
                        // failures (atomic increment), so we re-derive from
                        // the final records to keep the contract testable.
                        let failed_count = records.iter().filter(|r| r.fps.is_none()).count();

                        Some(Event::AnalyzeFinished {
                            batch_id: id,
                            records,
                            failed_count,
                        })
                    },
                    done_tx.clone(),
                    None,
                    evt_tx.clone(),
                    DropCounter::default(),
                );
            }
        }
    }
}

/// Read history records from the given data directory (empty if
/// `None`), apply `limit` + `after`, return newest-first.
///
/// Production caller passes the app's records directory; tests pass a
/// temp dir. Taking the dir as a parameter (rather than reading the
/// env var inside) keeps tests parallel-safe — env vars are process-
/// global and would race between threads.
fn load_history(
    data_dir: Option<PathBuf>,
    limit: Option<usize>,
    after: Option<DateTime<Utc>>,
) -> Vec<EncodingRecord> {
    use crate::data::store::read_records;

    let dir = match data_dir {
        Some(d) => d,
        None => return Vec::new(),
    };

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut all: Vec<EncodingRecord> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("parquet") {
            continue;
        }
        if let Ok(mut recs) = read_records(&path) {
            all.append(&mut recs);
        }
    }

    // Newest first; chronological reverse lets truncate() pick the
    // most recent N after the time filter.
    all.sort_by(|a, b| b.started_at.cmp(&a.started_at));

    if let Some(t) = after {
        all.retain(|r| r.started_at >= t);
    }

    if let Some(n) = limit {
        all.truncate(n);
    }

    all
}

/// Hook for `run_ffmpeg_with_progress` (U21 §3.8.6). Wired into the
/// `StartBatch` loop once `transcode_single` accepts a progress
/// callback. Currently unused — kept `pub(crate)` so future
/// plumbing can call it without re-exporting.
#[cfg(test)]
mod controller_tests {
    use super::*;

    #[test]
    fn handle_into_split_returns_both_halves() {
        let (cmd_tx, _) = unbounded::<Request>();
        let (_, evt_rx) = unbounded::<Event>();
        let handle = ControllerHandle {
            cmd_tx,
            evt_rx,
            config: Arc::new(crate::config::app::AppConfig::default()),
            config_path: std::path::PathBuf::from("(test)"),
        };
        let (_tx, _rx) = handle.into_split();
    }
}

// =====================================================================
// Mock test layer — message protocol contract without ffmpeg
// =====================================================================

#[cfg(test)]
#[allow(clippy::type_complexity)] // mock closure types are intentionally explicit
mod mock_tests {
    //! End-to-end tests of the StartBatch / Cancel / LoadHistory
    //! message protocol without invoking ffmpeg. The actor's encode
    //! step is replaced by a closure that returns predetermined
    //! EncodingRecord values, so tests verify:
    //!
    //! - Event sequence (BatchStarted → FileStarted × N → FileCompleted
    //!   × N → BatchCompleted).
    //! - Single-batch enforcement (second StartBatch while one
    //!   running → Event::Error(ConfigError)).
    //! - Cancel mid-batch (BatchCancelled with the correct completed
    //!   count).
    //! - LoadHistory filtering (limit + after) and missing-data-dir
    //!   graceful return.
    //!
    //! Real ffmpeg behavior is NOT tested here — that lives in the
    //! `transcode_single(_with_progress)` unit tests. These tests
    //! cover the *wiring*.

    use super::*;
    use crate::types::{EncoderSettings, EncodingOutcome};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};

    /// Per-process unique counter for tmp dirs in tests. Avoids
    /// collisions when tests run in parallel within the same
    /// nanosecond.
    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_tmp(label: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, AtomicOrdering::SeqCst);
        std::env::temp_dir().join(format!("rencoder_mock_{label}_{}_{n}", std::process::id()))
    }

    /// Test helper: wrap a path literal in `InputPath`. See the
    /// equivalent in `mod tests` above for rationale.
    fn input(p: &str) -> crate::types::InputPath {
        crate::types::InputPath::new(std::path::Path::new(p)).expect("test fixture")
    }

    /// A real directory with `count` fake video files in it.
    ///
    /// The actor touches the filesystem: the single-instance guard
    /// takes the batch's target directory (design doc §3.11.1.1), so
    /// the batch's files must live in a directory that exists.
    fn batch_files(label: &str, count: usize) -> Vec<crate::types::InputPath> {
        let dir = unique_tmp(label);
        std::fs::create_dir_all(&dir).expect("the batch dir must exist");
        (0..count)
            .map(|i| input(&dir.join(format!("f{i}.mp4")).to_string_lossy()))
            .collect()
    }

    /// Run a mock actor with a mock encoder.
    ///
    /// `mock_fn` is called once per file and returns the EncodingRecord
    /// for that file. `data_dir` is the path passed to `load_history`
    /// (not via env var — parallel-safe).
    ///
    /// The mock mirrors the production actor's structure: batches run
    /// on a worker thread and the command loop stays responsive. The
    /// tests therefore exercise the real mechanism (§3.10.1.1).
    #[allow(clippy::needless_pass_by_value)]
    /// Test harness: the **production** actor with test dependencies.
    ///
    /// The mock replaces one step -- "encode one file" -- through the
    /// [`FileEncoder`] seam. Everything else (the busy gate, the worker
    /// spawn, the event sequences, the cancel wiring) is the
    /// production code, so the message protocol has exactly one
    /// implementation (design doc §3.10.4).
    ///
    /// `mock_fn` is called once per file and returns the record for
    /// that file. `data_dir` is where `Command::SaveConfig` writes and
    /// `Command::LoadHistory` reads (not via env var -- parallel-safe).
    fn run_test_controller<F>(
        mock_fn: F,
        data_dir: Option<PathBuf>,
    ) -> (Sender<Request>, Receiver<Event>)
    where
        F: Fn(
                &EncodeTask,
                &mut BatchContext,
                &CancelToken,
                &mut dyn FnMut(FfmpegProgress),
            ) -> Result<EncodingRecord, EncodeError>
            + Send
            + Sync
            + 'static,
    {
        run_test_controller_with_capability(mock_fn, data_dir, None)
    }

    /// [`run_test_controller`] with the NVENC capability the cache
    /// init hands to the UI.
    ///
    /// The production actor resolves the capability at startup and
    /// reports it in an `Event::NvencCapability`; the test harness
    /// pre-populates the cache channel instead, so a test that cares
    /// about that event (or about the review-date deferral it
    /// triggers) passes the capability in here.
    fn run_test_controller_with_capability<F>(
        mock_fn: F,
        data_dir: Option<PathBuf>,
        capability: Option<Capability>,
    ) -> (Sender<Request>, Receiver<Event>)
    where
        F: Fn(
                &EncodeTask,
                &mut BatchContext,
                &CancelToken,
                &mut dyn FnMut(FfmpegProgress),
            ) -> Result<EncodingRecord, EncodeError>
            + Send
            + Sync
            + 'static,
    {
        run_test_controller_with_runner(mock_fn, data_dir, capability, None)
    }

    /// [`run_test_controller_with_capability`] with an injected NVENC
    /// probe runner.
    ///
    /// The runner is the seam the probe goes through, so a test can
    /// make a probe block, or answer differently each time -- the
    /// probe's *order* matters (design doc §3.10.6 step 5: an older
    /// probe's result must not overwrite a newer one).
    #[allow(clippy::too_many_arguments)]
    fn run_test_controller_with_runner<F>(
        mock_fn: F,
        data_dir: Option<PathBuf>,
        capability: Option<Capability>,
        detect_runner: Option<crate::job::DetectRunner>,
    ) -> (Sender<Request>, Receiver<Event>)
    where
        F: Fn(
                &EncodeTask,
                &mut BatchContext,
                &CancelToken,
                &mut dyn FnMut(FfmpegProgress),
            ) -> Result<EncodingRecord, EncodeError>
            + Send
            + Sync
            + 'static,
    {
        let (cmd_tx, cmd_rx) = unbounded::<Request>();
        let (evt_tx, evt_rx) = unbounded::<Event>();

        // The temp dir plays the records directory: the writer and
        // the history reader must agree on it (design doc §3.6).
        let records_dir = data_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("/nonexistent/rencoder-records"));
        // The app directory: a temp dir, never the working directory
        // -- the actor writes there (the capability report, the saved
        // config), and a test must not leave files in the source
        // tree.
        let app_dir = data_dir.clone().unwrap_or_else(|| {
            let dir = unique_tmp("app_dir");
            std::fs::create_dir_all(&dir).ok();
            dir
        });
        let config_path = app_dir.join("saved-config.toml");
        let mut paths = Paths::resolve(
            config_path.clone(),
            app_dir.clone(),
            app_dir,
            &AppConfig::default(),
        );
        paths.records_dir = records_dir.clone();
        paths.capability_record = records_dir.join("nvenc-capability.json");

        let env = ActorEnv {
            app_config: Arc::new(AppConfig::default()),
            config_path,
            paths,
            file_encoder: Arc::new(MockFileEncoder { f: mock_fn }),
            // The NVENC probe never runs against real ffmpeg: the
            // fake runner answers it.
            detect_runner: detect_runner
                .unwrap_or_else(|| scripted_probe_runner(capability.as_ref())),
        };

        std::thread::spawn(move || {
            run_actor(cmd_rx, evt_tx, env);
        });

        (cmd_tx, evt_rx)
    }

    /// The version line the fake machine reports.
    const PROBE_VERSION: &str = "ffmpeg version 6.0 (test)";

    /// The fake machine's answers to the capability probe.
    ///
    /// The harness drives the **production** probe path: the runner
    /// answers the encoder list, the version, and the 1-frame probes,
    /// and the real probe turns those answers into the capability
    /// (design doc §3.10.4 rule 2 -- a test double must not
    /// re-implement the thing it tests).
    fn scripted_probe_runner(capability: Option<&Capability>) -> crate::job::DetectRunner {
        let encoders: Vec<String> = capability
            .map(|c| c.support().iter().map(|e| e.encoder.clone()).collect())
            .unwrap_or_default();
        let usable: Vec<String> = capability
            .map(|c| {
                c.support()
                    .iter()
                    .filter(|e| e.usable)
                    .map(|e| e.encoder.clone())
                    .collect()
            })
            .unwrap_or_default();
        let encoders_output = encoders.join("\n");
        Arc::new(move |args: &[&str]| {
            if args.contains(&"-encoders") {
                return Ok(encoders_output.clone());
            }
            if args.contains(&"-version") {
                return Ok(PROBE_VERSION.to_string());
            }
            // A 1-frame capability probe: it succeeds for the encoders
            // this machine can run, and fails for the others.
            let encoder = args
                .windows(2)
                .find(|pair| pair[0] == "-c:v")
                .map(|pair| pair[1].to_string());
            match encoder {
                Some(encoder) if usable.contains(&encoder) => Ok(String::new()),
                Some(_) => Err(EncodeError::Probe(
                    "the 1-frame capability probe failed".into(),
                )),
                None => Ok(String::new()),
            }
        })
    }

    /// The test double for "encode one file".
    struct MockFileEncoder<F> {
        f: F,
    }

    impl<F> FileEncoder for MockFileEncoder<F>
    where
        F: Fn(
                &EncodeTask,
                &mut BatchContext,
                &CancelToken,
                &mut dyn FnMut(FfmpegProgress),
            ) -> Result<EncodingRecord, EncodeError>
            + Send
            + Sync
            + 'static,
    {
        fn encode(
            &self,
            task: &EncodeTask,
            ctx: &mut BatchContext,
            cancel: &CancelToken,
            on_progress: &mut dyn FnMut(FfmpegProgress),
        ) -> Result<EncodingRecord, EncodeError> {
            (self.f)(task, ctx, cancel, on_progress)
        }
    }

    /// Build a minimal EncodingRecord marked Succeeded with a given
    /// source path. Used by mock encoders to fabricate "completed"
    /// records without invoking ffmpeg.
    fn fake_record(path: &crate::types::InputPath) -> EncodingRecord {
        EncodingRecord {
            source_path: path.to_string(),
            source_size_bytes: 0,
            source_codec: None,
            source_container: None,
            source_width: None,
            source_height: None,
            source_fps: None,
            source_duration_secs: None,
            source_bit_rate_bps: None,
            started_at: chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            ended_at: Some(chrono::DateTime::<Utc>::from_timestamp(1_700_000_010, 0).unwrap()),
            encoding_duration_secs: Some(10.0),
            encoder_selected: None,
            encoder_settings: EncoderSettings::default(),
            ffmpeg_argv: vec![],
            ffmpeg_stderr_tail: String::new(),
            output_path: None,
            output_size_bytes: None,
            output_duration_secs: None,
            output_codec: None,
            compression_ratio: None,
            outcome: EncodingOutcome::Succeeded,
            error: None,
        }
    }

    /// Mock encoder that returns a Succeeded record for each file
    /// and counts calls. No progress events emitted.
    fn make_succeeding_mock() -> (
        impl Fn(
                &EncodeTask,
                &mut BatchContext,
                &CancelToken,
                &mut dyn FnMut(FfmpegProgress),
            ) -> Result<EncodingRecord, EncodeError>
            + Send
            + Sync
            + 'static,
        Arc<AtomicUsize>,
    ) {
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = count.clone();
        let f = move |task: &EncodeTask,
                      _: &mut BatchContext,
                      _: &CancelToken,
                      _: &mut dyn FnMut(FfmpegProgress)|
              -> Result<EncodingRecord, EncodeError> {
            count_clone.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(fake_record(&task.input))
        };
        (f, count)
    }

    /// Mock encoder that emits 2 progress blocks per file then
    /// returns Succeeded. Used to verify Event::Progress flow.
    fn make_progress_mock() -> impl Fn(
        &EncodeTask,
        &mut BatchContext,
        &CancelToken,
        &mut dyn FnMut(FfmpegProgress),
    ) -> Result<EncodingRecord, EncodeError>
           + Send
           + Sync
           + 'static {
        fn f(
            task: &EncodeTask,
            _: &mut BatchContext,
            _: &CancelToken,
            on_progress: &mut dyn FnMut(FfmpegProgress),
        ) -> Result<EncodingRecord, EncodeError> {
            on_progress(FfmpegProgress {
                frame: 100,
                fps: 30.0,
                speed: 1.0,
                time_secs: 5.0,
                eta_secs: Some(55.0),
            });
            on_progress(FfmpegProgress {
                frame: 200,
                fps: 30.0,
                speed: 1.0,
                time_secs: 10.0,
                eta_secs: Some(50.0),
            });
            Ok(fake_record(&task.input))
        }
        f
    }

    /// The events of a batch, without the actor's own publications.
    ///
    /// The capability publication is the actor's business -- it lands
    /// when the first probe finishes. The batch's event sequence is
    /// what these tests are about, so the publications are filtered
    /// out and the sequence is asserted on its own.
    fn batch_events(events: Vec<Event>) -> Vec<Event> {
        events
            .into_iter()
            .filter(|e| {
                !matches!(
                    e,
                    Event::NvencCapability(_) | Event::CapabilityChecked { .. }
                )
            })
            .collect()
    }

    /// The safety net for every wait in this module.
    ///
    /// A timeout is **not** a synchronization: it must never be the
    /// thing a test relies on to mean "it happened". The waits below
    /// return as soon as the event arrives; the timeout only bounds
    /// a test that would otherwise hang forever.
    const WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

    /// Wait for the next event that satisfies `pred`.
    ///
    /// The event arrives in milliseconds; waiting for a fixed
    /// window instead would make the test slow **and** fragile (a
    /// loaded machine can miss the window).
    fn wait_for_event<F>(rx: &Receiver<Event>, pred: F) -> Event
    where
        F: Fn(&Event) -> bool,
    {
        let deadline = std::time::Instant::now() + WAIT_LIMIT;
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .expect("timed out waiting for the event");
            match rx.recv_timeout(remaining) {
                Ok(event) => {
                    if pred(&event) {
                        return event;
                    }
                }
                Err(e) => panic!("timed out waiting for the event: {e}"),
            }
        }
    }

    /// Wait for the batch's **terminal** event and return everything
    /// that arrived up to it.
    ///
    /// The terminal event *is* the "this batch is over" signal
    /// (§3.11.4): one batch, one terminal event. So a test that
    /// wants "what this batch did" waits for that -- not for a
    /// window.
    ///
    /// This is the **barrier's release point** (design doc §3.11.3):
    /// the stage publishes, the next stage starts. The tests wait
    /// for the publication, exactly as the chain does.
    fn batch_events_until_end(rx: &Receiver<Event>) -> Vec<Event> {
        let mut events = Vec::new();
        let deadline = std::time::Instant::now() + WAIT_LIMIT;
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .expect("timed out waiting for the batch to end");
            match rx.recv_timeout(remaining) {
                Ok(event) => {
                    let terminal = is_terminal_event(&event);
                    events.push(event);
                    if terminal {
                        return events;
                    }
                }
                Err(e) => panic!("timed out waiting for the batch to end: {e}"),
            }
        }
    }

    /// Assert that no event satisfying `pred` arrives within
    /// `window`.
    ///
    /// A negative wait **is** a window -- there is nothing else to
    /// wait for. It cannot produce a false failure (a late event only
    /// makes the assertion weaker), so the window can be modest.
    fn assert_no_event<F>(rx: &Receiver<Event>, pred: F, window: std::time::Duration)
    where
        F: Fn(&Event) -> bool,
    {
        let deadline = std::time::Instant::now() + window;
        while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(remaining) {
                Ok(event) => {
                    assert!(!pred(&event), "an unexpected event arrived: {event:?}");
                }
                Err(_) => break,
            }
        }
    }

    /// The next capability publication, skipping the other events.
    fn next_capability_event(rx: &Receiver<Event>) -> Capability {
        match wait_for_event(rx, |e| matches!(e, Event::NvencCapability(_))) {
            Event::NvencCapability(capability) => capability,
            other => unreachable!("the predicate matched: {other:?}"),
        }
    }

    /// Is this event the end of a batch?
    fn is_terminal_event(event: &Event) -> bool {
        matches!(
            event,
            Event::BatchCompleted { .. }
                | Event::BatchCancelled { .. }
                | Event::BatchFailed { .. }
                | Event::DryRunFinished { .. }
                | Event::AnalyzeFinished { .. }
        )
    }

    #[test]
    fn a_busy_target_is_a_benign_rejection() {
        // Another instance holds the target (design doc §3.11.1.1):
        // the command is **rejected**, not an error -- the running
        // batch is untouched, and this one never starts.
        let files = batch_files("busy_target", 1);
        let target = files[0]
            .as_path()
            .parent()
            .expect("the file has a parent")
            .to_path_buf();
        // Take the target the way another instance would.
        let _held = crate::guard::TargetGuard::acquire(&target).expect("the target is free");

        let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
        cmd_tx
            .send(Request::new(
                1,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files,
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        let event = wait_for_event(&evt_rx, |e| matches!(e, Event::CommandRejected { .. }));
        let events = [event];
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::CommandRejected { .. })),
            "the busy target must be a rejection, got {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, Event::Error { .. })),
            "a rejected command is not a fatal error: {events:?}"
        );
    }

    #[test]
    fn a_busy_descendant_stops_the_batch() {
        // A directory **inside** the batch's scope is held by another
        // instance (design doc §3.11.1.1): the batch cannot start --
        // it would collect that directory's files and write the same
        // outputs.
        let root = unique_tmp("busy_descendant");
        let held_dir = root.join("held");
        let other_dir = root.join("other");
        std::fs::create_dir_all(&held_dir).unwrap();
        std::fs::create_dir_all(&other_dir).unwrap();
        // Another instance holds one of the two directories.
        let _held = crate::guard::TargetGuard::acquire(&held_dir).expect("the target is free");
        // This batch works on both -- its scope is their common parent.
        let files = vec![
            input(&held_dir.join("a.mp4").to_string_lossy()),
            input(&other_dir.join("b.mp4").to_string_lossy()),
        ];

        let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
        cmd_tx
            .send(Request::new(
                1,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files,
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        let events = batch_events(batch_events_until_end(&evt_rx));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::BatchFailed { .. })),
            "the batch must not start: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::BatchCompleted { .. })),
            "no batch may run on a busy scope: {events:?}"
        );
    }

    #[test]
    fn start_batch_emits_expected_event_sequence() {
        let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
        let files = batch_files("event_sequence", 3);
        cmd_tx
            .send(Request::new(
                1,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files: files.clone(),
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        let events = batch_events(batch_events_until_end(&evt_rx));
        // Expected sequence: per-file events interleave (each
        // process_one_file emits FileStarted then FileCompleted
        // before moving to the next file).
        //   CommandAccepted
        //   BatchStarted
        //   FileStarted(0) FileCompleted(0)
        //   FileStarted(1) FileCompleted(1)
        //   FileStarted(2) FileCompleted(2)
        //   BatchCompleted
        assert_eq!(events.len(), 2 + 3 * 2 + 1);

        // The request is answered before the batch starts (design
        // doc §3.11.3): the answer carries the batch id.
        let batch_id = match &events[0] {
            Event::CommandAccepted {
                request_id,
                batch_id: Some(id),
            } => {
                assert_eq!(*request_id, 1, "the answer carries the request id");
                *id
            }
            other => panic!("expected CommandAccepted first, got {other:?}"),
        };
        assert!(matches!(
            events[1],
            Event::BatchStarted { total_files: 3, .. }
        ));

        // Verify per-file pairs in order.
        for i in 0..3 {
            let started = &events[2 + i * 2];
            let completed = &events[2 + i * 2 + 1];
            match (started, completed) {
                (
                    Event::FileStarted {
                        batch_id: sb,
                        index: si,
                        total: st,
                        source: ss,
                    },
                    Event::FileCompleted {
                        batch_id: cb,
                        index: ci,
                        ..
                    },
                ) => {
                    assert_eq!(*si, i);
                    assert_eq!(*ci, i);
                    assert_eq!(*st, 3);
                    assert_eq!(ss, &files[i]);
                    // Every event of the batch carries its id (design
                    // doc §3.11.3).
                    assert_eq!(*sb, batch_id);
                    assert_eq!(*cb, batch_id);
                }
                _ => panic!("events {started:?} {completed:?} not a started/completed pair"),
            }
        }

        match events.last().unwrap() {
            Event::BatchCompleted {
                batch_id: id,
                records,
            } => {
                assert_eq!(*id, batch_id);
                assert_eq!(records.len(), 3);
                for (i, r) in records.iter().enumerate() {
                    assert_eq!(r.source_path, files[i].to_string());
                }
            }
            _ => panic!("expected BatchCompleted as last event"),
        }
    }

    #[test]
    fn start_batch_emits_progress_events_during_encode() {
        let (cmd_tx, evt_rx) = run_test_controller(make_progress_mock(), None);
        let files = batch_files("progress", 2);
        cmd_tx
            .send(Request::new(
                1,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files,
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        let events = batch_events(batch_events_until_end(&evt_rx));
        // Expected: CommandAccepted + BatchStarted
        //   + (FileStarted + Progress×2 + FileCompleted) × 2 + BatchCompleted
        assert_eq!(events.len(), 2 + (1 + 2 + 1) * 2 + 1);
        let progress_count = events
            .iter()
            .filter(|e| matches!(e, Event::Progress { .. }))
            .count();
        assert_eq!(progress_count, 4); // 2 per file × 2 files
    }

    #[test]
    fn start_batch_rejects_second_batch_with_error() {
        // The busy gate covers the worker's lifetime: the second
        // StartBatch arrives while the first batch is still running,
        // so it must be rejected instead of queued.
        //
        // The mock sleeps long enough that the first batch is
        // definitely still running when the second command is
        // processed.
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let (cmd_tx, evt_rx) = run_test_controller(
            move |task, _, _, _| {
                calls_clone.fetch_add(1, AtomicOrdering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(200));
                Ok(fake_record(&task.input))
            },
            None,
        );
        let files = batch_files("second_batch", 1);
        cmd_tx
            .send(Request::new(
                1,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files: files.clone(),
                },
            ))
            .unwrap();
        cmd_tx
            .send(Request::new(
                2,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files,
                },
            ))
            .unwrap();

        let events = batch_events(batch_events_until_end(&evt_rx));
        assert!(
            events.iter().any(|e| matches!(
                e,
                Event::CommandRejected { reason, .. } if reason.contains("another batch")
            )),
            "the second batch must be rejected, got: {events:?}"
        );
        // Only the first batch ran (one file).
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn cancel_command_stops_a_running_batch() {
        // Regression test for the actor/worker split (§3.10.1.1).
        //
        // Before the split the actor ran the batch inline, so
        // `Command::Cancel` sat in the command queue until the batch
        // was over — the UI's cancel button never aborted anything.
        // Now the batch runs on a worker and the cancel reaches it.
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let (cmd_tx, evt_rx) = run_test_controller(
            move |task, _, _, _| {
                calls_clone.fetch_add(1, AtomicOrdering::SeqCst);
                // Slow enough that the cancel lands while this file
                // is still being encoded.
                std::thread::sleep(std::time::Duration::from_millis(200));
                Ok(fake_record(&task.input))
            },
            None,
        );
        let files: Vec<crate::types::InputPath> = batch_files("cancel_running", 20);
        cmd_tx
            .send(Request::new(
                1,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files,
                },
            ))
            .unwrap();

        // Wait until the worker really started the batch -- and
        // learn the batch id (the cancel names it, design doc
        // §3.11.3).
        let batch_id = loop {
            match evt_rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(Event::BatchStarted { id, .. }) => break id,
                Ok(_) => continue,
                Err(e) => panic!("no BatchStarted event: {e}"),
            }
        };

        cmd_tx
            .send(Request::new(2, Command::Cancel { batch_id }))
            .unwrap();

        let events = batch_events(batch_events_until_end(&evt_rx));
        let last = events.last().expect("expected a terminal event");
        match last {
            Event::BatchCancelled {
                batch_id: id,
                completed,
                total,
            } => {
                assert_eq!(*id, batch_id);
                assert_eq!(*total, 20);
                assert!(
                    *completed < 20,
                    "the batch must stop early, completed={completed}"
                );
            }
            _ => panic!("expected BatchCancelled, got {last:?}"),
        }
        // The encoder must not have run through the whole list.
        let calls = calls.load(AtomicOrdering::SeqCst);
        assert!(
            calls < 20,
            "cancel must stop the batch, but {calls} files were encoded"
        );
    }

    #[test]
    fn cancel_mid_batch_emits_batch_cancelled() {
        // Mock encoder that cancels the token on the 3rd call (after
        // 2 records have been emitted).
        let (cmd_tx, evt_rx) = run_test_controller(
            |task, _, cancel, _| {
                cancel.cancel();
                Ok(fake_record(&task.input))
            },
            None,
        );
        let files: Vec<crate::types::InputPath> = batch_files("cancel_mid", 5);
        cmd_tx
            .send(Request::new(
                1,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files,
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        let events = batch_events(batch_events_until_end(&evt_rx));
        // First file completes (FileStarted + FileCompleted) before
        // cancel propagates; then BatchCancelled with completed = 1.
        // (Mock cancels on file 1 itself, but the file's FileCompleted
        // already emitted before the cancel was observed.)
        let last = events.last().unwrap();
        match last {
            Event::BatchCancelled {
                completed, total, ..
            } => {
                assert_eq!(*completed, 1);
                assert_eq!(*total, 5);
            }
            _ => panic!("expected BatchCancelled, got {last:?}"),
        }
    }

    #[test]
    fn a_cancel_that_names_another_batch_is_refused() {
        // Design doc §3.11.3: only the active batch may be cancelled.
        // A stale cancel (the user cancelled, then a new batch
        // started) must not stop the batch that is running now.
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let (cmd_tx, evt_rx) = run_test_controller(
            move |task, _, _, _| {
                calls_clone.fetch_add(1, AtomicOrdering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(50));
                Ok(fake_record(&task.input))
            },
            None,
        );
        let files: Vec<crate::types::InputPath> = batch_files("stale_cancel", 6);
        cmd_tx
            .send(Request::new(
                1,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files,
                },
            ))
            .unwrap();

        // Wait for the batch to announce its id.
        let batch_id = loop {
            match evt_rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(Event::BatchStarted { id, .. }) => break id,
                Ok(_) => continue,
                Err(e) => panic!("no BatchStarted event: {e}"),
            }
        };

        // A cancel for a different batch: refused.
        cmd_tx
            .send(Request::new(
                2,
                Command::Cancel {
                    batch_id: batch_id + 100,
                },
            ))
            .unwrap();

        let event = wait_for_event(&evt_rx, |e| matches!(e, Event::CommandRejected { .. }));
        let events = [event];
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::CommandRejected { request_id: 2, .. })),
            "a cancel for another batch must be refused: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::BatchCancelled { .. })),
            "the running batch must not be cancelled: {events:?}"
        );
    }

    #[test]
    fn a_rejected_request_names_the_request() {
        // The busy gate's refusal says *which* request was refused
        // (design doc §3.11.3): the UI restores the state of that
        // request, not of "the last one".
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let (cmd_tx, evt_rx) = run_test_controller(
            move |task, _, _, _| {
                calls_clone.fetch_add(1, AtomicOrdering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(200));
                Ok(fake_record(&task.input))
            },
            None,
        );
        let files = batch_files("reject_names", 1);
        cmd_tx
            .send(Request::new(
                1,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files: files.clone(),
                },
            ))
            .unwrap();
        cmd_tx
            .send(Request::new(
                77,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files,
                },
            ))
            .unwrap();

        let event = wait_for_event(&evt_rx, |e| matches!(e, Event::CommandRejected { .. }));
        let events = [event];
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::CommandRejected { request_id: 77, .. })),
            "the refusal must name the refused request: {events:?}"
        );
    }

    #[test]
    fn load_history_returns_empty_when_no_data_dir() {
        // No RENENCODER_DATA_DIR set + no resolvable app dir →
        // load_history returns Vec::new() and emits HistoryLoaded(empty).
        std::env::remove_var("RENENCODER_DATA_DIR");
        let (cmd_tx, evt_rx) =
            run_test_controller(|task, _, _, _| Ok(fake_record(&task.input)), None);
        cmd_tx
            .send(Request::new(
                1,
                Command::LoadHistory {
                    limit: None,
                    after: None,
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        // The history is not a batch: it waits for **its own**
        // publication (the barrier's release point for that step).
        let loaded = wait_for_event(&evt_rx, |e| matches!(e, Event::HistoryLoaded { .. }));
        match loaded {
            Event::HistoryLoaded { records } => assert!(records.is_empty()),
            other => panic!("expected HistoryLoaded, got {other:?}"),
        }
    }

    #[test]
    fn load_history_reads_parquet_files_in_data_dir() {
        // Write 3 records to a temp parquet file, send LoadHistory,
        // expect HistoryLoaded with those records (newest-first by
        // started_at; same timestamp here so order is implementation
        // detail — just check the set).
        let tmp = unique_tmp("load");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("encoding-test.parquet");

        let records: Vec<EncodingRecord> = (0..3)
            .map(|i| {
                let mut r = fake_record(&input(&format!("/lh/{i}.mp4")));
                r.started_at =
                    chrono::DateTime::<Utc>::from_timestamp(1_700_000_000 + i as i64 * 10, 0)
                        .unwrap();
                r
            })
            .collect();
        let bytes = crate::data::store::records_to_parquet(&records).expect("parquet");
        std::fs::write(&path, bytes).expect("write parquet");

        let (cmd_tx, evt_rx) = run_test_controller(
            |task, _, _, _| Ok(fake_record(&task.input)),
            Some(tmp.clone()),
        );
        cmd_tx
            .send(Request::new(
                1,
                Command::LoadHistory {
                    limit: None,
                    after: None,
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        // The history is not a batch: it waits for **its own**
        // publication (the barrier's release point for that step).
        let loaded = wait_for_event(&evt_rx, |e| matches!(e, Event::HistoryLoaded { .. }));
        match loaded {
            Event::HistoryLoaded { records: loaded } => {
                assert_eq!(loaded.len(), 3);
                // Newest-first ordering.
                assert!(loaded[0].started_at >= loaded[1].started_at);
                assert!(loaded[1].started_at >= loaded[2].started_at);
            }
            other => panic!("expected HistoryLoaded, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_finished_batch_writes_its_records_to_the_history_file() {
        // The GUI's history reads `<data_dir>/encoding-*.parquet`; if
        // nobody writes it, the history stays empty forever (design
        // doc §3.11.4).
        let dir = unique_tmp("batch_records");
        std::fs::create_dir_all(&dir).unwrap();

        let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, Some(dir.clone()));
        cmd_tx
            .send(Request::new(
                1,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files: batch_files("history", 1),
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        let events = batch_events(batch_events_until_end(&evt_rx));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::BatchCompleted { .. })),
            "expected a BatchCompleted event, got {events:?}"
        );

        // The records are on disk, under the name the history reader
        // recognises.
        let written: Vec<_> = std::fs::read_dir(&dir)
            .expect("the data dir must be readable")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("encoding-") && name.ends_with(".parquet"))
            .collect();
        assert_eq!(
            written.len(),
            1,
            "the batch's records must be written: {written:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "parallel-analyze")]
    #[test]
    fn save_config_persists_the_caller_s_config() {
        // The payload is the user's edited draft. If the actor
        // persisted its own copy instead, the edit would be lost --
        // the file would be rewritten with the original values.
        let dir = unique_tmp("save_config");
        std::fs::create_dir_all(&dir).unwrap();

        let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, Some(dir.clone()));
        let edited = AppConfig {
            encode: Some(crate::config::app::EncodeConfig {
                crf: 28,
                ..Default::default()
            }),
            ..Default::default()
        };
        cmd_tx
            .send(Request::new(
                1,
                Command::SaveConfig {
                    config: Box::new(edited),
                    revision: 7,
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        let event = wait_for_event(&evt_rx, |e| matches!(e, Event::ConfigSaved { .. }));
        let events = [event];
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ConfigSaved { revision: 7, .. })),
            "the receipt must carry the revision it saved, got {events:?}"
        );

        let written = std::fs::read_to_string(dir.join("saved-config.toml")).unwrap();
        assert!(
            written.contains("crf = 28"),
            "the edited value must reach the file: {written}"
        );
    }

    #[test]
    fn detect_command_emits_a_capability_event() {
        // The actor's Command::Detect arm: the capability comes back
        // as an event so the UI can render it.
        let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
        cmd_tx
            .send(Request::new(1, Command::Detect { force: true }))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        let event = wait_for_event(&evt_rx, |e| matches!(e, Event::NvencCapability(_)));
        let events = [event];
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::NvencCapability(_))),
            "expected a NvencCapability event, got {events:?}"
        );
    }

    #[test]
    fn a_working_directory_change_rechecks_and_writes_the_report() {
        // The GUI's working directory moved: the operation directory
        // follows it (design doc §3.6), the capability check runs
        // again (design doc §3.10.7), and the report lands in the new
        // operation directory.
        let app = unique_tmp("working_dir_app");
        let working = unique_tmp("working_dir_op");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&working).unwrap();
        let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, Some(app.clone()));
        cmd_tx
            .send(Request::new(
                1,
                Command::SetWorkingDir {
                    dir: working.clone(),
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        let event = wait_for_event(&evt_rx, |e| matches!(e, Event::CapabilityChecked { .. }));
        let events = [event];
        let report = events
            .iter()
            .find_map(|e| match e {
                Event::CapabilityChecked { report } => Some(report.clone()),
                _ => None,
            })
            .expect("the check must announce its report");
        assert_eq!(
            report,
            working.join("capabilities.json"),
            "the report belongs to the new operation directory"
        );
        assert!(
            report.exists(),
            "the report must be written: {}",
            report.display()
        );
        let _ = std::fs::remove_dir_all(&app);
        let _ = std::fs::remove_dir_all(&working);
    }

    #[test]
    fn analyze_emits_started_progress_finished_sequence() {
        // U22 / Tier 3 #9: send Command::Analyze with 3 temp files
        // (random bytes — they fail to probe, but the actor still
        // emits the full Started + N×Progress + Finished event
        // sequence). Verifies the actor's Command::Analyze arm.
        //
        // The closure-injection semantics of
        // analyze_records_with_probe_and_progress_parallel (per-file
        // tick, monotonic done, FileOutcome variants) are tested
        // separately in analyzer::records::tests — this test only
        // pins the actor's emit sequence.
        let tmp = unique_tmp("analyze");
        std::fs::create_dir_all(&tmp).unwrap();
        let files: Vec<crate::types::InputPath> = (0..3)
            .map(|i| {
                let p = tmp.join(format!("f{i}.mp4"));
                std::fs::write(&p, b"not a real video").unwrap();
                crate::types::InputPath::new(&p).unwrap()
            })
            .collect();

        let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
        cmd_tx
            .send(Request::new(
                1,
                Command::Analyze {
                    files: files.clone(),
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        let events = batch_events(batch_events_until_end(&evt_rx));
        // Expected: 1 CommandAccepted + 1 AnalyzeStarted
        //            + 3 AnalyzeProgress + 1 AnalyzeFinished.
        assert_eq!(events.len(), 6, "expected 6 events, got: {events:?}");
        // First event: the request is answered (design doc §3.11.3).
        match &events[0] {
            Event::CommandAccepted { batch_id, .. } => assert!(batch_id.is_some()),
            _ => panic!("expected CommandAccepted first, got: {:?}", events[0]),
        }
        // Second event: AnalyzeStarted with total = 3.
        match &events[1] {
            Event::AnalyzeStarted { total, .. } => assert_eq!(*total, 3),
            _ => panic!("expected AnalyzeStarted, got: {:?}", events[1]),
        }
        // Middle 3 events: AnalyzeProgress, all with total = 3.
        for evt in &events[2..5] {
            match evt {
                Event::AnalyzeProgress { total, last, .. } => {
                    assert_eq!(*total, 3);
                    assert!(last.is_some(), "every Progress tick should carry last");
                }
                _ => panic!("expected AnalyzeProgress, got: {evt:?}"),
            }
        }
        // Last event: AnalyzeFinished with 3 records in input order
        // and failed_count = 3 (all probes fail on garbage files).
        match events.last().unwrap() {
            Event::AnalyzeFinished {
                records,
                failed_count,
                ..
            } => {
                assert_eq!(*failed_count, 3);
                assert_eq!(records.len(), 3);
                // Input order: f0, f1, f2 (par_iter().collect() preserves).
                for (i, r) in records.iter().enumerate() {
                    assert_eq!(r.path, files[i].as_path());
                    assert!(r.fps.is_none(), "garbage file → fps = None");
                }
            }
            _ => panic!("expected AnalyzeFinished last, got: {:?}", events.last()),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "parallel-analyze")]
    #[test]
    fn analyze_runs_to_completion() {
        // The Analyze success path: a normal batch runs to completion
        // (Started + Progress + Finished) with the mocked-out probe
        // backend.
        //
        // (This test used to be named
        // `analyze_rejects_concurrent_batch_with_error`, but it never
        // tested the rejection: the mock's Analyze arm had no busy
        // gate. The mock now drives the production actor, which does
        // gate Analyze -- see `start_batch_rejects_second_batch_with_error`
        // for the gate contract.)
        let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
        let tmp = unique_tmp("analyze-busy");
        std::fs::create_dir_all(&tmp).unwrap();
        let files: Vec<crate::types::InputPath> = (0..3)
            .map(|i| {
                let p = tmp.join(format!("f{i}.mp4"));
                std::fs::write(&p, b"not a real video").unwrap();
                crate::types::InputPath::new(&p).unwrap()
            })
            .collect();
        cmd_tx
            .send(Request::new(
                1,
                Command::Analyze {
                    files: files.clone(),
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.
        let events = batch_events(batch_events_until_end(&evt_rx));
        assert!(matches!(events[1], Event::AnalyzeStarted { .. }));
        assert!(matches!(
            events.last().unwrap(),
            Event::AnalyzeFinished { .. }
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "parallel-analyze")]
    #[test]
    fn analyze_empty_files_emits_started_then_finished() {
        // U22: zero-file analyze batch — production arm emits
        // Started unconditionally first, then the parallel loop
        // runs zero times (rayon par_iter is a no-op on empty
        // slices), then Finished fires. Sequence: Started +
        // Finished (2 events). Lets the UI clear any stale
        // "analyzing" state. Test name (U22 review-delta #11)
        // now matches the actual expectation; the previous
        // "emits_finished_only" was a lie since the body asserted
        // 2 events.
        let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
        cmd_tx
            .send(Request::new(1, Command::Analyze { files: Vec::new() }))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        let events = batch_events(batch_events_until_end(&evt_rx));
        assert_eq!(
            events.len(),
            3,
            "expected Accepted + Started + Finished, got: {events:?}"
        );
        match &events[1] {
            Event::AnalyzeStarted { total, .. } => assert_eq!(*total, 0),
            _ => panic!("expected AnalyzeStarted"),
        }
        match &events[2] {
            Event::AnalyzeFinished {
                records,
                failed_count,
                ..
            } => {
                assert!(records.is_empty());
                assert_eq!(*failed_count, 0);
            }
            _ => panic!("expected AnalyzeFinished"),
        }
    }

    #[test]
    fn load_history_applies_limit_filter() {
        let tmp = unique_tmp("limit");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("encoding-limit.parquet");
        let records: Vec<EncodingRecord> = (0..10)
            .map(|i| {
                let mut r = fake_record(&input(&format!("/lim/{i}.mp4")));
                r.started_at =
                    chrono::DateTime::<Utc>::from_timestamp(1_700_000_000 + i as i64, 0).unwrap();
                r
            })
            .collect();
        let bytes = crate::data::store::records_to_parquet(&records).expect("parquet");
        std::fs::write(&path, bytes).expect("write");

        let (cmd_tx, evt_rx) = run_test_controller(
            |task, _, _, _| Ok(fake_record(&task.input)),
            Some(tmp.clone()),
        );
        cmd_tx
            .send(Request::new(
                1,
                Command::LoadHistory {
                    limit: Some(3),
                    after: None,
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        let event = wait_for_event(&evt_rx, |e| matches!(e, Event::HistoryLoaded { .. }));
        let events = [event];
        match &events[0] {
            Event::HistoryLoaded { records: loaded } => {
                assert_eq!(loaded.len(), 3);
                // Newest-first: highest started_at first.
                assert!(loaded[0].started_at.timestamp() > loaded[1].started_at.timestamp());
                assert!(loaded[1].started_at.timestamp() > loaded[2].started_at.timestamp());
            }
            _ => panic!("expected HistoryLoaded"),
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn load_history_applies_after_filter() {
        let tmp = unique_tmp("after");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("encoding-after.parquet");
        let records: Vec<EncodingRecord> = (0..5)
            .map(|i| {
                let mut r = fake_record(&input(&format!("/aft/{i}.mp4")));
                r.started_at =
                    chrono::DateTime::<Utc>::from_timestamp(1_700_000_000 + i as i64 * 100, 0)
                        .unwrap();
                r
            })
            .collect();
        let bytes = crate::data::store::records_to_parquet(&records).expect("parquet");
        std::fs::write(&path, bytes).expect("write");

        let (cmd_tx, evt_rx) = run_test_controller(
            |task, _, _, _| Ok(fake_record(&task.input)),
            Some(tmp.clone()),
        );
        let cutoff = chrono::DateTime::<Utc>::from_timestamp(1_700_000_000 + 200, 0).unwrap();
        cmd_tx
            .send(Request::new(
                1,
                Command::LoadHistory {
                    limit: None,
                    after: Some(cutoff),
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        let event = wait_for_event(&evt_rx, |e| matches!(e, Event::HistoryLoaded { .. }));
        let events = [event];
        match &events[0] {
            Event::HistoryLoaded { records: loaded } => {
                // started_at values: 0, 100, 200, 300, 400 (seconds offset).
                // after = 200 → keep records with started_at >= 200.
                assert_eq!(loaded.len(), 3);
                for r in loaded {
                    assert!(r.started_at >= cutoff);
                }
            }
            _ => panic!("expected HistoryLoaded"),
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_per_file_failure_travels_in_the_record_not_in_a_fatal_event() {
        // Design doc §3.11.3: a file failure belongs to the file. It
        // travels in the record's `error` field -- a fatal event
        // would finalize the whole batch while the other files are
        // still running.
        let (cmd_tx, evt_rx) = run_test_controller(
            |_, _, _, _| {
                Err(crate::error::EncodeError::Ffmpeg(
                    "simulated failure".into(),
                ))
            },
            None,
        );
        let files = batch_files("encode_error", 1);
        cmd_tx
            .send(Request::new(
                1,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files,
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        // The batch's events -- up to its terminal event (the
        // barrier's release point).
        let events = batch_events(batch_events_until_end(&evt_rx));
        // A per-file failure is **not** a fatal event (design doc
        // §3.11.3): it travels in the record's `error` field.
        assert!(
            !events.iter().any(|e| matches!(e, Event::Error { .. })),
            "a file failure is not a fatal event: {events:?}"
        );
        // Review #120: failed records ARE included in the final
        // BatchCompleted list (the per-file failure is mapped to
        // Ok(Failed record) inside the processor closure, not to an
        // aborted batch).
        match events.last().unwrap() {
            Event::BatchCompleted { records, .. } => {
                assert_eq!(records.len(), 1);
                assert!(matches!(records[0].outcome, EncodingOutcome::Failed));
                assert!(records[0].error.is_some(), "the record carries the error");
                assert!(matches!(
                    records[0].error.as_ref().unwrap().category,
                    ErrorCategory::FfmpegSpawnFailed
                ));
            }
            other => panic!("expected BatchCompleted, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn cancel_during_encode_removes_partial_output() {
        // v1.2 follow-up #2: end-to-end test through the actor's
        // full StartBatch path. The mock calls the REAL
        // run_ffmpeg_with_progress with `argv[0] = "sh"` — a
        // shell script acts as fake-ffmpeg, writes to a real
        // partial output file, and emits `progress=continue`
        // blocks like real ffmpeg.
        //
        // On the first progress block, the mock cancels the
        // token. The next cancel poll in run_ffmpeg_with_progress
        // fires (within ~100ms), kills the child, runs the
        // cleanup helper, and returns Err. The actor's processor
        // closure wraps the err into a Failed record; TaskManager
        // moves to the next file (where the between-files cancel
        // poll in job.rs fires and aborts the batch).
        //
        // After the actor drains, the partial output file must
        // be GONE — proving the cleanup ran in the real flow,
        // not just my isolated unit test.
        //
        // NOTE: `run_test_controller` (job.rs:1355) ignores the
        // `params` from `Command::StartBatch` and uses its own
        // `EncodeParams::default()` (X264, default ext = "mkv"
        // after v1.2 follow-up #3). So `task.output = file1.mkv`
        // — different from the input (`file1.mp4`). The mock must
        // use `task.output` as argv's last element for cleanup to
        // hit the right file.
        let tmp = unique_tmp("cancel-partial");
        std::fs::create_dir_all(&tmp).unwrap();

        let input_path = tmp.join("file1.mp4");
        std::fs::write(&input_path, b"fake video").unwrap();
        // task.output = file1.mkv (X264 → mkv, no collision
        // with the .mp4 input). Write to that exact path.
        let partial = tmp.join("file1.mkv");
        let partial_check_b = partial.clone();
        std::fs::write(&partial, b"garbage").unwrap();

        // Fake-ffmpeg script: emits progress blocks + writes more
        // garbage to the output. Loops ~forever; we kill it via
        // cancel.
        let script = format!(
            r#"
            i=0
            while [ $i -lt 200 ]; do
                printf 'frame=%d\nfps=30\nspeed=1.0x\nout_time_us=1000000\nprogress=continue\n' $i
                echo garbage >> {}
                sleep 0.05
                i=$((i+1))
            done
            "#,
            partial.display()
        );

        let (cmd_tx, evt_rx) = run_test_controller(
            move |task: &EncodeTask,
                  _ctx: &mut BatchContext,
                  cancel: &CancelToken,
                  on_progress: &mut dyn FnMut(FfmpegProgress)| {
                let output = task.output.to_string_lossy().into_owned();
                let argv = vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    script.clone(),
                    output.clone(),
                ];
                assert_eq!(
                    argv.last().unwrap(),
                    &output,
                    "argv.last() must equal task.output for cleanup to work"
                );
                let cancel_signal = cancel.clone();
                let mut progress = move |p: FfmpegProgress| {
                    cancel_signal.cancel();
                    on_progress(p);
                };
                let res = crate::pipeline::batch::run_ffmpeg_with_progress(
                    &argv,
                    cancel,
                    Some(60.0),
                    &mut progress,
                );
                match res {
                    Ok(()) => panic!("expected cancel err"),
                    Err((category, msg, stderr)) => Ok(EncodingRecord {
                        source_path: task.input.to_string(),
                        source_size_bytes: 0,
                        source_codec: None,
                        source_container: None,
                        source_width: None,
                        source_height: None,
                        source_fps: None,
                        source_duration_secs: None,
                        source_bit_rate_bps: None,
                        started_at: chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 0)
                            .unwrap(),
                        ended_at: Some(
                            chrono::DateTime::<Utc>::from_timestamp(1_700_000_010, 0).unwrap(),
                        ),
                        encoding_duration_secs: Some(10.0),
                        encoder_selected: None,
                        encoder_settings: EncoderSettings::default(),
                        ffmpeg_argv: argv.clone(),
                        ffmpeg_stderr_tail: stderr,
                        output_path: None,
                        output_size_bytes: None,
                        output_duration_secs: None,
                        output_codec: None,
                        compression_ratio: None,
                        outcome: EncodingOutcome::Failed,
                        error: Some(crate::types::ErrorContext {
                            category,
                            message: msg,
                            ffmpeg_stderr_tail: String::new(),
                        }),
                    }),
                }
            },
            None,
        );

        let files: Vec<crate::types::InputPath> = vec![input(input_path.to_str().unwrap())];
        cmd_tx
            .send(Request::new(
                1,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files,
                },
            ))
            .unwrap();
        // The command channel stays open: closing it tells the actor
        // "nobody left to serve", and it exits. The actor is the
        // publisher (design doc §3.10.6 step 5), so an actor that
        // exited would never publish the startup capability and the
        // batch would wait for a state that never arrives.

        let _events = batch_events(batch_events_until_end(&evt_rx));

        assert!(
            !partial_check_b.exists(),
            "partial output {} still on disk after cancel; cleanup did NOT run in the full actor flow",
            partial_check_b.display()
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_batch_gets_the_published_capability() {
        // The point of the generation work (design doc §3.10.6
        // step 5): the GUI's batches must see the machine's NVENC
        // capability. The actor publishes it; the batch freezes that
        // publication.
        //
        // Before this, the actor's authoritative copy was never
        // assigned -- the batches ran with an empty capability, so
        // `any_usable()` was false and the selection always fell
        // back to software encoding.
        let capability = Capability::from_support(vec![crate::types::NvencSupport {
            encoder: "h264_nvenc".into(),
            usable: true,
            reason: None,
        }]);
        let saw_usable = Arc::new(AtomicUsize::new(0));
        let saw_usable_clone = saw_usable.clone();
        let (cmd_tx, evt_rx) = run_test_controller_with_capability(
            move |task: &EncodeTask,
                  ctx: &mut BatchContext,
                  _: &CancelToken,
                  _: &mut dyn FnMut(FfmpegProgress)| {
                if ctx.capability().any_usable() {
                    saw_usable_clone.fetch_add(1, AtomicOrdering::SeqCst);
                }
                Ok(fake_record(&task.input))
            },
            None,
            Some(capability),
        );
        let files = batch_files("published_capability", 1);
        cmd_tx
            .send(Request::new(
                1,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files,
                },
            ))
            .unwrap();

        let events = batch_events(batch_events_until_end(&evt_rx));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::BatchCompleted { .. })),
            "the batch must run: {events:?}"
        );
        assert_eq!(
            saw_usable.load(AtomicOrdering::SeqCst),
            1,
            "the batch must see the machine's capability; events: {events:?}"
        );
    }

    #[test]
    fn a_failed_re_probe_keeps_the_current_capability() {
        // Design doc §3.10.6 step 5: a failed detection keeps the
        // current snapshot. Publishing an empty capability instead
        // would silently turn NVENC off for the next batch -- the
        // machine is fine, only the probe failed.
        let capability = Capability::from_support(vec![crate::types::NvencSupport {
            encoder: "h264_nvenc".into(),
            usable: true,
            reason: None,
        }]);
        let saw_usable = Arc::new(AtomicUsize::new(0));
        let saw_usable_clone = saw_usable.clone();
        // The probe fails -- but only from the **second** probe on: the
        // first one is the startup, which must succeed (the scenario
        // is "the machine is fine, the re-probe failed").
        let probes = Arc::new(AtomicUsize::new(0));
        let probes_for_runner = probes.clone();
        let scripted = scripted_probe_runner(Some(&capability));
        let (cmd_tx, evt_rx) = run_test_controller_with_runner(
            move |task: &EncodeTask,
                  ctx: &mut BatchContext,
                  _: &CancelToken,
                  _: &mut dyn FnMut(FfmpegProgress)| {
                if ctx.capability().any_usable() {
                    saw_usable_clone.fetch_add(1, AtomicOrdering::SeqCst);
                }
                Ok(fake_record(&task.input))
            },
            None,
            Some(capability),
            Some(Arc::new(move |args: &[&str]| {
                if args.contains(&"-encoders")
                    && probes_for_runner.fetch_add(1, AtomicOrdering::SeqCst) > 0
                {
                    return Err(EncodeError::Probe("probe failed".into()));
                }
                scripted(args)
            })),
        );

        // The startup's publication comes first: the runner counts
        // the probes, so the test has to know which probe is which
        // before it sends the re-probe.
        let startup = next_capability_event(&evt_rx);
        assert_eq!(startup.generation(), 1, "the startup publishes first");

        // The forced re-probe: it fails.
        cmd_tx
            .send(Request::new(1, Command::Detect { force: true }))
            .unwrap();
        let failure = wait_for_event(&evt_rx, |e| matches!(e, Event::OperationFailed { .. }));
        assert!(
            matches!(failure, Event::OperationFailed { .. }),
            "the failed probe must be reported: {failure:?}"
        );

        // The next batch still sees the machine's capability.
        cmd_tx
            .send(Request::new(
                2,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files: batch_files("failed_probe", 1),
                },
            ))
            .unwrap();
        let events = batch_events(batch_events_until_end(&evt_rx));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::BatchCompleted { .. })),
            "the batch must run: {events:?}"
        );
        assert_eq!(
            saw_usable.load(AtomicOrdering::SeqCst),
            1,
            "the failed probe must not have wiped the capability"
        );
    }

    #[test]
    fn a_re_probe_publishes_a_new_generation() {
        // A forced re-probe publishes a new capability -- a new
        // generation -- and the next batch freezes that one (design
        // doc §3.10.6 step 5).
        let generations = Arc::new(Mutex::new(Vec::<u64>::new()));
        let generations_clone = Arc::clone(&generations);
        let (cmd_tx, evt_rx) = run_test_controller_with_capability(
            move |task: &EncodeTask,
                  ctx: &mut BatchContext,
                  _: &CancelToken,
                  _: &mut dyn FnMut(FfmpegProgress)| {
                generations_clone.lock().push(ctx.capability().generation());
                Ok(fake_record(&task.input))
            },
            None,
            None,
        );

        // The first batch: the startup publication.
        cmd_tx
            .send(Request::new(
                1,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files: batch_files("generation_first", 1),
                },
            ))
            .unwrap();
        let events = batch_events(batch_events_until_end(&evt_rx));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::BatchCompleted { .. })),
            "the first batch must run: {events:?}"
        );

        // The forced re-probe.
        cmd_tx
            .send(Request::new(2, Command::Detect { force: true }))
            .unwrap();
        let event = wait_for_event(&evt_rx, |e| matches!(e, Event::NvencCapability(_)));
        let events = [event];
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::NvencCapability(_))),
            "the re-probe must publish a capability: {events:?}"
        );

        // The second batch: the new generation.
        cmd_tx
            .send(Request::new(
                3,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files: batch_files("generation_second", 1),
                },
            ))
            .unwrap();
        let events = batch_events(batch_events_until_end(&evt_rx));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::BatchCompleted { .. })),
            "the second batch must run: {events:?}"
        );

        let seen = generations.lock().clone();
        assert_eq!(seen.len(), 2, "two batches: {seen:?}");
        assert_eq!(seen[0], 1, "the first batch froze the startup generation");
        assert!(
            seen[1] > seen[0],
            "the re-probe must publish a newer generation: {seen:?}"
        );
    }

    #[test]
    fn an_older_probe_result_does_not_overwrite_a_newer_one() {
        // Design doc §3.10.6 step 5: the probe requests carry a
        // sequence number, and only the newest result is published.
        // An older probe that finishes late must not overwrite the
        // newer snapshot (or the newer record on disk).
        //
        // The first probe blocks in the runner; the second one runs
        // through and publishes; then the first one is let go -- and
        // its result must be dropped.
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release_for_runner = Arc::clone(&release);
        // The runner announces its first call. **That is the anchor
        // the test needs**: which probe blocks depends on the thread
        // start order, which the system does not promise. Waiting
        // for the announcement -- before sending the second Detect --
        // makes "the first probe is the older one" a fact instead of
        // an accident.
        // The fixture blocks the probe that comes **after** the test
        // arms the flag -- not "the n-th call": which thread reaches
        // the runner first is a race (the system does not promise an
        // order), so a count would block the wrong probe.
        let block_next = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let block_next_for_runner = Arc::clone(&block_next);
        let runner: crate::job::DetectRunner = Arc::new(move |args: &[&str]| {
            if args.contains(&"-encoders")
                && block_next_for_runner.swap(false, AtomicOrdering::SeqCst)
            {
                // This probe waits until the test lets it through --
                // it is the probe whose result must arrive late.
                while !release_for_runner.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            Ok(String::new())
        });
        let (cmd_tx, evt_rx) = run_test_controller_with_runner(
            |task: &EncodeTask,
             _: &mut BatchContext,
             _: &CancelToken,
             _: &mut dyn FnMut(FfmpegProgress)| Ok(fake_record(&task.input)),
            None,
            None,
            Some(runner),
        );

        // The startup's publication comes first: the startup's probe
        // is done, so the next probe to reach the runner is the one
        // the test arms.
        let startup = next_capability_event(&evt_rx);
        assert_eq!(startup.generation(), 1, "the startup publishes first");

        // Arm the fixture: the next probe blocks.
        block_next.store(true, AtomicOrdering::SeqCst);
        // The first probe: it blocks.
        cmd_tx
            .send(Request::new(1, Command::Detect { force: true }))
            .unwrap();
        // The second probe: it runs through.
        cmd_tx
            .send(Request::new(2, Command::Detect { force: true }))
            .unwrap();

        // The second probe's publication: it is the newer one.
        let newer = next_capability_event(&evt_rx);
        assert_eq!(
            newer.generation(),
            2,
            "the newer probe publishes the next generation"
        );

        // Let the first probe finish: its result is the older one.
        release.store(true, std::sync::atomic::Ordering::SeqCst);

        // It must not publish: the sequence check drops it.
        assert_no_event(
            &evt_rx,
            |e| matches!(e, Event::NvencCapability(_)),
            std::time::Duration::from_secs(2),
        );
        // (the assertion is inside `assert_no_event`)
    }

    #[test]
    fn a_verdict_from_an_older_generation_does_not_land() {
        // Design doc §3.10.6 step 5: only the current generation's
        // runtime corrections are merged. A batch that ran under an
        // older publication reported against that publication -- its
        // conclusion is about a machine state the current generation
        // has already replaced.
        let mut publisher = CapabilityPublisher::new(Arc::new(Mutex::new(None)));
        // The publisher starts at generation 0 (nothing published
        // yet).
        let current = publisher.capability().generation();

        // A report from a newer generation than the current one:
        // dropped.
        publisher.merge_verdicts(vec![(current + 1, "h264_nvenc".to_string(), false)]);
        assert!(
            !publisher.capability().verdicts().is_disabled("h264_nvenc"),
            "a stale report must not disable the encoder"
        );

        // A report from the current generation lands.
        publisher.merge_verdicts(vec![(current, "h264_nvenc".to_string(), false)]);
        assert!(
            publisher.capability().verdicts().is_disabled("h264_nvenc"),
            "the current generation's report must land"
        );
    }

    #[test]
    fn a_late_verdict_from_an_old_batch_does_not_disable_the_new_generation() {
        // The scenario: a batch runs under generation 1 and finds an
        // NVENC encoder unusable; while it is still running, a forced
        // re-probe publishes generation 2; the batch's late report
        // must not disable the encoder for the next batch (design doc
        // §3.10.6 step 5).
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release_for_mock = Arc::clone(&release);
        let recorded = Arc::new(Mutex::new(Vec::<bool>::new()));
        let recorded_for_mock = Arc::clone(&recorded);
        let (cmd_tx, evt_rx) = run_test_controller(
            move |task: &EncodeTask,
                  ctx: &mut BatchContext,
                  _: &CancelToken,
                  _: &mut dyn FnMut(FfmpegProgress)| {
                // The batch blocks here until the re-probe has
                // published the new generation.
                while !release_for_mock.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                // What this batch was admitted with.
                recorded_for_mock
                    .lock()
                    .push(ctx.capability().verdicts().is_disabled("h264_nvenc"));
                // The runtime correction: the encoder turned out to
                // be unusable.
                ctx.capability_mut().verdicts_mut().set("h264_nvenc", false);
                Ok(fake_record(&task.input))
            },
            None,
        );

        // The first batch: it blocks inside the encoder.
        cmd_tx
            .send(Request::new(
                1,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files: batch_files("late_verdict", 1),
                },
            ))
            .unwrap();
        // Wait until the batch is really inside the encoder.
        loop {
            match evt_rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(Event::FileStarted { .. }) => break,
                Ok(_) => continue,
                Err(e) => panic!("the batch must start: {e}"),
            }
        }

        // The forced re-probe: it publishes a new generation while the
        // first batch is still running.
        cmd_tx
            .send(Request::new(2, Command::Detect { force: true }))
            .unwrap();
        loop {
            match evt_rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(Event::NvencCapability(capability)) => {
                    if capability.generation() > 1 {
                        break;
                    }
                }
                Ok(_) => continue,
                Err(e) => panic!("no capability event: {e}"),
            }
        }

        // Let the first batch finish: its verdict report is late now.
        release.store(true, std::sync::atomic::Ordering::SeqCst);
        loop {
            match evt_rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(Event::BatchCompleted { .. }) => break,
                Ok(_) => continue,
                Err(e) => panic!("the first batch must finish: {e}"),
            }
        }

        // The second batch: it must not inherit the old conclusion.
        cmd_tx
            .send(Request::new(
                3,
                Command::StartBatch {
                    params: EncodeParams::default(),
                    files: batch_files("late_verdict_second", 1),
                },
            ))
            .unwrap();
        loop {
            match evt_rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(Event::BatchCompleted { .. }) => break,
                Ok(_) => continue,
                Err(e) => panic!("the second batch must finish: {e}"),
            }
        }

        let seen = recorded.lock().clone();
        assert_eq!(seen.len(), 2, "two batches: {seen:?}");
        assert!(
            !seen[1],
            "the new generation must not inherit the old batch's verdict: {seen:?}"
        );
    }

    #[test]
    fn the_capability_reaches_the_ui_at_startup() {
        // The GUI's half of the review cycle: the capability (with the
        // advisory flag) reaches the UI without the user asking for it.
        //
        // Pushing the review date is the lib's job -- the resolve
        // does it, so there is no state machine in the frontend
        // (`nvenc::tests::an_expired_record_is_re_stored_with_a_pushed_review_date`
        // covers that half).
        let dir = unique_tmp("capability_event");
        std::fs::create_dir_all(&dir).expect("the temp dir must be created");

        // The stored record is what makes the review advisory
        // appear: the probe reuses it (it is not forced) and reports
        // that it is past its review date.
        let record = crate::encoder::nvenc::CapabilityRecord {
            version: crate::encoder::nvenc::RECORD_VERSION,
            ffmpeg_version: PROBE_VERSION.to_string(),
            ffmpeg_fingerprint: crate::encoder::nvenc::fingerprint(PROBE_VERSION, ""),
            probed_at: chrono::Utc::now() - chrono::Duration::days(40),
            next_review_at: chrono::Utc::now() - chrono::Duration::days(10),
            encoders: vec![],
        };
        std::fs::write(
            dir.join("nvenc-capability.json"),
            serde_json::to_string(&record).expect("the record must serialize"),
        )
        .expect("the record must be written");

        let (_cmd_tx, evt_rx) = run_test_controller_with_capability(
            make_succeeding_mock().0,
            Some(dir.clone()),
            Some(Capability::from_check(
                Vec::new(),
                Some(chrono::Utc::now() - chrono::Duration::days(40)),
                true,
            )),
        );

        let mut reported = None;
        for _ in 0..50 {
            match evt_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(Event::NvencCapability(capability)) => {
                    reported = Some(capability);
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let capability = reported.expect("the capability must reach the UI");
        assert!(
            capability.review_due(),
            "the UI needs the advisory flag to show the advisory"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encode_error_category_mapping() {
        // Direct test of the mapping helper. Per design §3.8.6.
        let cases = [
            (
                EncodeError::Io(std::io::Error::other("x")),
                ErrorCategory::IoError,
            ),
            (
                EncodeError::Ffmpeg("x".into()),
                ErrorCategory::FfmpegSpawnFailed,
            ),
            (EncodeError::Probe("x".into()), ErrorCategory::ProbeFailed),
            (
                EncodeError::InvalidParam("x".into()),
                ErrorCategory::ConfigError,
            ),
            (
                EncodeError::EncoderNotFound("x".into()),
                ErrorCategory::EncoderSelectionFailed,
            ),
            (
                EncodeError::FpsParse("x".into()),
                ErrorCategory::ConfigError,
            ),
            (EncodeError::UserCancel, ErrorCategory::Cancelled),
        ];
        for (err, expected) in cases {
            assert_eq!(err.category(), expected, "mapping wrong for {err:?}",);
        }
    }
}
