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
use crate::config::path::resolve_config;
use crate::data::extract::probe_video_metadata_auto;
use crate::encoder::nvenc::{Capability, CapabilityVerdicts};
use crate::error::EncodeError;
use crate::pipeline::batch::{FfmpegFileEncoder, FfmpegProgress, FileEncoder};
use crate::pipeline::cancel::CancelToken;
use crate::pipeline::context::BatchContext;
use crate::pipeline::task::TaskManager;
use crate::types::{EncodeParams, EncoderCache, ErrorCategory, TranscodeTask};

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
    SaveConfig { config: Box<AppConfig> },
    /// Detect this machine's NVENC capability and report it back.
    ///
    /// Same logic as the CLI's `rencoder detect`: the result is stored
    /// in `<app_dir>/nvenc-capability.json`, and the stored record is
    /// reused when it matches the current ffmpeg build.
    ///
    /// * `force = true` -- always probe (the "重新检测" button).
    /// * `force = false` -- reuse the stored record when it is fresh.
    Detect { force: bool },
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
    /// (Succeeded for the files that would be encoded, Failed for the
    /// ones the prepare step could not handle) WITHOUT invoking
    /// ffmpeg. UI uses this to show the user what WOULD happen before
    /// commit.
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

    /// The NVENC capability of this machine (after a `Command::Detect`
    /// or the startup resolve). The UI shows it next to the NVENC
    /// switch: which hardware encoders run here, and why not.
    NvencCapability(crate::encoder::nvenc::Capability),

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

    // ---- Rejection topic (benign) ----
    /// A command was rejected; nothing fatal happened.
    ///
    /// The busy gate answers this way when a batch is already running:
    /// the command is dropped and the running batch continues. It used
    /// to answer with [`Event::Error`], which is fatal-only -- the UI
    /// read that as "the batch failed" and finalized it.
    CommandRejected { reason: String },

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
        send_evt_lossy(&tx, Event::BatchCompleted { records: vec![] }, &counter);
        // Droppable counter must NOT have ticked (structural events
        // never increment).
        assert_eq!(counter.0.load(Ordering::Relaxed), 0);
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

/// Send a `Command` to the lib actor without blocking the caller.
/// UI commands are human-paced; if the channel is full the command
/// is dropped (review #111). The lib already handles duplicate
/// `StartBatch` via the busy-gate.
fn send_cmd_lossy(tx: &Sender<Command>, cmd: Command) {
    if let Err(crossbeam_channel::TrySendError::Full(cmd)) = tx.try_send(cmd) {
        tracing::warn!(
            cmd_variant = ?std::mem::discriminant(&cmd),
            "cmd_tx full; dropping command"
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
    /// Where `Command::LoadHistory` reads from.
    pub data_dir: Option<PathBuf>,
    /// Where the NVENC capability record lives.
    pub nvenc_cache_path: Option<PathBuf>,
    /// How to encode one file (the test seam -- design doc §3.10.4).
    pub file_encoder: Arc<dyn FileEncoder>,
    /// The ffmpeg runner the NVENC probe uses (the test seam).
    pub detect_runner: DetectRunner,
}

/// The production environment: real ffmpeg, the app's own paths.
fn production_env(app_config: Arc<AppConfig>, config_path: PathBuf) -> ActorEnv {
    ActorEnv {
        app_config,
        config_path,
        data_dir: crate::config::path::resolve_data_dir(),
        nvenc_cache_path: crate::config::path::resolve_nvenc_cache(),
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
    let cache_rx = crate::pipeline::cache::init_encoder_cache_async_with_capability_at(
        env.nvenc_cache_path.clone(),
        crate::encoder::nvenc::run_ffmpeg,
    );

    std::thread::Builder::new()
        .name("renencoder-controller".into())
        .spawn(move || {
            run_actor(cmd_rx, evt_tx, env, cache_rx);
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
    let (cmd_tx, cmd_rx) = bounded::<Command>(CMD_TX_CAPACITY);
    let (evt_tx, evt_rx) = bounded::<Event>(EVT_TX_CAPACITY);

    // The cache init goes through the injected runner, so a test never
    // spawns ffmpeg (the fake runner answers every invocation). The
    // record path travels with it: a test must not write the
    // capability record into the real app directory (design doc
    // §3.10.4).
    let detect_runner_for_cache = Arc::clone(&env.detect_runner);
    let cache_rx = crate::pipeline::cache::init_encoder_cache_async_with_capability_at(
        env.nvenc_cache_path.clone(),
        move |args| detect_runner_for_cache(args),
    );

    let app_config = Arc::clone(&env.app_config);
    let config_path = env.config_path.clone();
    std::thread::Builder::new()
        .name("renencoder-controller".into())
        .spawn(move || {
            run_actor(cmd_rx, evt_tx, env, cache_rx);
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

/// The encoder cache cell: a replaceable handle.
///
/// `None` means "the cache has not landed yet" (the init thread is
/// still running); `Some(Err(..))` means the init failed and every
/// batch must see that error.
type CacheCell = Arc<Mutex<Option<CacheSlot>>>;

/// Replace the encoder cache with a freshly resolved capability.
///
/// Only the `detect` path calls this: the capability is a fact about
/// the machine, so it changes only when the machine (or the ffmpeg
/// build) changes -- not when the user flips the `nv` switch. That
/// switch is a run choice and travels in the run's parameters, so
/// flipping it never touches the cache.
///
/// The lock is held only for the pointer swap -- no IO inside
/// (§3.10.1 rule 2).
fn replace_encoder_cache(cache_cell: &CacheCell, resolution: &crate::encoder::nvenc::Resolution) {
    let cache = crate::pipeline::cache::cache_from_resolution(resolution);
    let mut guard = cache_cell.lock();
    *guard = Some(Ok(Arc::new(cache)));
}

// =====================================================================
// Batch workers — the actor never runs a batch itself (§3.10.1.1)
// =====================================================================

/// "This batch is done" signal, sent by a worker to the actor.
///
/// The actor reopens the busy gate when it reads the signal. A channel
/// is the right tool: the signal is a message, not shared state.
///
/// `updates` carries the NVENC verdicts the worker found (an encoder
/// that failed at runtime). The actor merges them into its authoritative
/// copy, so the next batch inherits them.
struct BatchDone {
    /// Encoder name → usable?
    updates: Vec<(String, bool)>,
}

/// Drain the pending "batch done" signals.
///
/// Returns whether a batch has finished, plus the verdict updates it
/// reported. The actor calls this right before it consults the busy
/// gate: a worker can report "done" while the actor is handling a
/// command, and a batch that has finished must not be mistaken for a
/// running one.
fn drain_batch_done(done_rx: &Receiver<BatchDone>) -> (bool, Vec<(String, bool)>) {
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
    updates: Option<Receiver<(String, bool)>>,
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
/// sees.
fn spawn_batch_job<F>(
    body: F,
    done_tx: Sender<BatchDone>,
    updates: Option<Receiver<(String, bool)>>,
) where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name("rencoder-batch".into())
        .spawn(move || {
            // Dropped when the body returns — or unwinds.
            let _done = DoneGuard { done_tx, updates };
            body();
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
    tx: Sender<(String, bool)>,
    rx: Receiver<(String, bool)>,
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
    tasks: Vec<TranscodeTask>,
    label: &'static str,
    batch_path: PathBuf,
    make_processor: F,
    updates: BatchUpdates,
    evt_tx: Sender<Event>,
    dropped_counter: DropCounter,
    done_tx: Sender<BatchDone>,
) where
    P: FnMut(
            &TranscodeTask,
            &crate::pipeline::task::TaskContext,
        ) -> Result<EncodingRecord, EncodeError>
        + Send
        + 'static,
    F: FnOnce(Sender<(String, bool)>) -> Result<P, (ErrorCategory, String)> + Send + 'static,
{
    let BatchUpdates { tx, rx } = updates;
    std::thread::Builder::new()
        .name("rencoder-batch".into())
        .spawn(move || {
            // Dropped when the body returns — or unwinds. The guard
            // reports "done" (and hands the verdict updates to the
            // actor) even if the batch panicked.
            let _done = DoneGuard {
                done_tx,
                updates: Some(rx),
            };
            let mut processor = match make_processor(tx) {
                Ok(processor) => processor,
                Err((category, message)) => {
                    // The batch could not start (no encoder cache).
                    // The error event is terminal for the UI.
                    send_evt_lossy(
                        &evt_tx,
                        Event::Error { category, message },
                        &dropped_counter,
                    );
                    return;
                }
            };
            let total = tasks.len();
            // `TaskManager::new(None)`: the actor path has no signal
            // handler — the cancel branch of `run_ffmpeg_with_progress`
            // deletes the partial output itself.
            let run_result = TaskManager::new(None).run(&tasks, label, &batch_path, &mut processor);
            match run_result {
                Ok(records) => {
                    send_evt_lossy(&evt_tx, Event::BatchCompleted { records }, &dropped_counter)
                }
                // The processor asked to abort (cancel). `idx` is the
                // 0-based index of the task the batch stopped at.
                Err((idx, _)) => send_evt_lossy(
                    &evt_tx,
                    Event::BatchCancelled {
                        completed: idx,
                        total,
                    },
                    &dropped_counter,
                ),
            }
        })
        .expect("spawn batch worker");
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
    cancel: CancelToken,
    evt_tx: Sender<Event>,
    dropped_counter: DropCounter,
    update_tx: Sender<(String, bool)>,
    encoder: Arc<dyn FileEncoder>,
) -> impl FnMut(
    &TranscodeTask,
    &crate::pipeline::task::TaskContext,
) -> Result<EncodingRecord, EncodeError>
       + Send
       + 'static {
    // What the actor already knows: the verdicts this batch started
    // with. The comparison tells us what the encode changed.
    let mut reported = batch_ctx.capability().verdicts().clone();
    move |task: &TranscodeTask, ctx: &crate::pipeline::task::TaskContext| {
        // FileStarted uses 0-indexed (matches the v1 UI contract).
        // TaskManager passes 1-indexed in `ctx.index`; convert.
        let idx = ctx.index - 1;

        send_evt_lossy(
            &evt_tx,
            Event::FileStarted {
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
        // so the next batch inherits them.
        for change in batch_ctx.capability().verdicts().changes_since(&reported) {
            let _ = update_tx.send(change);
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
                send_evt_lossy(
                    &evt_tx,
                    Event::Error {
                        category: e.category(),
                        message: format!("encode error: {e}"),
                    },
                    &dropped_counter,
                );
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
fn take_cache(cell: &CacheCell, timeout: std::time::Duration) -> Option<CacheSlot> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // The lock is held only for the pointer clone — no IO inside
        // (§3.10.1 rule 2).
        if let Some(slot) = { cell.lock().clone() } {
            return Some(slot);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Fold the verdicts a worker reported into the actor's copy.
fn apply_verdict_updates(verdicts: &mut CapabilityVerdicts, updates: Vec<(String, bool)>) {
    for (encoder, usable) in updates {
        verdicts.set(&encoder, usable);
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
    cmd_rx: Receiver<Command>,
    evt_tx: Sender<Event>,
    env: ActorEnv,
    // Review #126: cache init runs on a background thread; this
    // receiver yields the result when ready. The cmd_rx loop can
    // process other commands (Cancel / LoadHistory / SaveConfig)
    // while init runs. StartBatch handlers block on this when a
    // batch actually starts.
    cache_rx: std::sync::mpsc::Receiver<crate::pipeline::cache::CacheInit>,
) {
    // The injected dependencies, unpacked for the arms below.
    let config_path = env.config_path.clone();
    let data_dir = env.data_dir.clone();
    let nvenc_cache_path = env.nvenc_cache_path.clone();
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
    let cache_cell: CacheCell = Arc::new(Mutex::new(None));
    // The background init thread fills the slot.
    let cache_cell_for_thread = Arc::clone(&cache_cell);
    let evt_tx_for_init = evt_tx.clone();
    let dropped_for_init = DropCounter::default();
    std::thread::spawn(move || {
        // The receiver delivers exactly one `Result`. Use
        // `unwrap_or_else` to surface a channel-disconnect as
        // a sentinel error so the OnceLock always gets set.
        let init = cache_rx
            .recv()
            .unwrap_or_else(|_| crate::pipeline::cache::CacheInit {
                cache: Err(EncodeError::Probe(
                    "encoder cache init channel closed".into(),
                )),
                capability: None,
            });
        // Peek at the result so we can emit an error event with the
        // correct category before moving into the OnceLock.
        let err_event = match &init.cache {
            Err(e) => Some((e.category(), format!("encoder cache init failed: {e}"))),
            Ok(_) => None,
        };
        // The NVENC capability as resolved at startup -- the UI shows
        // it without the user having to ask for it.
        //
        // The review cycle is the lib's: the resolution already pushed
        // the review date when the record was due, so the UI's
        // advisory is shown once and the next start is quiet.
        if let Some(capability) = init.capability {
            send_evt_lossy(
                &evt_tx_for_init,
                Event::NvencCapability(capability),
                &dropped_for_init,
            );
        }
        {
            let mut guard = cache_cell_for_thread.lock();
            *guard = Some(init.cache.map(Arc::new).map_err(Arc::new));
        }
        if let Some((category, message)) = err_event {
            send_evt_lossy(
                &evt_tx_for_init,
                Event::Error { category, message },
                &dropped_for_init,
            );
        }
    });
    // `app_config` is still held (referenced by `config_path`
    // for the SaveConfig handler). Don't drop yet.

    let mut next_batch_id: BatchId = 0;
    // Busy gate: one batch in flight at a time (§3.4 / §3.8.1).
    //
    // A plain `bool` in the actor — no lock, no atomic. The batch
    // itself runs on a worker thread (§3.10.1.1), so the actor is the
    // only reader and the only writer.
    let mut batch_running = false;
    // Per-batch cancel token. Reset between batches. Shared with the
    // worker: the actor sets it on `Command::Cancel`, the worker polls
    // it (between files, and inside the ffmpeg progress loop).
    let mut cancel_token = CancelToken::new();
    // "This batch is done" signals. The worker sends one when it
    // finishes; the actor reopens the busy gate when it reads one.
    let (done_tx, done_rx) = bounded::<BatchDone>(1);
    // The authoritative capability: this machine's NVENC capability as
    // the session knows it. Each batch worker gets a snapshot and
    // reports its changes back (the done signal carries them).
    let mut capability = Capability::default();

    loop {
        // Wait for the next command OR for a worker to report "done".
        // The done branch re-opens the gate and goes back to waiting.
        let cmd = select! {
            recv(cmd_rx) -> cmd => cmd,
            recv(done_rx) -> done => {
                if let Ok(BatchDone { updates }) = done {
                    apply_verdict_updates(capability.verdicts_mut(), updates);
                    batch_running = false;
                }
                continue;
            }
        };
        let Ok(cmd) = cmd else {
            // The UI dropped the command sender: nobody left to
            // serve. Leave the loop. A worker that is still running
            // finishes on its own — its events go nowhere, but nothing
            // here waits for them.
            break;
        };

        // The busy gate covers StartBatch / DryRun / Analyze (§3.4:
        // one batch in flight at a time). It is checked in one place
        // for all three.
        //
        // The drain comes first: a worker can report "done" while the
        // actor was handling a command, and a batch that has finished
        // must not be mistaken for a running one.
        if matches!(
            &cmd,
            Command::StartBatch { .. } | Command::DryRun { .. } | Command::Analyze { .. }
        ) {
            let (finished, updates) = drain_batch_done(&done_rx);
            apply_verdict_updates(capability.verdicts_mut(), updates);
            if finished {
                batch_running = false;
            }
            if batch_running {
                // Benign rejection: the command is dropped, the running
                // batch continues. `Event::Error` would be fatal-only
                // and the UI would finalize the running batch.
                send_evt_lossy(
                    &evt_tx,
                    Event::CommandRejected {
                        reason: "another batch is already running".into(),
                    },
                    &dropped_counter,
                );
                continue;
            }
        }

        match cmd {
            Command::StartBatch { params, files } => {
                // Review #120: delegate per-file iteration to
                // TaskManager::run. The processor closure owns the UI
                // protocol (FileStarted / FileCompleted / Progress /
                // Error); TaskManager owns batch/task scaffold logs +
                // partial_output::Guard registration.
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

                // One cache snapshot per batch (a batch is a unit —
                // the encoder must not change under it). The cache is
                // resolved *in the worker*: the one-time init may
                // still be running, and waiting for it must not block
                // the actor.
                //
                // Review #122: extension still hoisted (unused now — kept for
                // the hoist comment / future use).
                let _ext = params.format.default_extension();
                let tasks: Vec<TranscodeTask> = files
                    .iter()
                    .map(|file| {
                        // v1.2 follow-up #3: route through
                        // output_path_for for the `_encoded`
                        // collision guard.
                        TranscodeTask {
                            input: file.clone(),
                            output: Some(crate::utils::fs::output_path_for(
                                file.as_path(),
                                params.format,
                            )),
                        }
                    })
                    .collect();

                let batch_path = crate::pipeline::batch::batch_target_path_for_files(&files);

                let cache_cell_for_worker = Arc::clone(&cache_cell);
                let params_for_worker = params.clone();
                let cancel_for_worker = cancel_token.clone();
                let evt_tx_for_worker = evt_tx.clone();
                let dropped_for_worker = DropCounter::default();
                // The worker gets a snapshot of the authoritative
                // capability; whatever it changes is reported back
                // through the update channel.
                let capability_for_worker = capability.clone();
                let file_encoder_for_worker = Arc::clone(&file_encoder);
                let make_processor = move |update_tx: Sender<(String, bool)>| {
                    // The context is built here, in the worker: the
                    // cache may still be initializing, and waiting for
                    // it must not block the actor (§3.10.1.1 rule 5).
                    let slot = take_cache(&cache_cell_for_worker, CACHE_WAIT_TIMEOUT).ok_or((
                        ErrorCategory::ConfigError,
                        "encoder cache init timed out".into(),
                    ))?;
                    let cache = cache_from_slot(slot)?;
                    let batch_ctx =
                        BatchContext::new(params_for_worker, cache, capability_for_worker);
                    Ok(make_encode_processor(
                        batch_ctx,
                        cancel_for_worker,
                        evt_tx_for_worker,
                        dropped_for_worker,
                        update_tx,
                        file_encoder_for_worker,
                    ))
                };

                // The batch runs on its own thread: the actor stays
                // responsive, so `Command::Cancel` reaches the
                // in-flight ffmpeg.
                batch_running = true;
                spawn_task_batch(
                    tasks,
                    "encoding",
                    batch_path,
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
                let cache_cell_for_worker = Arc::clone(&cache_cell);
                let evt_tx_for_worker = evt_tx.clone();
                let dropped_for_worker = DropCounter::default();
                // A dry run never encodes, so it cannot change the
                // capability -- it only reads it.
                let capability_for_worker = capability.clone();
                batch_running = true;
                spawn_batch_job(
                    move || {
                        // The cache is resolved here, in the worker: the
                        // one-time init may still be running, and
                        // waiting for it must not block the actor.
                        let slot = take_cache(&cache_cell_for_worker, CACHE_WAIT_TIMEOUT);
                        let cache = match slot {
                            Some(slot) => match cache_from_slot(slot) {
                                Ok(cache) => cache,
                                Err((category, message)) => {
                                    send_evt_lossy(
                                        &evt_tx_for_worker,
                                        Event::Error { category, message },
                                        &dropped_for_worker,
                                    );
                                    return;
                                }
                            },
                            None => {
                                send_evt_lossy(
                                    &evt_tx_for_worker,
                                    Event::Error {
                                        category: ErrorCategory::ConfigError,
                                        message: "encoder cache init timed out".into(),
                                    },
                                    &dropped_for_worker,
                                );
                                return;
                            }
                        };
                        let batch_ctx = BatchContext::new(params, cache, capability_for_worker);
                        let records =
                            crate::pipeline::batch::dry_run_batch(&batch_ctx, files.iter());
                        send_evt_lossy(
                            &evt_tx_for_worker,
                            Event::DryRunFinished { records },
                            &dropped_for_worker,
                        );
                    },
                    done_tx.clone(),
                    None,
                );
            }

            Command::Cancel => {
                // Marks the per-batch token. The worker polls it: the
                // in-flight ffmpeg is killed (and its partial output
                // removed), and the batch stops at the next file
                // boundary.
                cancel_token.cancel();
            }

            Command::LoadHistory { limit, after } => {
                let records = load_history(data_dir.clone(), limit, after);
                send_evt_lossy(&evt_tx, Event::HistoryLoaded { records }, &dropped_counter);
            }

            Command::SaveConfig { config } => {
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
                }
                // The `nv` switch may have changed -- and that is all
                // that changes: the capability is a fact about the
                // machine, so the cache stays as it is. The next batch
                // picks the new `nv` up from the run parameters (design
                // doc §3.10.5).
            }

            Command::Detect { force } => {
                // Same path as the CLI's `rencoder detect`: resolve
                // through the on-disk record, store a fresh probe, and
                // report what this machine can actually run.
                //
                // The probe spawns ffmpeg subprocesses (up to ~1.5s),
                // so it runs on its own thread: doing it here would
                // stall every other command -- `Command::Cancel`
                // included -- for the duration.
                let evt_tx_detect = evt_tx.clone();
                let dropped_detect = DropCounter::default();
                let cache_path = nvenc_cache_path.clone();
                let detect_runner = Arc::clone(&detect_runner);
                // A fresh probe may find different encoders usable than
                // the cache knows about -- so the cache follows the
                // capability (not the other way round).
                let cache_cell_for_detect = Arc::clone(&cache_cell);
                std::thread::Builder::new()
                    .name("renencoder-detect".into())
                    .spawn(move || {
                        match crate::encoder::nvenc::resolve_and_store_with(
                            cache_path.as_deref(),
                            force,
                            move |args| detect_runner(args),
                        ) {
                            Ok(resolution) => {
                                replace_encoder_cache(&cache_cell_for_detect, &resolution);
                                send_evt_lossy(
                                    &evt_tx_detect,
                                    Event::NvencCapability(resolution.capability()),
                                    &dropped_detect,
                                );
                            }
                            Err(e) => {
                                send_evt_lossy(
                                    &evt_tx_detect,
                                    Event::Error {
                                        category: ErrorCategory::ProbeFailed,
                                        message: format!("NVENC detection failed: {e}"),
                                    },
                                    &dropped_detect,
                                );
                            }
                        }
                    })
                    .expect("spawn detect thread");
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
                send_evt_lossy(&evt_tx, Event::AnalyzeStarted { total }, &dropped_counter);

                let evt_tx_for_worker = evt_tx.clone();
                let dropped_for_worker = DropCounter::default();
                batch_running = true;
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

                        send_evt_lossy(
                            &evt_tx_for_worker,
                            Event::AnalyzeFinished {
                                records,
                                failed_count,
                            },
                            &dropped_for_worker,
                        );
                    },
                    done_tx.clone(),
                    None,
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
    ) -> (Sender<Command>, Receiver<Event>)
    where
        F: Fn(
                &TranscodeTask,
                &BatchContext,
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
    ) -> (Sender<Command>, Receiver<Event>)
    where
        F: Fn(
                &TranscodeTask,
                &BatchContext,
                &CancelToken,
                &mut dyn FnMut(FfmpegProgress),
            ) -> Result<EncodingRecord, EncodeError>
            + Send
            + Sync
            + 'static,
    {
        let (cmd_tx, cmd_rx) = unbounded::<Command>();
        let (evt_tx, evt_rx) = unbounded::<Event>();

        // The cache is fed through the same channel the production
        // uses: `CacheInit`'s fields are public, so the test can
        // pre-populate it without spawning ffmpeg.
        let (cache_tx, cache_rx) = std::sync::mpsc::channel();
        cache_tx
            .send(crate::pipeline::cache::CacheInit {
                cache: Ok(EncoderCache {
                    encoders_output: String::new(),
                }),
                capability,
            })
            .expect("send the pre-populated cache");

        let config_path = data_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("saved-config.toml");
        let nvenc_cache_path = data_dir.clone().map(|d| d.join("nvenc-capability.json"));

        let env = ActorEnv {
            app_config: Arc::new(AppConfig::default()),
            config_path,
            data_dir,
            nvenc_cache_path,
            file_encoder: Arc::new(MockFileEncoder { f: mock_fn }),
            // The NVENC probe never runs in a test: the fake runner
            // answers "no encoders", so nothing is probed.
            detect_runner: Arc::new(|_args| Ok(String::new())),
        };

        std::thread::spawn(move || {
            run_actor(cmd_rx, evt_tx, env, cache_rx);
        });

        (cmd_tx, evt_rx)
    }

    /// The test double for "encode one file".
    struct MockFileEncoder<F> {
        f: F,
    }

    impl<F> FileEncoder for MockFileEncoder<F>
    where
        F: Fn(
                &TranscodeTask,
                &BatchContext,
                &CancelToken,
                &mut dyn FnMut(FfmpegProgress),
            ) -> Result<EncodingRecord, EncodeError>
            + Send
            + Sync
            + 'static,
    {
        fn encode(
            &self,
            task: &TranscodeTask,
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
                &TranscodeTask,
                &BatchContext,
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
        let f = move |task: &TranscodeTask,
                      _: &BatchContext,
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
        &TranscodeTask,
        &BatchContext,
        &CancelToken,
        &mut dyn FnMut(FfmpegProgress),
    ) -> Result<EncodingRecord, EncodeError>
           + Send
           + Sync
           + 'static {
        fn f(
            task: &TranscodeTask,
            _: &BatchContext,
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
        let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
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
        let (cmd_tx, evt_rx) = run_test_controller(make_progress_mock(), None);
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
        let files: Vec<crate::types::InputPath> = vec![input("/a.mp4")];
        cmd_tx
            .send(Command::StartBatch {
                params: EncodeParams::default(),
                files: files.clone(),
            })
            .unwrap();
        cmd_tx
            .send(Command::StartBatch {
                params: EncodeParams::default(),
                files,
            })
            .unwrap();

        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
        assert!(
            events.iter().any(|e| matches!(
                e,
                Event::CommandRejected { reason } if reason.contains("another batch")
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
        let files: Vec<crate::types::InputPath> =
            (0..20).map(|i| input(&format!("/x/{i}.mp4"))).collect();
        cmd_tx
            .send(Command::StartBatch {
                params: EncodeParams::default(),
                files,
            })
            .unwrap();

        // Wait until the worker really started the batch.
        loop {
            match evt_rx.recv_timeout(std::time::Duration::from_secs(2)) {
                Ok(Event::FileStarted { .. }) => break,
                Ok(_) => continue,
                Err(e) => panic!("no FileStarted event: {e}"),
            }
        }

        cmd_tx.send(Command::Cancel).unwrap();

        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
        let last = events.last().expect("expected a terminal event");
        match last {
            Event::BatchCancelled { completed, total } => {
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
            run_test_controller(|task, _, _, _| Ok(fake_record(&task.input)), None);
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

        let (cmd_tx, evt_rx) = run_test_controller(
            |task, _, _, _| Ok(fake_record(&task.input)),
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
            .send(Command::SaveConfig {
                config: Box::new(edited),
            })
            .unwrap();
        drop(cmd_tx);

        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ConfigSaved { .. })),
            "expected a ConfigSaved event, got {events:?}"
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
        cmd_tx.send(Command::Detect { force: true }).unwrap();
        drop(cmd_tx);

        let events = collect_events(&evt_rx, std::time::Duration::from_secs(2));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::NvencCapability(_))),
            "expected a NvencCapability event, got {events:?}"
        );
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
        let (cmd_tx, evt_rx) = run_test_controller(make_succeeding_mock().0, None);
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

        let (cmd_tx, evt_rx) = run_test_controller(
            |task, _, _, _| Ok(fake_record(&task.input)),
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

        let (cmd_tx, evt_rx) = run_test_controller(
            |task, _, _, _| Ok(fake_record(&task.input)),
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
        let (cmd_tx, evt_rx) = run_test_controller(
            |_, _, _, _| {
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
        // Review #120: failed records ARE included in the final
        // BatchCompleted list (the per-file failure is now mapped to
        // Ok(Failed record) inside the processor closure, not to
        // an aborted batch).
        assert!(matches!(
            events.last().unwrap(),
            Event::BatchCompleted { records } if records.len() == 1
                && matches!(records[0].outcome, EncodingOutcome::Failed)
        ));
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
            move |task: &TranscodeTask,
                  _ctx: &BatchContext,
                  cancel: &CancelToken,
                  on_progress: &mut dyn FnMut(FfmpegProgress)| {
                let output = task
                    .output
                    .as_ref()
                    .expect("test: task has output")
                    .to_string_lossy()
                    .into_owned();
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
            .send(Command::StartBatch {
                params: EncodeParams::default(),
                files,
            })
            .unwrap();
        drop(cmd_tx);

        let _events = collect_events(&evt_rx, std::time::Duration::from_secs(5));

        assert!(
            !partial_check_b.exists(),
            "partial output {} still on disk after cancel; cleanup did NOT run in the full actor flow",
            partial_check_b.display()
        );
        let _ = std::fs::remove_dir_all(&tmp);
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

        let capability = Capability::from_check(
            Vec::new(),
            Some(chrono::Utc::now() - chrono::Duration::days(40)),
            true,
        );

        let (_cmd_tx, evt_rx) = run_test_controller_with_capability(
            make_succeeding_mock().0,
            Some(dir.clone()),
            Some(capability),
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
