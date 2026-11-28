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
//! A second `StartBatch` while one is running is **rejected**
//! (`Event::CommandRejected`) rather than queued — the running batch
//! is untouched (design doc §3.11.3).
//!
//! The batch's chain (§3.10.1.2): the actor admits the batch, waits
//! for the **facts** (the capability + the encoder cache) to be
//! published, and only then starts the worker — with the facts
//! frozen into it. The waiting belongs to the actor, so a cancel can
//! interrupt it.

use crossbeam_channel::{Receiver, Sender, after, bounded, never, select, unbounded};
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use chrono::{DateTime, Utc};

use crate::analyzer::records::{AnalyzeProgress, analyze_records_with_probe_and_progress_parallel};
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
    /// Check this machine's NVENC capability and report it back.
    ///
    /// **One check, and only when it is needed** (design doc
    /// §3.10.7). There is no routine re-check: the machine's facts are
    /// established once and shared by every batch. A batch never asks
    /// for a check of its own -- it takes the published fact.
    ///
    /// * `ignore_record = false` -- *ensure the fact holds*: the
    ///   stored record is reused when it is fresh and its fingerprint
    ///   matches the current ffmpeg build; only when it does not is
    ///   the machine checked. This is the chain's first stage (the
    ///   startup probe) and the CLI's default run.
    /// * `ignore_record = true` -- *the user says the record is
    ///   stale*: the machine is checked once. Nobody but the user
    ///   knows whether the machine changed (a driver was installed,
    ///   a GPU was enabled), so this is the user's call -- the UI's
    ///   "check now" button and the CLI's `--detect`.
    ///
    /// Either way: **at most one check per request**, and the answer
    /// is published and shared by every batch (design doc §3.10.6
    /// step 5).
    CheckCapability { ignore_record: bool },
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
    /// A batch's work has started. `id` is unique within the
    /// controller's lifetime. `total_files` is the count after
    /// collection.
    ///
    /// It fires when the **worker** starts -- not when the request is
    /// accepted. The two are different moments: the batch is accepted
    /// first and then waits for the facts (the chain's barrier,
    /// design doc §3.10.1.2). The UI's `Encoding` phase follows
    /// **this** event, so it means "the worker is running"
    /// (design doc §3.11.3).
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

    /// The NVENC capability of this machine (after a
    /// `Command::CheckCapability` or the startup resolve). The UI shows
    /// it next to the NVENC switch: which hardware encoders run here,
    /// and why not.
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
            Command::CheckCapability {
                ignore_record: true,
            },
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
                | Command::CheckCapability { .. }
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

    // The capability check runs on its own thread and hands the
    // result back; the actor publishes it (design doc §3.10.6
    // step 5). The actor can drain cmd_rx while the check runs.
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

/// Where a batch is in its chain (design doc §3.10.6).
///
/// The chain has one barrier before the work starts: the **facts**
/// (the capability and the encoder cache) must be published before a
/// batch can encode with them. The publisher releases that barrier;
/// the batch does not poll for it.
///
/// No active batch means the chain is over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    /// Admitted; waiting for the facts to be published.
    Facts,
    /// The worker is running.
    Work,
}

/// The active batch: its id, where it is in its chain, and its token.
struct ActiveBatch {
    /// The batch's id (design doc §3.11.3).
    id: BatchId,
    /// Where the batch is in its chain.
    stage: Stage,
    /// The batch's cancel token.
    token: CancelToken,
}

/// A batch that is admitted but has not started yet: it waits for the
/// facts (the barrier).
///
/// The work is a closure: the actor holds the batch's inputs until the
/// facts land, and then hands them over **frozen** (the generation the
/// batch runs under).
struct PendingBatch {
    /// The batch's id.
    id: BatchId,
    /// How many files the batch has (the cancel event reports it).
    total: usize,
    /// When the facts must have landed. Past it, the batch fails
    /// instead of hanging (the gate would stay closed forever).
    deadline: std::time::Instant,
    /// Starts the work, with the facts.
    start: Box<dyn FnOnce(PublishedState) + Send>,
}

/// The actor's own state: the batch gate, the active batch and the
/// capability publication.
///
/// The admission lives here as **one operation**: every batch kind
/// takes its gate, its id and its cancel token from it, so none of
/// them can inherit the previous batch's, and none of them has to
/// remember to reset anything (design doc §3.11.2).
struct ActorState {
    /// The active batch -- `None` means the gate is open (§3.4).
    active: Option<ActiveBatch>,
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
    ///
    /// The batch starts in [`Stage::Facts`]: it waits for the facts
    /// before it works (the chain's barrier).
    fn admit_batch(&mut self, cancel_file: Option<&std::path::Path>) -> BatchAdmission {
        let id = self.next_batch_id;
        self.next_batch_id += 1;
        let token = match cancel_file {
            Some(path) => {
                let _ = std::fs::remove_file(path);
                CancelToken::watching(path.to_path_buf())
            }
            None => CancelToken::new(),
        };
        self.active = Some(ActiveBatch {
            id,
            stage: Stage::Facts,
            token: token.clone(),
        });
        BatchAdmission { id, token }
    }

    /// The batch ended: open the gate and forget the active batch.
    fn finish_batch(&mut self) {
        self.active = None;
    }

    /// Is a batch in flight? The busy gate (one batch at a time,
    /// §3.4).
    fn is_running(&self) -> bool {
        self.active.is_some()
    }

    /// Where the active batch is in its chain.
    fn stage(&self) -> Option<Stage> {
        self.active.as_ref().map(|b| b.stage)
    }

    /// The chain moves on: the batch's work starts.
    fn set_stage(&mut self, batch_id: BatchId, stage: Stage) {
        if let Some(active) = self.active.as_mut().filter(|b| b.id == batch_id) {
            active.stage = stage;
        }
    }

    /// Cancel the active batch -- if the request names it.
    ///
    /// A cancel that names another batch is refused: a stale cancel
    /// (the user cancelled, then a new batch started) must not stop
    /// the batch that is running now (design doc §3.11.3).
    fn cancel(&mut self, batch_id: BatchId) -> bool {
        let Some(active) = self.active.as_mut().filter(|b| b.id == batch_id) else {
            return false;
        };
        active.token.cancel();
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
                // failed check would discard the startup's
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
    /// The known cost: after a check publishes a new generation,
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

    /// The published fact, if it is there.
    ///
    /// The chain's barrier: a batch waits until this returns `Some`.
    /// The publisher is the only writer, so the answer is one
    /// consistent snapshot (the cache and the capability of the same
    /// publication -- design doc §3.10.6 step 5).
    fn published_fact(&self) -> Option<PublishedState> {
        self.state.lock().clone()
    }
}

/// Spawn a capability probe and hand the result to the actor.
///
/// The probe runs off the actor thread (it spawns ffmpeg); the
/// **publisher** decides what to do with the result (design doc
/// §3.10.6 step 5).
///
/// The startup probe and a user-requested check take **the same
/// path** -- the startup is simply a check that does not ignore the
/// record (it reuses a fresh record instead of probing again). One
/// path, one rule.
///
/// `ignore_record` is the whole difference between "ensure the fact
/// holds" and "the user says the record is stale" -- see
/// [`Command::CheckCapability`].
fn spawn_probe<F>(
    seq: u64,
    request_id: Option<RequestId>,
    ignore_record: bool,
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
                ignore_record,
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
    /// The resolution behind a check: it carries the encoder list
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
    F: FnOnce(Sender<VerdictUpdate>) -> P + Send + 'static,
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
                // The facts are already frozen into the batch: the
                // processor builds its context from them, and there is
                // nothing left to wait for.
                let mut processor = make_processor(tx);
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
/// The context is built *in the worker*, from the facts the actor
/// froze into the batch (design doc §3.10.1.2) -- the worker waits
/// for nothing.
fn make_encode_processor(
    mut batch_ctx: BatchContext,
    batch_id: BatchId,
    cancel: CancelToken,
    evt_tx: Sender<Event>,
    dropped_counter: DropCounter,
    update_tx: Sender<VerdictUpdate>,
    encoder: Arc<dyn FileEncoder>,
) -> impl FnMut(&EncodeTask, &crate::pipeline::task::TaskContext) -> Result<EncodingRecord, EncodeError>
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

/// How long a batch waits for the facts (the capability + the encoder
/// cache) before it gives up.
///
/// The facts normally land in a second or two -- the startup check
/// probes ffmpeg. The deadline exists so a probe that never answers
/// fails the batch instead of holding the gate forever.
const FACTS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// The chain's barrier: start the pending batch, if the facts are in.
///
/// The publisher releases the barrier (the actor's probe branch), and
/// a batch that arrives after the publication does not wait at all.
/// The facts are handed over **frozen**: the batch runs under the
/// generation it was started with, whatever the publisher does later
/// (design doc §3.10.1.2, §3.10.6 step 5).
fn advance_pending(pending: &mut Option<PendingBatch>, state: &mut ActorState) {
    if pending.is_none() {
        return;
    }
    let Some(fact) = state.publisher.published_fact() else {
        return;
    };
    let p = pending.take().expect("the pending batch is there");
    state.set_stage(p.id, Stage::Work);
    (p.start)(fact);
}

/// Turn a cache slot into the cache a batch needs, or into the error
/// to report.
fn cache_from_slot(slot: CacheSlot) -> Result<Arc<EncoderCache>, (ErrorCategory, String)> {
    match slot {
        Ok(cache) => Ok(cache),
        Err(e) => Err((e.category(), format!("the encoder cache is unusable: {e}"))),
    }
}

/// The actor's context: everything the command handlers share.
///
/// One owner -- the actor thread -- so a plain struct, no lock
/// (design doc §3.10.1 rule 1). The published cell inside the
/// publisher is the one exception: the publisher thread writes it,
/// the actor reads a snapshot per batch.
///
/// The context exists so the handlers do not need a parameter bag
/// (design doc §3.10.4): the state, the paths, the channels and
/// the seams travel together.
struct Ctx {
    /// The actor's own state: the gate, the batch ids, the
    /// publication (design doc §3.11.2).
    state: ActorState,
    /// The paths: the app-level ones never move, the run-level ones
    /// follow the working directory (design doc §3.6).
    paths: Paths,
    /// The machine facts, gathered once by the capability check
    /// (design doc §3.10.7).
    machine_facts: Option<crate::capability::MachineFacts>,
    /// The batch that is admitted but waits for the facts (the
    /// chain's barrier).
    pending: Option<PendingBatch>,
    /// Where `Command::SaveConfig` writes.
    config_path: PathBuf,
    /// The records directory (the history comes from it).
    records_dir: PathBuf,
    /// The NVENC capability record on disk.
    nvenc_cache_path: PathBuf,
    /// The encoder seam (a test replaces it -- design doc §3.10.4).
    file_encoder: Arc<dyn FileEncoder>,
    /// The capability probe seam.
    detect_runner: DetectRunner,
    /// Where the events go.
    evt_tx: Sender<Event>,
    /// The dropped-event counter (diagnostics only).
    dropped_counter: DropCounter,
    /// The probe channel: the probe threads hand their result back
    /// here, and the actor publishes it.
    probe_tx: Sender<ProbeResult>,
    /// The "batch finished" channel: the worker reports back.
    done_tx: Sender<BatchDone>,
}

impl Ctx {
    /// Admit an encode batch, hand it the facts, and let it
    /// run on a worker thread.
    fn on_start_batch(
        &mut self,
        request_id: RequestId,
        params: EncodeParams,
        files: Vec<crate::types::InputPath>,
    ) {
        // Review #120: delegate per-file iteration to
        // TaskManager::run. The processor closure owns the UI
        // protocol (FileStarted / FileCompleted / Progress);
        // TaskManager owns batch/task scaffold logs +
        // partial_output::Guard registration.
        // The operation directory: the directory this batch
        // acts on (design doc §3.6). The GUI's working
        // directory can change between batches, so the
        // run-level paths follow it.
        let batch_paths = self
            .paths
            .with_op_dir(&crate::pipeline::batch::batch_target_path_for_files(&files));
        // The admission: the gate, the id and a cancel token
        // of this batch's own (design doc §3.11.2).
        let admission = self.state.admit_batch(Some(&batch_paths.cancel_file));
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
                &self.evt_tx,
                Event::CommandRejected {
                    request_id,
                    reason: e.to_string(),
                },
                &self.dropped_counter,
            );
            return;
        }
        // The batch is admitted: the request gets its answer
        // and the batch its id (design doc §3.11.3). The UI
        // moves from `Starting` to `Encoding` on this event.
        send_evt_lossy(
            &self.evt_tx,
            Event::CommandAccepted {
                request_id,
                batch_id: Some(id),
            },
            &self.dropped_counter,
        );
        // One cache snapshot per batch (a batch is a unit —
        // the encoder must not change under it).
        //
        // Review #122: extension still hoisted (unused now — kept for
        // the hoist comment / future use).
        let _ext = params.format.default_extension();
        // The tasks come from the shared construction entry
        // (design doc §3.11.1): the same names the encode
        // path and the dry run use.
        let tasks = crate::pipeline::batch::encode_tasks(&files, params.format);

        let batch_path = crate::pipeline::batch::batch_target_path_for_files(&files);

        let cancel_for_worker = admission.token.clone();
        let evt_tx_for_worker = self.evt_tx.clone();
        let dropped_for_worker = DropCounter::default();
        let file_encoder_for_worker = Arc::clone(&self.file_encoder);
        let records_dir_for_worker = self.records_dir.clone();
        let done_tx_for_worker = self.done_tx.clone();
        // The processor and the worker each hold a handle on
        // the same token (the `move` closure would otherwise
        // take it).
        let cancel_for_processor = cancel_for_worker.clone();
        let total_files = files.len();
        // The chain's barrier: the batch waits for the facts
        // (the capability + the encoder cache). The actor
        // holds it here and hands it over **frozen** -- the
        // generation the batch runs under (design doc §3.10.6
        // step 5).
        //
        // The worker does not wait for the facts: it is
        // started **with** them. That is what lets a cancel
        // interrupt the wait: the waiting belongs to the
        // actor, and a cancel is a command the actor
        // processes.
        let start = Box::new(move |fact: PublishedState| {
            // The batch's work starts **now**: the facts are in
            // and the worker is about to run. The UI's phase
            // follows this event (design doc §3.11.3) -- so
            // "Encoding" means "the worker is running", not
            // "the lib took the request".
            send_evt_lossy(
                &evt_tx_for_worker,
                Event::BatchStarted { id, total_files },
                &dropped_for_worker,
            );
            let cache = match cache_from_slot(fact.cache) {
                Ok(cache) => cache,
                Err((category, message)) => {
                    // The encoder cache is unusable: this
                    // batch cannot run (design doc §3.11.3 --
                    // a batch-scoped failure).
                    send_evt_lossy(
                        &evt_tx_for_worker,
                        Event::BatchFailed {
                            batch_id: id,
                            category,
                            message,
                        },
                        &dropped_for_worker,
                    );
                    return;
                }
            };
            let evt_tx_for_processor = evt_tx_for_worker.clone();
            let make_processor = move |update_tx: Sender<VerdictUpdate>| {
                // The capability of **this** publication: the
                // batch freezes the generation it was admitted
                // with (design doc §3.10.6 step 5).
                let batch_ctx = BatchContext::new(params, cache, fact.capability.clone());
                make_encode_processor(
                    batch_ctx,
                    id,
                    cancel_for_processor,
                    evt_tx_for_processor.clone(),
                    dropped_for_worker,
                    update_tx,
                    file_encoder_for_worker.clone(),
                )
            };

            // The batch runs on its own thread: the actor stays
            // responsive, so `Command::Cancel` reaches the
            // in-flight ffmpeg.
            spawn_task_batch(
                id,
                tasks,
                "encoding",
                batch_path,
                Some(records_dir_for_worker.clone()),
                cancel_for_worker.clone(),
                make_processor,
                BatchUpdates::new(),
                evt_tx_for_worker.clone(),
                DropCounter::default(),
                done_tx_for_worker.clone(),
            );
        });
        self.pending = Some(PendingBatch {
            id,
            total: files.len(),
            deadline: std::time::Instant::now() + FACTS_DEADLINE,
            start,
        });
        // The facts may already be there (every batch but the
        // first): then the barrier is already released.
        advance_pending(&mut self.pending, &mut self.state);
    }

    /// A dry run: the same prepare + skip as the encode path,
    /// but no ffmpeg invocation.
    fn on_dry_run(
        &mut self,
        request_id: RequestId,
        params: EncodeParams,
        files: Vec<crate::types::InputPath>,
    ) {
        // Dry-run: same per-file probe + partial + skip as
        // StartBatch, but no ffmpeg invocation. The busy gate
        // (checked above) applies so a real batch can't sneak
        // in during dry-run.
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
        let admission = self.state.admit_batch(None);
        let cancel_for_worker = admission.token.clone();
        // The dry run is a batch too: it gets a batch id, so
        // its events can be matched (design doc §3.11.3).
        let id = admission.id;
        let total = files.len();
        send_evt_lossy(
            &self.evt_tx,
            Event::CommandAccepted {
                request_id,
                batch_id: Some(id),
            },
            &self.dropped_counter,
        );
        let evt_tx_for_worker = self.evt_tx.clone();
        let done_tx_for_worker = self.done_tx.clone();
        let dropped_for_worker = DropCounter::default();
        // The chain's barrier -- the same one the encode batch
        // waits at (the facts are the facts).
        let start = Box::new(move |fact: PublishedState| {
            let cache = match cache_from_slot(fact.cache) {
                Ok(cache) => cache,
                Err((category, message)) => {
                    send_evt_lossy(
                        &evt_tx_for_worker,
                        Event::BatchFailed {
                            batch_id: id,
                            category,
                            message,
                        },
                        &dropped_for_worker,
                    );
                    return;
                }
            };
            // The capability of this publication: the
            // dry run freezes the generation too.
            // The generation this dry run froze: the
            // plan is only valid for it (design doc
            // §3.10.5).
            let generation = fact.capability.generation();
            let batch_ctx = BatchContext::new(params, cache, fact.capability.clone());
            spawn_batch_job(
                move || {
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
                done_tx_for_worker.clone(),
                None,
                evt_tx_for_worker.clone(),
                DropCounter::default(),
            );
        });
        self.pending = Some(PendingBatch {
            id,
            total,
            deadline: std::time::Instant::now() + FACTS_DEADLINE,
            start,
        });
        advance_pending(&mut self.pending, &mut self.state);
    }

    /// Cancel the active batch. Only the active one: a cancel
    /// that names another batch is refused.
    fn on_cancel(&mut self, request_id: RequestId, batch_id: BatchId) {
        // Only the active batch may be cancelled (design doc
        // §3.11.3): a cancel that names another batch -- a
        // stale one, or a batch that already ended -- is
        // refused. It must not stop whatever runs now.
        if !self.state.cancel(batch_id) {
            send_evt_lossy(
                &self.evt_tx,
                Event::CommandRejected {
                    request_id,
                    reason: format!("no active batch {batch_id} to cancel"),
                },
                &self.dropped_counter,
            );
            return;
        }
        send_evt_lossy(
            &self.evt_tx,
            Event::CommandAccepted {
                request_id,
                batch_id: Some(batch_id),
            },
            &self.dropped_counter,
        );
        // The token is marked (by `state.cancel`): the worker
        // polls it -- the in-flight ffmpeg is killed (and its
        // partial output removed), and the batch stops at the
        // next file boundary.
        //
        // **Unless** the batch has not started yet: then it is
        // waiting at the chain's barrier (the facts), and the
        // wait ends here -- the worker is never started. That
        // is the whole point of the waiting belonging to the
        // actor: a cancel is a command, and the actor is the
        // one who processes it.
        if self.state.stage() == Some(Stage::Facts)
            && let Some(p) = self.pending.take()
        {
            self.state.finish_batch();
            send_evt_lossy(
                &self.evt_tx,
                Event::BatchCancelled {
                    batch_id: p.id,
                    completed: 0,
                    total: p.total,
                },
                &self.dropped_counter,
            );
        }
    }

    /// Read the records the earlier runs wrote.
    fn on_load_history(
        &mut self,
        request_id: RequestId,
        limit: Option<usize>,
        after: Option<DateTime<Utc>>,
    ) {
        send_evt_lossy(
            &self.evt_tx,
            Event::CommandAccepted {
                request_id,
                batch_id: None,
            },
            &self.dropped_counter,
        );
        let records = load_history(Some(self.records_dir.clone()), limit, after);
        send_evt_lossy(
            &self.evt_tx,
            Event::HistoryLoaded { records },
            &self.dropped_counter,
        );
    }

    /// Persist the UI's edited draft.
    fn on_save_config(&mut self, request_id: RequestId, config: Box<AppConfig>, revision: u64) {
        send_evt_lossy(
            &self.evt_tx,
            Event::CommandAccepted {
                request_id,
                batch_id: None,
            },
            &self.dropped_counter,
        );
        // The payload is the UI's edited draft. Persisting our
        // own copy would silently drop the edit: the file
        // would be rewritten with the original values while
        // the UI showed "Saved".
        match config.save(&self.config_path) {
            Ok(()) => {
                send_evt_lossy(
                    &self.evt_tx,
                    Event::ConfigSaved {
                        path: self.config_path.clone(),
                        revision,
                    },
                    &self.dropped_counter,
                );
            }
            Err(e) => {
                // A failed save is a **non-batch** failure: it
                // finalizes nothing (design doc §3.11.3).
                send_evt_lossy(
                    &self.evt_tx,
                    Event::OperationFailed {
                        request_id,
                        category: ErrorCategory::ConfigError,
                        message: format!("save failed: {e}"),
                    },
                    &self.dropped_counter,
                );
            }
        }
        // The `nv` switch may have changed -- and that is all
        // that changes: the capability is a fact about the
        // machine, so the cache stays as it is. The next batch
        // picks the new `nv` up from the run parameters (design
        // doc §3.10.5).
    }

    /// One capability check, and only when it is needed
    /// (design doc §3.10.7).
    fn on_check_capability(&mut self, request_id: RequestId, ignore_record: bool) {
        send_evt_lossy(
            &self.evt_tx,
            Event::CommandAccepted {
                request_id,
                batch_id: None,
            },
            &self.dropped_counter,
        );
        // One check, and only when it is needed (design doc
        // §3.10.7). Same path as the CLI's `rencoder detect`:
        // resolve through the on-disk record, store the
        // fresh result, and report what this machine can
        // actually run.
        //
        // The probe spawns ffmpeg subprocesses (up to ~1.5s),
        // so it runs on its own thread: doing it here would
        // stall every other command -- `Command::Cancel`
        // included -- for the duration.
        let cache_path = self.nvenc_cache_path.clone();
        let detect_runner = Arc::clone(&self.detect_runner);
        // The probe request takes the next sequence number:
        // the actor publishes the result only if it is the
        // newest one (design doc §3.10.6 step 5).
        let seq = self.state.publisher.request();
        spawn_probe(
            seq,
            Some(request_id),
            ignore_record,
            cache_path.clone(),
            move |args| detect_runner(args),
            self.probe_tx.clone(),
        );
    }

    /// Move the operation directory (design doc §3.6) and
    /// re-check the filesystem.
    fn on_set_working_dir(&mut self, request_id: RequestId, dir: PathBuf) {
        send_evt_lossy(
            &self.evt_tx,
            Event::CommandAccepted {
                request_id,
                batch_id: None,
            },
            &self.dropped_counter,
        );
        // The operation directory follows the working
        // directory (design doc §3.6): the run-level paths
        // move with it.
        self.paths = self.paths.with_op_dir(&dir);
        // Re-check (design doc §3.10.7): the directory
        // changed, the machine did not -- so the machine's
        // facts are reused and only the filesystem is checked
        // anew. The report lands in the new operation
        // directory.
        let facts = self.machine_facts.clone().unwrap_or_default();
        crate::capability::check_run_capabilities(&self.paths, &facts);
        send_evt_lossy(
            &self.evt_tx,
            Event::CapabilityChecked {
                report: self.paths.capability_report.clone(),
            },
            &self.dropped_counter,
        );
    }

    /// Analyze the files: a batch like the others, but it does
    /// not wait for the facts -- it probes the media.
    fn on_analyze(&mut self, request_id: RequestId, files: Vec<crate::types::InputPath>) {
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
        let id = self.state.admit_batch(None).id;
        // The analyze batch does not wait for the facts: it
        // probes the media, it does not encode with them. So
        // its chain has no barrier -- it starts right away.
        self.state.set_stage(id, Stage::Work);
        send_evt_lossy(
            &self.evt_tx,
            Event::CommandAccepted {
                request_id,
                batch_id: Some(id),
            },
            &self.dropped_counter,
        );
        send_evt_lossy(
            &self.evt_tx,
            Event::AnalyzeStarted {
                batch_id: id,
                total,
            },
            &self.dropped_counter,
        );
        let evt_tx_for_worker = self.evt_tx.clone();
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
            self.done_tx.clone(),
            None,
            self.evt_tx.clone(),
            DropCounter::default(),
        );
    }
}

fn run_actor(cmd_rx: Receiver<Request>, evt_tx: Sender<Event>, env: ActorEnv) {
    // The published state: the encoder cache and the capability, as
    // one answer (design doc §3.10.6 step 5). The publisher writes
    // it; the actor reads one snapshot per batch and hands it to the
    // worker frozen.
    //
    // The slot holds `Result<EncoderCache, EncodeError>` so a FAILED
    // check stays observable: the batch sees `Err(...)` and can
    // surface a meaningful "the encoder cache is unusable" error
    // rather than a generic "cache not available" fallback.
    //
    // The cell holds an `Arc` so the read path is a pointer clone:
    // the reader drops the lock immediately and then encodes without
    // holding it. Holding a lock across an encode (minutes) would
    // block the publisher for the whole file.
    //
    // A plain `Mutex` is the right primitive -- there is exactly one
    // reader (the actor thread, which takes a snapshot per batch) and
    // one writer (the publisher), so a read/write split would buy
    // nothing.
    let published_cell: PublishedCell = Arc::new(Mutex::new(None));
    // The probe channel: the probe threads hand their result back
    // here, and the actor publishes it (design doc §3.10.6 step 5).
    let (probe_tx, probe_rx) = unbounded::<ProbeResult>();
    // Busy gate: one batch in flight at a time (§3.4 / §3.8.1).
    //
    // A plain `bool` in the actor - no lock, no atomic. The batch
    // itself runs on a worker thread (§3.10.1.1), so the actor is the
    // only reader and the only writer.
    // "This batch is done" signals. The worker sends one when it
    // finishes; the actor reopens the busy gate when it reads one.
    let (done_tx, done_rx) = bounded::<BatchDone>(1);
    // The actor's context: its state, its paths, its channels and
    // its seams. One owner (the actor thread) -- no lock (design doc
    // §3.10.1 rule 1).
    let mut ctx = Ctx {
        state: ActorState {
            active: None,
            next_batch_id: 0,
            publisher: CapabilityPublisher::new(published_cell.clone()),
        },
        // The paths: the app-level ones never move, the run-level
        // ones follow the working directory (design doc §3.6).
        paths: env.paths.clone(),
        // The machine facts, gathered once by the capability check
        // (design doc §3.10.7). The actor adopts them: a check after
        // a working directory change reuses them instead of
        // probing the machine again.
        //
        // A plain field, not a cell: the actor is the only reader
        // *and* the only writer (the probes hand their result to
        // the actor, which publishes it) -- one owner, so no lock
        // (§3.10.1 rule 1).
        machine_facts: None,
        pending: None,
        config_path: env.config_path.clone(),
        records_dir: env.paths.records_dir.clone(),
        nvenc_cache_path: env.paths.capability_record.clone(),
        file_encoder: Arc::clone(&env.file_encoder),
        detect_runner: Arc::clone(&env.detect_runner),
        evt_tx,
        // Review #111: dropped-event counter for diagnostics. Each
        // sender owns one (it is never read by anyone but its own
        // thread).
        dropped_counter: DropCounter::default(),
        probe_tx: probe_tx.clone(),
        done_tx,
    };
    // The chain's **first stage**: "ensure the fact holds". It runs on
    // a worker (it spawns ffmpeg) and the actor publishes its result
    // (design doc §3.10.6 step 5).
    //
    // The startup check does **not** ignore the record: a fresh record
    // on disk is reused as-is (the record is shared with the CLI), a
    // missing or mismatched one is checked once. The path is the same
    // one a user-requested check takes -- only `ignore_record` differs.
    {
        let seq = ctx.state.publisher.request();
        // The **injected** runner: a test must never touch the real
        // machine (design doc §3.10.4), and the production runner is
        // the same seam (the actor's environment carries it).
        let detect_runner_for_startup = Arc::clone(&ctx.detect_runner);
        spawn_probe(
            seq,
            None,
            false,
            ctx.nvenc_cache_path.clone(),
            move |args| detect_runner_for_startup(args),
            probe_tx.clone(),
        );
    }
    // `app_config` is still held (referenced by `config_path`
    // for the SaveConfig handler). Don't drop yet.
    //
    // The "no deadline" case: created once, not per iteration (the
    // actor's loop is hot).
    let no_deadline: Receiver<std::time::Instant> = never();

    loop {
        // The pending batch's deadline: the facts must land in time.
        // Without it, a probe that never answers would hold the gate
        // forever.
        let deadline_rx = match &ctx.pending {
            Some(p) => after(
                p.deadline
                    .saturating_duration_since(std::time::Instant::now()),
            ),
            None => no_deadline.clone(),
        };
        // Wait for the next command OR for a worker to report "done".
        // The done branch re-opens the gate and goes back to waiting.
        let request = select! {
            recv(cmd_rx) -> req => req,
            recv(done_rx) -> done => {
                if let Ok(BatchDone { updates }) = done {
                    ctx.state.publisher.merge_verdicts(updates);
                    ctx.state.finish_batch();
                }
                continue;
            }
            recv(deadline_rx) -> _ => {
                // The facts did not land in time: the batch fails
                // instead of hanging.
                if let Some(p) = ctx.pending.take() {
                    ctx.state.finish_batch();
                    send_evt_lossy(
                        &ctx.evt_tx,
                        Event::BatchFailed {
                            batch_id: p.id,
                            category: ErrorCategory::ConfigError,
                            message: "the capability check timed out".into(),
                        },
                        &ctx.dropped_counter,
                    );
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
                    if let Some(publication) = ctx.state.publisher.publish(
                        probed.seq,
                        outcome,
                        ctx.nvenc_cache_path.as_path(),
                    ) {
                        match publication.failure {
                            Some((category, message)) => {
                                // The startup failure is the actor's
                                // own (no scope); a user's check's failure
                                // belongs to the request that asked
                                // for it.
                                match request_id {
                                    Some(request_id) => send_evt_lossy(
                                        &ctx.evt_tx,
                                        Event::OperationFailed {
                                            request_id,
                                            category,
                                            message,
                                        },
                                        &ctx.dropped_counter,
                                    ),
                                    None => send_evt_lossy(
                                        &ctx.evt_tx,
                                        Event::Error { category, message },
                                        &ctx.dropped_counter,
                                    ),
                                }
                            }
                            None => {
                                ctx.machine_facts = Some(publication.facts.clone());
                                // The run capability report for the
                                // current operation directory.
                                crate::capability::check_run_capabilities(
                                    &ctx.paths,
                                    &publication.facts,
                                );
                                send_evt_lossy(
                                    &ctx.evt_tx,
                                    Event::CapabilityChecked {
                                        report: ctx.paths.capability_report.clone(),
                                    },
                                    &ctx.dropped_counter,
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
                                    &ctx.evt_tx,
                                    Event::NvencCapability(publication.capability),
                                    &ctx.dropped_counter,
                                );
                            }
                        }
                    }
                }
                // The publication released the chain's barrier: the
                // waiting batch starts, with this version of the
                // facts frozen into it.
                advance_pending(&mut ctx.pending, &mut ctx.state);
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
            ctx.state.publisher.merge_verdicts(updates);
            if finished {
                ctx.state.finish_batch();
            }
            if ctx.state.is_running() {
                // Benign rejection: the command is dropped, the running
                // batch continues. `Event::Error` would be fatal-only
                // and the UI would finalize the running batch.
                send_evt_lossy(
                    &ctx.evt_tx,
                    Event::CommandRejected {
                        request_id,
                        reason: "another batch is already running".into(),
                    },
                    &ctx.dropped_counter,
                );
                continue;
            }
        }

        match command {
            Command::StartBatch { params, files } => {
                tracing::debug!(request_id, files = files.len(), "the actor: StartBatch");
                ctx.on_start_batch(request_id, params, files)
            }
            Command::DryRun { params, files } => {
                tracing::debug!(request_id, files = files.len(), "the actor: DryRun");
                ctx.on_dry_run(request_id, params, files)
            }
            Command::Cancel { batch_id } => {
                tracing::debug!(request_id, batch_id, "the actor: Cancel");
                ctx.on_cancel(request_id, batch_id)
            }
            Command::LoadHistory { limit, after } => {
                tracing::debug!(request_id, "the actor: LoadHistory");
                ctx.on_load_history(request_id, limit, after)
            }
            Command::SaveConfig { config, revision } => {
                tracing::debug!(request_id, revision, "the actor: SaveConfig");
                ctx.on_save_config(request_id, config, revision)
            }
            Command::CheckCapability { ignore_record } => {
                tracing::debug!(request_id, ignore_record, "the actor: CheckCapability");
                ctx.on_check_capability(request_id, ignore_record)
            }
            Command::SetWorkingDir { dir } => {
                tracing::debug!(request_id, dir = %dir.display(), "the actor: SetWorkingDir");
                ctx.on_set_working_dir(request_id, dir)
            }
            Command::Analyze { files } => {
                tracing::debug!(request_id, files = files.len(), "the actor: Analyze");
                ctx.on_analyze(request_id, files)
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
        match read_records(&path) {
            Ok(mut recs) => all.append(&mut recs),
            // A file we cannot read is **not** an empty run: say so.
            // Swallowing the error made a damaged file look like a
            // run that never happened -- and the records are gone for
            // good, so the user has to know.
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "could not read the records file; it is left out of the history"
                );
            }
        }
    }

    // Newest first; chronological reverse lets truncate() pick the
    // most recent N after the time filter.
    all.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    tracing::debug!(records = all.len(), "the history was read");

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
mod mock_tests;
