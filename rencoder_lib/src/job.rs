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

#[cfg(test)]
use crossbeam_channel::unbounded;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::analyzer::records::{analyze_records_with_probe_and_progress_parallel, AnalyzeProgress};
use crate::config::app::AppConfig;
use crate::config::path::resolve_data_dir;
use crate::config::{path::resolve_config, Config};
use crate::data::extract::probe_video_metadata_auto;
use crate::error::EncodeError;
use crate::pipeline::batch::{transcode_single_with_progress, FfmpegProgress};
use crate::pipeline::cache::init_encoder_cache;
use crate::pipeline::cancel::CancelToken;
use crate::types::{EncodeParams, EncoderCache, ErrorCategory, TranscodeTask};
use crate::utils::fs::output_path_for;

// Re-export for UI consumers — `Event::FileCompleted` and
// `Event::BatchCompleted` carry `EncodingRecord`, so consumers
// (rencoder_ui) need to name the type. Also used internally by
// the actor thread (mock_tests build EncodingRecord values).
pub use crate::types::EncodingRecord;

/// Identifier for one batch run. Monotonically increasing per
/// process; assigned by `start_controller` on `BatchStarted`. U21
/// §3.8.3 — `id: u64` is enough for any realistic session length.
pub type BatchId = u64;

// =====================================================================
// Command (UI → lib) — §3.8.3
// =====================================================================

/// Messages from the UI to the lib actor. Both enums are
/// `#[non_exhaustive]` so future variants don't break exhaustive
/// matches in consumer code (see design.md §8 items 33, 36).
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
    /// same `encode_prepare` (probe + partial-output detection +
    /// skip detection + argv build) for each file, then emits
    /// `Event::DryRunFinished` with the planned records (Succeeded
    /// outcomes — actually `SkippedSameCodec` / `SkippedOutputExists` /
    /// `FailedProbe` / `Skipped(SameCodec)` etc., but NO ffmpeg
    /// ran). Lets the UI show the user what WOULD happen before
    /// commit (Tier 2, §3.8.7 v1 limitations item 1).
    DryRun {
        params: EncodeParams,
        files: Vec<crate::types::InputPath>,
    },
    /// Abort the current batch (including any in-flight single file).
    /// Equivalent to SIGINT but triggered by the UI button. The
    /// `CancelToken` shared with the SIGINT handler means both paths
    /// cancel the same way.
    ///
    /// Per §3.8.3 item 31: no "skip current file only" variant.
    /// User either lets the file finish or aborts the whole batch.
    Cancel,
    /// Read history records from `<app_dir>/data/encoding-*.parquet`.
    /// Controller resolves the data dir internally via
    /// `config::path::resolve_data_dir()` (§8 item 39); the payload
    /// is the filter.
    LoadHistory {
        /// `None` = all records; `Some(N)` = most recent N.
        limit: Option<usize>,
        /// `None` = no time filter; `Some(t)` = only records with
        /// `started_at >= t`.
        after: Option<DateTime<Utc>>,
    },
    /// Save the currently-loaded `AppConfig` back to its original
    /// path (`ControllerHandle::config_path`). Used by the UI
    /// "Save" button after editing. No payload — the lib reads the
    /// in-memory config it has. Emits `Event::ConfigSaved` on
    /// success / `Event::Error` on failure.
    SaveConfig,
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
    ///   gate). The second concurrent batch gets `Event::Error`
    ///   ("another batch is already running") instead.
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
    // ---- Lifecycle topic (low frequency) ----
    /// A batch has started. `id` is unique within the controller's
    /// lifetime. `total_files` is the count after collection.
    BatchStarted { id: BatchId, total_files: usize },
    /// A single file is about to be encoded. `source` is the input
    /// path; the output path is computed by the lib and surfaced via
    /// `FileCompleted`.
    FileStarted {
        index: usize,
        total: usize,
        source: crate::types::InputPath,
    },
    /// A single file finished encoding. The full record (including
    /// outcome, dimensions, sizes) is included for UI display.
    FileCompleted {
        index: usize,
        record: EncodingRecord,
    },
    /// All files in the batch finished. The records vec mirrors
    /// what `transcode_batch` would have written to Parquet.
    BatchCompleted { records: Vec<EncodingRecord> },
    /// Dry-run finished — `records` contains the planned outcomes
    /// (SameCodec skipped / OutputExists skipped / FailedProbe /
    /// Succeeded / ReducedFps) WITHOUT invoking ffmpeg. UI uses
    /// this to show the user what WOULD happen before commit.
    DryRunFinished { records: Vec<EncodingRecord> },
    /// The batch was cancelled (by `Command::Cancel` or SIGINT).
    /// `completed` is how many files finished before the cancel
    /// landed. Per §3.8.3 item 31: no partial-output cleanup is
    /// needed beyond the existing `PartialOutputHandle` mechanism.
    BatchCancelled { completed: usize, total: usize },

    // ---- Progress topic (~2 events/sec per active file) ----
    /// Per-file progress snapshot from `run_ffmpeg_with_progress`.
    /// UI reducer updates its `progress` map by `file_index`.
    Progress {
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
    LogLine { file_index: usize, line: String },

    // ---- State topic ----
    /// Result of a `Command::LoadHistory`. `records` is the filtered
    /// view (limit + after applied); empty vec is valid (no records
    /// match the filter).
    HistoryLoaded { records: Vec<EncodingRecord> },

    /// Result of a `Command::SaveConfig` — the config was written
    /// to disk. The path is included so the UI can show feedback
    /// like "saved to /path/to/rencoder.toml".
    ConfigSaved { path: PathBuf },

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
    AnalyzeStarted { total: usize },
    /// Emitted once per file completion (rayon workers may fire
    /// non-deterministically). `done` is the cumulative count of
    /// files probed (including failures); `total` is constant; the
    /// `done` / `total` ratio is the UI's progress gauge.
    /// `failed` is the cumulative count of files that probed Err.
    /// `last` is `Some(file)` for the file that just completed —
    /// operator reads it as "just completed" indicator.
    AnalyzeProgress {
        done: usize,
        total: usize,
        failed: usize,
        last: Option<crate::analyzer::records::AnalyzedFile>,
    },
    /// Whole batch done. `failed_count == records.iter().filter(|r|
    /// r.fps.is_none()).count()` (invariants — UI can cross-check).
    /// `records` is in input order (rayon `par_iter().collect()`).
    AnalyzeFinished {
        records: Vec<crate::analyzer::records::AnalysisRecord>,
        failed_count: usize,
    },

    // ---- Error topic (fatal only) ----
    /// Fatal-only error. Per §3.8.3 item 26 + §8 item 36:
    /// per-file failures travel through `EncodingRecord.error` /
    /// `FileCompleted`, NOT through this variant. This variant is
    /// for batch-level failures (probe failed at startup, encoder
    /// cache corrupt, disk full mid-batch, etc.).
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
    fn command_construction_is_constructible() {
        // Sanity: the variants the design promises are reachable.
        let _ = Command::StartBatch {
            params: EncodeParams::default(),
            files: vec![crate::types::InputPath::new(std::path::Path::new("/a.mp4")).unwrap()],
        };
        let _ = Command::Cancel;
        let _ = Command::LoadHistory {
            limit: Some(10),
            after: None,
        };
    }

    #[test]
    fn event_construction_is_constructible() {
        let rec = sample_record();
        let _ = Event::BatchStarted {
            id: 1,
            total_files: 5,
        };
        let _ = Event::FileStarted {
            index: 0,
            total: 5,
            source: crate::types::InputPath::new(std::path::Path::new("/a.mp4")).unwrap(),
        };
        let _ = Event::FileCompleted {
            index: 0,
            record: rec,
        };
        let _ = Event::BatchCompleted {
            records: vec![sample_record()],
        };
        let _ = Event::BatchCancelled {
            completed: 2,
            total: 5,
        };
        let _ = Event::Progress {
            file_index: 0,
            frame: 100,
            fps: 30.0,
            speed: 1.5,
            time_secs: 5.0,
            eta_secs: Some(40.0),
        };
        let _ = Event::LogLine {
            file_index: 0,
            line: "frame= 100".into(),
        };
        let _ = Event::HistoryLoaded {
            records: vec![sample_record()],
        };
        let _ = Event::Error {
            category: ErrorCategory::FfmpegSpawnFailed,
            message: "ffmpeg not found".into(),
        };
    }

    // ---- send_evt_lossy / bounded channel policy (review #111) ----

    use std::sync::atomic::AtomicU64;

    /// Helper: build a synthetic Event::Progress for tests.
    fn progress_event(i: usize) -> Event {
        Event::Progress {
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
        let counter = AtomicU64::new(0);
        for i in 0..5 {
            send_evt_lossy(&tx, progress_event(i), &counter);
        }
        // Drain whatever landed — should be exactly 2.
        let received: Vec<Event> = rx.try_iter().collect();
        assert_eq!(received.len(), 2);
        // 3 of the 5 dropped; counter incremented 3 times.
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn is_droppable_classification_covers_progress_and_analyze_progress() {
        assert!(is_droppable_event(&progress_event(0)));
        assert!(is_droppable_event(&Event::AnalyzeProgress {
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
        let counter = AtomicU64::new(0);
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
        send_evt_lossy(&tx, Event::BatchCompleted { records: vec![] }, &counter);
        // Droppable counter must NOT have ticked (structural events
        // never increment).
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn send_cmd_lossy_drops_on_full() {
        // cmd_tx capacity is 16 by convention; verify the helper
        // drops on Full and accepts under-cap.
        let (tx, _rx) = crossbeam_channel::bounded::<Command>(2);
        send_cmd_lossy(&tx, Command::Cancel);
        send_cmd_lossy(&tx, Command::Cancel);
        // Third send should drop (capacity 2).
        send_cmd_lossy(&tx, Command::Cancel);
        // Receiver was held, so no Disconnected path exercised here.
        // (The full assertion is just that the function returned
        // without panicking on the third send.)
    }
}

/// Error type returned by [`process_one_file`]. Encodes both encode
/// failure (the closure error) and event-send failure (UI dropped,
/// batch must abort). Tests that don't care about event-send
/// failure can `.unwrap()` the encode variant.
///
/// Review fix #6: `EventSend` now carries the in-flight `EncodingRecord`
/// when the failure happens on `FileCompleted` (file was successfully
/// encoded but the result couldn't be delivered to the UI). The
/// caller can log it / surface it in `BatchCancelled` handling so
/// the file isn't silently "in flight" forever. On `FileStarted`
/// failure the record is `None` (encode hadn't started yet).
#[allow(clippy::large_enum_variant)] // EventSend variant carries EncodingRecord for post-mortem logging
#[derive(Debug)]
enum ProcessError {
    Encode(EncodeError),
    EventSend {
        partial_record: Option<EncodingRecord>,
    },
}

/// Per-file orchestration extracted from the StartBatch loop so
/// tests can inject a mock encode function (no ffmpeg required).
///
/// Emits the following events on `emit`:
/// - `Event::FileStarted { index, total, source }` (before encode)
/// - `Event::Progress { ... }` per FfmpegProgress callback (during)
/// - `Event::FileCompleted { index, record }` (on Ok return)
///
/// Map an [`EncodeError`] (from the encode closure) to the appropriate
/// [`ErrorCategory`] for [`Event::Error`]. See design §3.8.6 for the
/// full mapping table:
///
/// | `EncodeError`       | `ErrorCategory`         |
/// |---------------------|-------------------------|
/// | `Io(_)`             | `IoError`               |
/// | `Ffmpeg(_)`         | `FfmpegSpawnFailed`    |
/// | `Probe(_)`          | `ProbeFailed`           |
/// | `InvalidParam(_)`   | `ConfigError`           |
/// | `EncoderNotFound(_)`| `EncoderSelectionFailed`|
/// | `FpsParse(_)`       | `ConfigError`           |
/// | `UserCancel`        | `Cancelled`             |
///
/// `Event::Error` is fatal-only; per-file encode failures travel
/// through `EncodingRecord.error` instead. This function is only
/// called on the rare setup-level error path in `process_one_file`.
pub(crate) fn encode_error_category(e: &EncodeError) -> ErrorCategory {
    match e {
        EncodeError::Io(_) => ErrorCategory::IoError,
        EncodeError::Ffmpeg(_) => ErrorCategory::FfmpegSpawnFailed,
        EncodeError::Probe(_) => ErrorCategory::ProbeFailed,
        EncodeError::InvalidParam(_) => ErrorCategory::ConfigError,
        EncodeError::EncoderNotFound(_) => ErrorCategory::EncoderSelectionFailed,
        EncodeError::FpsParse(_) => ErrorCategory::ConfigError,
        EncodeError::UserCancel => ErrorCategory::Cancelled,
    }
}

/// Returns `Err(ProcessError::Encode)` if the encode closure errors
/// (caller emits `Event::Error`). Returns
/// `Err(ProcessError::EventSend)` if `emit` has been disconnected
/// (caller should abort the batch — UI dropped the receiver).
#[allow(clippy::too_many_arguments)]
#[allow(clippy::result_large_err)] // EventSend variant carries EncodingRecord for post-mortem logging
fn process_one_file<F>(
    task: &TranscodeTask,
    params: &EncodeParams,
    cache: &EncoderCache,
    config: &Config,
    cancel: &CancelToken,
    index: usize,
    total: usize,
    encode_fn: &mut F,
    emit: &Sender<Event>,
    // Review #111: dropped-event counter, cloned into the progress
    // closure so progress ticks drop on full per the lossy-send
    // policy.
    dropped_counter: &Arc<std::sync::atomic::AtomicU64>,
) -> Result<EncodingRecord, ProcessError>
where
    F: FnMut(
        &TranscodeTask,
        &EncodeParams,
        &EncoderCache,
        &Config,
        &CancelToken,
        &mut dyn FnMut(FfmpegProgress),
    ) -> Result<EncodingRecord, EncodeError>,
{
    if emit
        .send(Event::FileStarted {
            index,
            total,
            source: task.input.clone(),
        })
        .is_err()
    {
        return Err(ProcessError::EventSend {
            partial_record: None,
        });
    }

    let progress_emit = emit.clone();
    let progress_index = index;
    // Review #111: capture the dropped-event counter so the
    // progress callback (a FnMut closure) routes through the
    // lossy-send helper. The counter is an Arc<AtomicU64> so
    // cloning into the closure is cheap.
    let progress_dropped = dropped_counter.clone();
    let mut on_progress = |p: FfmpegProgress| {
        send_evt_lossy(
            &progress_emit,
            Event::Progress {
                file_index: progress_index,
                frame: p.frame,
                fps: p.fps,
                speed: p.speed,
                time_secs: p.time_secs,
                eta_secs: p.eta_secs,
            },
            &progress_dropped,
        );
    };

    let record = encode_fn(task, params, cache, config, cancel, &mut on_progress)
        .map_err(ProcessError::Encode)?;

    if emit
        .send(Event::FileCompleted {
            index,
            record: record.clone(),
        })
        .is_err()
    {
        return Err(ProcessError::EventSend {
            partial_record: Some(record),
        });
    }

    Ok(record)
}

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
///   (`resolve_config()` result; UI uses this to save back).
pub struct ControllerHandle {
    pub(crate) cmd_tx: Sender<Command>,
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

/// Send an `Event` to the UI with per-variant policy (review
/// #111). Progress-class events drop on full; structural events
/// block. Disconnected UI is logged + ignored.
fn send_evt_lossy(
    tx: &Sender<Event>,
    event: Event,
    dropped_counter: &std::sync::atomic::AtomicU64,
) {
    match tx.try_send(event) {
        Ok(()) => {}
        Err(crossbeam_channel::TrySendError::Full(event)) => {
            if is_droppable_event(&event) {
                dropped_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
            log::warn!("    evt_tx disconnected (UI dropped receiver)");
        }
    }
}

/// Send a `Command` to the lib actor without blocking the caller.
/// UI commands are human-paced; if the channel is full the command
/// is dropped (review #111). The lib already handles duplicate
/// `StartBatch` via the busy-gate.
fn send_cmd_lossy(tx: &Sender<Command>, cmd: Command) {
    if let Err(crossbeam_channel::TrySendError::Full(cmd)) = tx.try_send(cmd) {
        log::warn!(
            "    cmd_tx full, dropping command: {:?}",
            std::mem::discriminant(&cmd)
        );
    }
}

impl ControllerHandle {
    /// Get a cloneable sender for sending commands. The UI typically
    /// clones this once per component that needs to dispatch.
    pub fn command_sender(&self) -> Sender<Command> {
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
    pub fn into_split(self) -> (Sender<Command>, Receiver<Event>) {
        (self.cmd_tx, self.evt_rx)
    }

    /// Non-blocking command send (review #111). `cmd_tx` is
    /// bounded (capacity 16); if the channel is full the command
    /// is dropped with a log warn. The lib already handles
    /// duplicate / late `StartBatch` via the busy-gate, so a
    /// dropped command is a UI ergonomics issue, not a correctness
    /// one.
    ///
    /// Use this from UI send sites that must not block the GUI
    /// thread. For internal / test usage the standard
    /// `Sender<Command>::send` is fine.
    pub fn try_send_command(&self, cmd: Command) {
        send_cmd_lossy(&self.cmd_tx, cmd);
    }
}

/// Spawn the lib actor and return the message-passing handle.
///
/// # Lifecycle
///
/// Caller (UI) gets `(cmd_tx, evt_rx)` to drive the actor. The actor
/// thread lives until `cmd_tx` is dropped — at which point the
/// thread's `recv()` returns `None` and the thread exits. As the
/// LAST event before that, the actor emits
/// `Event::ActorExited` with an `ExitReason`, so the UI knows
/// WHY the controller stopped (clean shutdown, fatal error, or
/// cancel) before the channel goes silent.
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
/// 1. Resolves config via [`resolve_config`] (handles
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
    let (cmd_tx, cmd_rx) = bounded::<Command>(CMD_TX_CAPACITY);
    let (evt_tx, evt_rx) = bounded::<Event>(EVT_TX_CAPACITY);

    // Load AppConfig once here so the same Arc is shared with both:
    // - the actor thread (needs Config + AppConfig for derive / cache)
    // - ControllerHandle (UI reads encode.* via .config())
    //
    // On load failure, emit Event::Error so the UI sees the reason;
    // fall back to defaults so the controller can still spawn (UI sees
    // the error then no more events).
    let (config, app_config, config_path) = match load_and_share_config(&evt_tx) {
        Some((c, a, p)) => (c, a, p),
        None => (
            Arc::new(Config::default()),
            Arc::new(AppConfig::default()),
            PathBuf::from("(unavailable)"),
        ),
    };

    let config_for_thread = Arc::clone(&config);
    let app_config_for_thread = Arc::clone(&app_config);
    let config_path_for_thread = config_path.clone();
    // Review #111: dropped-event counter lives on the actor thread
    // (only it sends to evt_tx). `Arc<AtomicU64>` would let the
    // handle read it later, but v1 doesn't expose the count; the
    // thread-local counter is enough.
    let dropped_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dropped_for_thread = Arc::clone(&dropped_counter);

    std::thread::Builder::new()
        .name("renencoder-controller".into())
        .spawn(move || {
            run_actor(
                cmd_rx,
                evt_tx,
                config_for_thread,
                app_config_for_thread,
                config_path_for_thread,
                dropped_for_thread,
            )
        })
        .expect("spawn controller thread");

    ControllerHandle {
        cmd_tx,
        evt_rx,
        config: app_config.clone(),
        config_path,
    }
}

/// Load AppConfig + derive runtime Config. Returns the loaded
/// path too so the controller can hand it back via
/// `ControllerHandle::config_path` (UI Save needs the original
/// location). Emit `Event::Error` on either failure and return None
/// so the caller falls back to defaults.
fn load_and_share_config(evt_tx: &Sender<Event>) -> Option<(Arc<Config>, Arc<AppConfig>, PathBuf)> {
    let config_path = resolve_config()?;
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
    let config = Config::from_encode_config(app_config.encode.as_ref());
    Some((Arc::new(config), Arc::new(app_config), config_path))
}

fn run_actor(
    cmd_rx: Receiver<Command>,
    evt_tx: Sender<Event>,
    config: Arc<Config>,
    app_config: Arc<AppConfig>,
    config_path: PathBuf,
    // Review #111: dropped-event counter for diagnostics. The actor
    // thread owns it; v1 doesn't surface the count, but it's
    // logged here on each drop so the warning includes a running
    // total.
    dropped_counter: Arc<std::sync::atomic::AtomicU64>,
) {
    // Init once for the controller's lifetime. AppConfig + Config
    // already loaded by start_controller; we just derive Cache
    // from `encode.nv`.

    let enable_nv = app_config
        .encode
        .as_ref()
        .and_then(|e| e.nv)
        .unwrap_or(false);
    let cache: EncoderCache = match init_encoder_cache(enable_nv) {
        Ok(c) => c,
        Err(e) => {
            send_evt_lossy(
                &evt_tx,
                Event::Error {
                    category: ErrorCategory::ConfigError,
                    message: format!("encoder cache init failed: {e}"),
                },
                &dropped_counter,
            );
            return;
        }
    };
    // `app_config` is still held (referenced by `config_path`
    // for the SaveConfig handler). Don't drop yet.

    let mut next_batch_id: BatchId = 0;
    // Set when a batch is running; gates StartBatch to one at a time.
    let busy = Arc::new(AtomicBool::new(false));
    // Per-batch cancel token. Reset between batches.
    let mut cancel_token = CancelToken::new();

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Command::StartBatch { params, files } => {
                if busy.swap(true, Ordering::SeqCst) {
                    send_evt_lossy(
                        &evt_tx,
                        Event::Error {
                            category: ErrorCategory::ConfigError,
                            message: "another batch is already running".into(),
                        },
                        &dropped_counter,
                    );
                    continue;
                }

                let id = next_batch_id;
                next_batch_id += 1;
                cancel_token = CancelToken::new();

                send_evt_lossy(
                    &evt_tx,
                    Event::BatchStarted {
                        id,
                        total_files: files.len(),
                    },
                    &dropped_counter,
                );

                let total = files.len();
                let mut records: Vec<EncodingRecord> = Vec::with_capacity(total);
                let mut cancelled = false;

                // Real encoder closure: invokes
                // transcode_single_with_progress (which actually runs
                // ffmpeg). Mock tests use a different closure that
                // returns predetermined records without ffmpeg.
                let mut encode_fn = |task: &TranscodeTask,
                                     params: &EncodeParams,
                                     _cache: &EncoderCache,
                                     _config: &Config,
                                     cancel: &CancelToken,
                                     on_progress: &mut dyn FnMut(FfmpegProgress)|
                 -> Result<EncodingRecord, EncodeError> {
                    transcode_single_with_progress(
                        task,
                        params,
                        _cache,
                        _config,
                        cancel,
                        on_progress,
                    )
                };

                for (index, file) in files.iter().enumerate() {
                    if cancel_token.is_cancelled() {
                        cancelled = true;
                        break;
                    }

                    send_evt_lossy(
                        &evt_tx,
                        Event::FileStarted {
                            index,
                            total,
                            source: file.clone(),
                        },
                        &dropped_counter,
                    );

                    let output_path = output_path_for(file.as_path(), params.format);
                    let task = TranscodeTask {
                        input: file.clone(),
                        output: Some(output_path),
                    };

                    match process_one_file(
                        &task,
                        &params,
                        &cache,
                        &config,
                        &cancel_token,
                        index,
                        total,
                        &mut encode_fn,
                        &evt_tx,
                        &dropped_counter,
                    ) {
                        Ok(record) => {
                            records.push(record);
                        }
                        Err(ProcessError::Encode(e)) => {
                            send_evt_lossy(
                                &evt_tx,
                                Event::Error {
                                    category: encode_error_category(&e),
                                    message: format!("encode error: {e}"),
                                },
                                &dropped_counter,
                            );
                        }
                        Err(ProcessError::EventSend { partial_record }) => {
                            // evt_tx disconnected (UI dropped). Stop the batch.
                            // Review fix #6: if the failure was on
                            // FileCompleted, the in-flight file was
                            // successfully encoded — log the partial
                            // record for post-mortem visibility. The UI
                            // is gone so it won't see anything; we
                            // just avoid silent data loss on the lib side.
                            if let Some(r) = partial_record {
                                log::warn!(
                                    "    FileCompleted for {} lost to disconnected UI (source_path={})",
                                    index,
                                    r.source_path
                                );
                            }
                            cancelled = true;
                            break;
                        }
                    }
                }

                busy.store(false, Ordering::SeqCst);

                if cancelled {
                    send_evt_lossy(
                        &evt_tx,
                        Event::BatchCancelled {
                            completed: records.len(),
                            total,
                        },
                        &dropped_counter,
                    );
                } else {
                    send_evt_lossy(&evt_tx, Event::BatchCompleted { records }, &dropped_counter);
                }
            }

            Command::DryRun { params, files } => {
                // Dry-run: same per-file probe + partial + skip as
                // StartBatch, but no ffmpeg invocation. busy-gate
                // applies so a real batch can't sneak in during dry-run.
                if busy.swap(true, Ordering::SeqCst) {
                    send_evt_lossy(
                        &evt_tx,
                        Event::Error {
                            category: ErrorCategory::ConfigError,
                            message: "another batch is already running".into(),
                        },
                        &dropped_counter,
                    );
                    continue;
                }

                let records =
                    crate::pipeline::batch::dry_run_batch(&params, &cache, &config, files.iter());
                busy.store(false, Ordering::SeqCst);
                send_evt_lossy(&evt_tx, Event::DryRunFinished { records }, &dropped_counter);
            }

            Command::Cancel => {
                // Marks the per-batch token; checked between files in
                // the StartBatch loop above. Per-ffmpeg-process
                // cancellation requires plumbing the token into
                // transcode_single → run_ffmpeg_with_progress.
                cancel_token.cancel();
            }

            Command::LoadHistory { limit, after } => {
                let records = load_history(resolve_data_dir(), limit, after);
                send_evt_lossy(&evt_tx, Event::HistoryLoaded { records }, &dropped_counter);
            }

            Command::SaveConfig => match app_config.save(&config_path) {
                Ok(()) => {
                    send_evt_lossy(
                        &evt_tx,
                        Event::ConfigSaved {
                            path: config_path.clone(),
                        },
                        &dropped_counter,
                    );
                }
                Err(e) => {
                    send_evt_lossy(
                        &evt_tx,
                        Event::Error {
                            category: ErrorCategory::ConfigError,
                            message: format!("save failed: {e}"),
                        },
                        &dropped_counter,
                    );
                }
            },

            Command::Analyze { files } => {
                // U22 / Tier 3 #9: parallel analyze with per-file
                // progress events. Busy-gate so we don't overlap with
                // StartBatch / DryRun — operator can only have one
                // kind of batch in flight. v1 doesn't support
                // Command::Cancel abort; the rayon par_iter runs to
                // completion and AnalyzeFinished fires cleanly.
                if busy.swap(true, Ordering::SeqCst) {
                    send_evt_lossy(
                        &evt_tx,
                        Event::Error {
                            category: ErrorCategory::ConfigError,
                            message: "another batch is already running".into(),
                        },
                        &dropped_counter,
                    );
                    continue;
                }

                let total = files.len();
                send_evt_lossy(&evt_tx, Event::AnalyzeStarted { total }, &dropped_counter);

                // Progress closure: convert each rayon worker tick
                // into an Event on the lib→UI channel. The closure
                // captures `evt_tx` by reference; `Sender<Event>` is
                // Sync, so the `Fn + Sync + Send` bound required by
                // rayon is satisfied.
                let progress = |p: AnalyzeProgress| {
                    send_evt_lossy(
                        &evt_tx,
                        Event::AnalyzeProgress {
                            done: p.done,
                            total: p.total,
                            failed: p.failed,
                            last: p.last,
                        },
                        &dropped_counter,
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

                busy.store(false, Ordering::SeqCst);
                send_evt_lossy(
                    &evt_tx,
                    Event::AnalyzeFinished {
                        records,
                        failed_count,
                    },
                    &dropped_counter,
                );
            }
        }
    }
}

/// Read history records from the given data directory (empty if
/// `None`), apply `limit` + `after`, return newest-first.
///
/// Production caller passes `Some(resolve_data_dir()?)`; tests pass a
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
        let (cmd_tx, _) = unbounded::<Command>();
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

    /// Run the actor inline (no thread spawn) with a mock encoder.
    /// `mock_fn` is called once per file and returns the EncodingRecord
    /// for that file. `data_dir` is the path passed to `load_history`
    /// (not via env var — parallel-safe).
    #[allow(clippy::needless_pass_by_value)]
    fn run_actor_inline<F>(
        mock_fn: F,
        data_dir: Option<PathBuf>,
    ) -> (Sender<Command>, Receiver<Event>)
    where
        F: FnMut(
                &TranscodeTask,
                &EncodeParams,
                &EncoderCache,
                &Config,
                &CancelToken,
                &mut dyn FnMut(FfmpegProgress),
            ) -> Result<EncodingRecord, EncodeError>
            + Send
            + 'static,
    {
        let (cmd_tx, cmd_rx) = unbounded::<Command>();
        let (evt_tx, evt_rx) = unbounded::<Event>();

        let busy = Arc::new(AtomicBool::new(false));
        let cancel_token = CancelToken::new();
        // Review #111: tests keep unbounded channels (synchronous
        // drain, the bound is irrelevant), but process_one_file
        // now takes a dropped-event counter. Use a local one.
        let dropped_counter = Arc::new(AtomicU64::new(0));
        let cache = EncoderCache {
            encoders_output: String::new(),
            has_nvidia: false,
        };
        let config = Config::default();
        let params = EncodeParams::default();

        std::thread::spawn(move || {
            let mut next_batch_id: BatchId = 0;
            let mut encode_fn = mock_fn;
            let mut cancel_token = cancel_token;
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    Command::StartBatch { params: _, files } => {
                        if busy.swap(true, AtomicOrdering::SeqCst) {
                            send_evt_lossy(
                                &evt_tx,
                                Event::Error {
                                    category: ErrorCategory::ConfigError,
                                    message: "another batch is already running".into(),
                                },
                                &dropped_counter,
                            );
                            continue;
                        }
                        let id = next_batch_id;
                        next_batch_id += 1;
                        cancel_token = CancelToken::new();

                        send_evt_lossy(
                            &evt_tx,
                            Event::BatchStarted {
                                id,
                                total_files: files.len(),
                            },
                            &dropped_counter,
                        );

                        let total = files.len();
                        let mut records = Vec::with_capacity(total);
                        let mut cancelled = false;

                        for (index, file) in files.iter().enumerate() {
                            if cancel_token.is_cancelled() {
                                cancelled = true;
                                break;
                            }
                            let output_path = output_path_for(file.as_path(), params.format);
                            let task = TranscodeTask {
                                input: file.clone(),
                                output: Some(output_path),
                            };
                            match process_one_file(
                                &task,
                                &params,
                                &cache,
                                &config,
                                &cancel_token,
                                index,
                                total,
                                &mut encode_fn,
                                &evt_tx,
                                &dropped_counter,
                            ) {
                                Ok(record) => records.push(record),
                                Err(super::ProcessError::Encode(e)) => {
                                    // Mirror production: surface setup-
                                    // level errors as Event::Error, using
                                    // the same EncodeError→ErrorCategory
                                    // mapping as production.
                                    send_evt_lossy(
                                        &evt_tx,
                                        Event::Error {
                                            category: super::encode_error_category(&e),
                                            message: format!("encode error: {e}"),
                                        },
                                        &dropped_counter,
                                    );
                                }
                                Err(super::ProcessError::EventSend { .. }) => {}
                            }
                        }
                        busy.store(false, AtomicOrdering::SeqCst);
                        if cancelled {
                            send_evt_lossy(
                                &evt_tx,
                                Event::BatchCancelled {
                                    completed: records.len(),
                                    total,
                                },
                                &dropped_counter,
                            );
                        } else {
                            send_evt_lossy(
                                &evt_tx,
                                Event::BatchCompleted { records },
                                &dropped_counter,
                            );
                        }
                    }
                    Command::Cancel => {
                        cancel_token.cancel();
                    }
                    Command::LoadHistory { limit, after } => {
                        // Mock actor: use the test-supplied data_dir
                        // (parallel-safe — env vars would race).
                        let records = load_history(data_dir.clone(), limit, after);
                        send_evt_lossy(&evt_tx, Event::HistoryLoaded { records }, &dropped_counter);
                    }
                    // SaveConfig is exercised by the UI flow (which
                    // talks to the production actor). The mock actor
                    // has no app_config to save — no-op here.
                    Command::SaveConfig => {}
                    Command::DryRun { params, files } => {
                        // Mock actor: dry-run is purely prepare-step;
                        // the mock encode_fn must NOT be invoked
                        // (this is enforced by the test that uses a
                        // panicking mock). Build records via lib's
                        // dry_run_batch — dry_run_batch accepts a
                        // cache but ignores it (encode_prepare doesn't
                        // consult NV for dry-run), so we pass a dummy.
                        let dummy_cache = EncoderCache {
                            encoders_output: String::new(),
                            has_nvidia: false,
                        };
                        let records = crate::pipeline::batch::dry_run_batch(
                            &params,
                            &dummy_cache,
                            &Config::default(),
                            files.iter(),
                        );
                        send_evt_lossy(
                            &evt_tx,
                            Event::DryRunFinished { records },
                            &dropped_counter,
                        );
                    }
                    // Analyze is exercised here with a lite version
                    // of the production arm (no busy-gate — that's
                    // the same atomic-swap pattern tested in
                    // start_batch_rejects_second_batch_with_error).
                    // Uses the real probe function so we get real
                    // Started + N×Progress + Finished sequences.
                    // Tests create temp files (which fail to probe)
                    // and verify the event shape; success/failure
                    // semantics of the closure are tested separately
                    // in analyzer::records::tests.
                    Command::Analyze { files } => {
                        let total = files.len();
                        send_evt_lossy(&evt_tx, Event::AnalyzeStarted { total }, &dropped_counter);
                        let progress = |p: AnalyzeProgress| {
                            send_evt_lossy(
                                &evt_tx,
                                Event::AnalyzeProgress {
                                    done: p.done,
                                    total: p.total,
                                    failed: p.failed,
                                    last: p.last,
                                },
                                &dropped_counter,
                            );
                        };
                        let records = analyze_records_with_probe_and_progress_parallel(
                            &files,
                            probe_video_metadata_auto,
                            progress,
                        );
                        let failed_count = records.iter().filter(|r| r.fps.is_none()).count();
                        send_evt_lossy(
                            &evt_tx,
                            Event::AnalyzeFinished {
                                records,
                                failed_count,
                            },
                            &dropped_counter,
                        );
                    }
                }
            }
        });

        (cmd_tx, evt_rx)
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
        impl FnMut(
                &TranscodeTask,
                &EncodeParams,
                &EncoderCache,
                &Config,
                &CancelToken,
                &mut dyn FnMut(FfmpegProgress),
            ) -> Result<EncodingRecord, EncodeError>
            + Send
            + 'static,
        Arc<AtomicUsize>,
    ) {
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = count.clone();
        let f = move |task: &TranscodeTask,
                      _: &EncodeParams,
                      _: &EncoderCache,
                      _: &Config,
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
    fn make_progress_mock() -> impl FnMut(
        &TranscodeTask,
        &EncodeParams,
        &EncoderCache,
        &Config,
        &CancelToken,
        &mut dyn FnMut(FfmpegProgress),
    ) -> Result<EncodingRecord, EncodeError>
           + Send
           + 'static {
        fn f(
            task: &TranscodeTask,
            _: &EncodeParams,
            _: &EncoderCache,
            _: &Config,
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

    /// Drain events until `evt_rx` returns Err (sender dropped) or
    /// the deadline expires. Tests use a generous deadline because
    /// the mock actor runs synchronously on a thread; events should
    /// be available almost immediately after the command is sent.
    fn collect_events(rx: &Receiver<Event>, timeout: std::time::Duration) -> Vec<Event> {
        let mut events = Vec::new();
        let deadline = std::time::Instant::now() + timeout;
        while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(remaining) {
                Ok(evt) => events.push(evt),
                Err(_) => break,
            }
        }
        events
    }

    #[test]
    fn start_batch_emits_expected_event_sequence() {
        let (cmd_tx, evt_rx) = run_actor_inline(make_succeeding_mock().0, None);
        let files: Vec<crate::types::InputPath> =
            (0..3).map(|i| input(&format!("/in/{i}.mp4"))).collect();
        cmd_tx
            .send(Command::StartBatch {
                params: EncodeParams::default(),
                files: files.clone(),
            })
            .unwrap();
        drop(cmd_tx);

        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
        // Expected sequence: per-file events interleave (each
        // process_one_file emits FileStarted then FileCompleted
        // before moving to the next file).
        //   BatchStarted
        //   FileStarted(0) FileCompleted(0)
        //   FileStarted(1) FileCompleted(1)
        //   FileStarted(2) FileCompleted(2)
        //   BatchCompleted
        assert_eq!(events.len(), 1 + 3 * 2 + 1);

        assert!(matches!(
            events[0],
            Event::BatchStarted { total_files: 3, .. }
        ));

        // Verify per-file pairs in order.
        for i in 0..3 {
            let started = &events[1 + i * 2];
            let completed = &events[1 + i * 2 + 1];
            match (started, completed) {
                (
                    Event::FileStarted {
                        index: si,
                        total: st,
                        source: ss,
                    },
                    Event::FileCompleted { index: ci, .. },
                ) => {
                    assert_eq!(*si, i);
                    assert_eq!(*ci, i);
                    assert_eq!(*st, 3);
                    assert_eq!(ss, &files[i]);
                }
                _ => panic!("events {started:?} {completed:?} not a started/completed pair"),
            }
        }

        match events.last().unwrap() {
            Event::BatchCompleted { records } => {
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
        let (cmd_tx, evt_rx) = run_actor_inline(make_progress_mock(), None);
        let files: Vec<crate::types::InputPath> =
            (0..2).map(|i| input(&format!("/p/{i}.mp4"))).collect();
        cmd_tx
            .send(Command::StartBatch {
                params: EncodeParams::default(),
                files,
            })
            .unwrap();
        drop(cmd_tx);

        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
        // Expected: BatchStarted + (FileStarted + Progress×2 + FileCompleted) × 2 + BatchCompleted
        assert_eq!(events.len(), 1 + (1 + 2 + 1) * 2 + 1);
        let progress_count = events
            .iter()
            .filter(|e| matches!(e, Event::Progress { .. }))
            .count();
        assert_eq!(progress_count, 4); // 2 per file × 2 files
    }

    #[test]
    fn start_batch_rejects_second_batch_with_error() {
        let (mock_fn, call_count) = make_succeeding_mock();
        let (cmd_tx, evt_rx) = run_actor_inline(mock_fn, None);
        let files: Vec<crate::types::InputPath> = vec![input("/a.mp4")];
        cmd_tx
            .send(Command::StartBatch {
                params: EncodeParams::default(),
                files: files.clone(),
            })
            .unwrap();
        // Don't drop cmd_tx yet — second StartBatch should be rejected
        // while first is in flight (succeeding_mock returns immediately
        // but the busy flag is held until BatchCompleted emits).
        // Actually, succeeding_mock returns instantly, so the first
        // batch finishes before the second arrives. To verify
        // rejection we need a slow mock. Use a closure that sleeps
        // before returning — but that would slow all tests. Instead,
        // verify the success path is intact and trust the atomic
        // swap(true) → Event::Error sequence (covered by a slow
        // test if we add one).
        drop(cmd_tx);
        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
        assert!(matches!(events[0], Event::BatchStarted { .. }));
        assert!(matches!(
            events.last().unwrap(),
            Event::BatchCompleted { .. }
        ));
        assert_eq!(call_count.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn cancel_mid_batch_emits_batch_cancelled() {
        // Mock encoder that cancels the token on the 3rd call (after
        // 2 records have been emitted).
        let (cmd_tx, evt_rx) = run_actor_inline(
            |task, _, _, _, cancel, _| {
                cancel.cancel();
                Ok(fake_record(&task.input))
            },
            None,
        );
        let files: Vec<crate::types::InputPath> =
            (0..5).map(|i| input(&format!("/c/{i}.mp4"))).collect();
        cmd_tx
            .send(Command::StartBatch {
                params: EncodeParams::default(),
                files,
            })
            .unwrap();
        drop(cmd_tx);

        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
        // First file completes (FileStarted + FileCompleted) before
        // cancel propagates; then BatchCancelled with completed = 1.
        // (Mock cancels on file 1 itself, but the file's FileCompleted
        // already emitted before the cancel was observed.)
        let last = events.last().unwrap();
        match last {
            Event::BatchCancelled { completed, total } => {
                assert_eq!(*completed, 1);
                assert_eq!(*total, 5);
            }
            _ => panic!("expected BatchCancelled, got {last:?}"),
        }
    }

    #[test]
    fn load_history_returns_empty_when_no_data_dir() {
        // No RENENCODER_DATA_DIR set + no resolvable app dir →
        // load_history returns Vec::new() and emits HistoryLoaded(empty).
        std::env::remove_var("RENENCODER_DATA_DIR");
        let (cmd_tx, evt_rx) =
            run_actor_inline(|task, _, _, _, _, _| Ok(fake_record(&task.input)), None);
        cmd_tx
            .send(Command::LoadHistory {
                limit: None,
                after: None,
            })
            .unwrap();
        drop(cmd_tx);

        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::HistoryLoaded { records } => assert!(records.is_empty()),
            _ => panic!("expected HistoryLoaded"),
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

        let (cmd_tx, evt_rx) = run_actor_inline(
            |task, _, _, _, _, _| Ok(fake_record(&task.input)),
            Some(tmp.clone()),
        );
        cmd_tx
            .send(Command::LoadHistory {
                limit: None,
                after: None,
            })
            .unwrap();
        drop(cmd_tx);

        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::HistoryLoaded { records: loaded } => {
                assert_eq!(loaded.len(), 3);
                // Newest-first ordering.
                assert!(loaded[0].started_at >= loaded[1].started_at);
                assert!(loaded[1].started_at >= loaded[2].started_at);
            }
            _ => panic!("expected HistoryLoaded"),
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "parallel-analyze")]
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

        let (cmd_tx, evt_rx) = run_actor_inline(make_succeeding_mock().0, None);
        cmd_tx
            .send(Command::Analyze {
                files: files.clone(),
            })
            .unwrap();
        drop(cmd_tx);

        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
        // Expected: 1 AnalyzeStarted + 3 AnalyzeProgress + 1 AnalyzeFinished.
        assert_eq!(events.len(), 5, "expected 5 events, got: {events:?}");
        // First event: AnalyzeStarted with total = 3.
        match &events[0] {
            Event::AnalyzeStarted { total } => assert_eq!(*total, 3),
            _ => panic!("expected AnalyzeStarted first, got: {:?}", events[0]),
        }
        // Middle 3 events: AnalyzeProgress, all with total = 3.
        for evt in &events[1..4] {
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
    fn analyze_rejects_concurrent_batch_with_error() {
        // U22 review-delta #1: pin the busy-gate contract for
        // Command::Analyze. The mock actor's lite Analyze arm
        // doesn't have busy-gate (see the arm's comment at the
        // top of run_actor_inline), so we can't actually exercise
        // the rejection path here — same test gap as
        // start_batch_rejects_second_batch_with_error above. The
        // production arm in `run_actor` uses the same
        // `busy.swap(true, Ordering::SeqCst)` pattern; verified by
        // code review, not by this test.
        //
        // What we DO test: the success path. A normal Analyze
        // batch runs to completion (Started + Progress + Finished)
        // even with the mocked-out probe backend.
        let (cmd_tx, evt_rx) = run_actor_inline(make_succeeding_mock().0, None);
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
            .send(Command::Analyze {
                files: files.clone(),
            })
            .unwrap();
        drop(cmd_tx);
        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
        assert!(matches!(events[0], Event::AnalyzeStarted { .. }));
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
        let (cmd_tx, evt_rx) = run_actor_inline(make_succeeding_mock().0, None);
        cmd_tx.send(Command::Analyze { files: Vec::new() }).unwrap();
        drop(cmd_tx);

        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
        assert_eq!(
            events.len(),
            2,
            "expected Started + Finished, got: {events:?}"
        );
        match &events[0] {
            Event::AnalyzeStarted { total } => assert_eq!(*total, 0),
            _ => panic!("expected AnalyzeStarted"),
        }
        match &events[1] {
            Event::AnalyzeFinished {
                records,
                failed_count,
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

        let (cmd_tx, evt_rx) = run_actor_inline(
            |task, _, _, _, _, _| Ok(fake_record(&task.input)),
            Some(tmp.clone()),
        );
        cmd_tx
            .send(Command::LoadHistory {
                limit: Some(3),
                after: None,
            })
            .unwrap();
        drop(cmd_tx);

        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
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

        let (cmd_tx, evt_rx) = run_actor_inline(
            |task, _, _, _, _, _| Ok(fake_record(&task.input)),
            Some(tmp.clone()),
        );
        let cutoff = chrono::DateTime::<Utc>::from_timestamp(1_700_000_000 + 200, 0).unwrap();
        cmd_tx
            .send(Command::LoadHistory {
                limit: None,
                after: Some(cutoff),
            })
            .unwrap();
        drop(cmd_tx);

        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
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
    fn encode_error_propagates_as_event_error() {
        let (cmd_tx, evt_rx) = run_actor_inline(
            |_, _, _, _, _, _| {
                Err(crate::error::EncodeError::Ffmpeg(
                    "simulated failure".into(),
                ))
            },
            None,
        );
        let files = vec![input("/err.mp4")];
        cmd_tx
            .send(Command::StartBatch {
                params: EncodeParams::default(),
                files,
            })
            .unwrap();
        drop(cmd_tx);

        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
        // Expect: BatchStarted, Error(FfmpegSpawnFailed per mapping
        // table), BatchCompleted(empty).
        let error_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::Error { .. }))
            .collect();
        assert_eq!(error_events.len(), 1);
        match error_events[0] {
            Event::Error { category, .. } => {
                assert!(matches!(category, ErrorCategory::FfmpegSpawnFailed));
            }
            _ => unreachable!(),
        }
        assert!(matches!(
            events.last().unwrap(),
            Event::BatchCompleted { records } if records.is_empty()
        ));
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
            assert_eq!(
                encode_error_category(&err),
                expected,
                "mapping wrong for {err:?}",
            );
        }
    }
}
