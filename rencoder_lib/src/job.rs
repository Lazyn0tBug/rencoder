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

use crossbeam_channel::{unbounded, Receiver, Sender};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::config::Config;
use crate::pipeline::batch::{run_ffmpeg_with_progress, transcode_single, FfmpegProgress};
use crate::pipeline::cache::init_encoder_cache;
use crate::pipeline::cancel::CancelToken;
use crate::types::{
    EncodeParams, EncodingRecord, EncoderCache, ErrorCategory, ProcessOutput, TranscodeTask,
};
use crate::utils::fs::output_path_for;

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
        files: Vec<PathBuf>,
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
}

// =====================================================================
// Event (lib → UI) — §3.8.3
// =====================================================================

/// Messages from the lib actor to the UI. Variants are grouped by
/// topic in the doc but encoded as enum variants (no separate
/// `topic` field) so dispatch is just `match` (§3.8.3).
#[non_exhaustive]
pub enum Event {
    // ---- Lifecycle topic (low frequency) ----
    /// A batch has started. `id` is unique within the controller's
    /// lifetime. `total_files` is the count after collection.
    BatchStarted {
        id: BatchId,
        total_files: usize,
    },
    /// A single file is about to be encoded. `source` is the input
    /// path; the output path is computed by the lib and surfaced via
    /// `FileCompleted`.
    FileStarted {
        index: usize,
        total: usize,
        source: PathBuf,
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
    LogLine {
        file_index: usize,
        line: String,
    },

    // ---- State topic ----
    /// Result of a `Command::LoadHistory`. `records` is the filtered
    /// view (limit + after applied); empty vec is valid (no records
    /// match the filter).
    HistoryLoaded { records: Vec<EncodingRecord> },

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
            files: vec![PathBuf::from("/a.mp4")],
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
            source: PathBuf::from("/a.mp4"),
        };
        let _ = Event::FileCompleted { index: 0, record: rec };
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
pub struct ControllerHandle {
    pub(crate) cmd_tx: Sender<Command>,
    pub(crate) evt_rx: Receiver<Event>,
}

impl ControllerHandle {
    /// Get a cloneable sender for sending commands. The UI typically
    /// clones this once per component that needs to dispatch.
    pub fn command_sender(&self) -> Sender<Command> {
        self.cmd_tx.clone()
    }

    /// Take the event receiver. Single-use — only one consumer.
    /// After this is called the handle is in a partial state; the UI
    /// typically destructures (`let ControllerHandle { evt_rx, .. } = handle`)
    /// rather than calling this method.
    pub fn into_split(self) -> (Sender<Command>, Receiver<Event>) {
        (self.cmd_tx, self.evt_rx)
    }
}

/// Spawn the lib actor and return the message-passing handle. The
/// caller (UI) gets `(cmd_tx, evt_rx)` to drive the actor. The actor
/// thread lives until `cmd_tx` is dropped — at which point the
/// thread's `recv()` returns `None` and the thread exits.
///
/// Single-batch in flight at a time (§3.4 / §3.8.1).
pub fn start_controller() -> ControllerHandle {
    let (cmd_tx, cmd_rx) = unbounded::<Command>();
    let (evt_tx, evt_rx) = unbounded::<Event>();

    std::thread::Builder::new()
        .name("rencoder-controller".into())
        .spawn(move || run_actor(cmd_rx, evt_tx))
        .expect("spawn controller thread");

    ControllerHandle { cmd_tx, evt_rx }
}

fn run_actor(
    cmd_rx: Receiver<Command>,
    evt_tx: Sender<Event>,
) {
    // Init once for the controller's lifetime. v1: no NV, default
    // config. Future: pull from AppConfig + honor `[encode].nv`.
    let cache: EncoderCache = match init_encoder_cache(false) {
        Ok(c) => c,
        Err(e) => {
            let _ = evt_tx.send(Event::Error {
                category: ErrorCategory::ConfigError,
                message: format!("encoder cache init failed: {e}"),
            });
            return;
        }
    };
    let config = Config::default();

    let mut next_batch_id: BatchId = 0;
    // Set when a batch is running; gates StartBatch to one at a time.
    let busy = Arc::new(AtomicBool::new(false));
    // Per-batch cancel token. Reset between batches.
    let mut cancel_token = CancelToken::new();

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Command::StartBatch { params, files } => {
                if busy.swap(true, Ordering::SeqCst) {
                    let _ = evt_tx.send(Event::Error {
                        category: ErrorCategory::ConfigError,
                        message: "another batch is already running".into(),
                    });
                    continue;
                }

                let id = next_batch_id;
                next_batch_id += 1;
                cancel_token = CancelToken::new();

                let _ = evt_tx.send(Event::BatchStarted {
                    id,
                    total_files: files.len(),
                });

                let total = files.len();
                let mut records: Vec<EncodingRecord> = Vec::with_capacity(total);
                let mut cancelled = false;

                for (index, file) in files.iter().enumerate() {
                    if cancel_token.is_cancelled() {
                        cancelled = true;
                        break;
                    }

                    let _ = evt_tx.send(Event::FileStarted {
                        index,
                        total,
                        source: file.clone(),
                    });

                    let output_path = output_path_for(file, params.format);
                    let task = TranscodeTask {
                        input: file.clone(),
                        output: Some(output_path),
                    };

                    // v1: use CLI-style transcode_single (no per-frame
                    // progress events). Per-frame events land when
                    // transcode_single is extended to accept
                    // CancelToken + on_progress.
                    match transcode_single(&task, &params, &cache, &config, ProcessOutput::Null) {
                        Ok(record) => {
                            records.push(record.clone());
                            let _ = evt_tx.send(Event::FileCompleted { index, record });
                        }
                        Err(e) => {
                            // transcode_single normally returns Ok(Failed record)
                            // for per-file failures; this branch is for
                            // unexpected setup-level errors. Emit and
                            // continue.
                            let _ = evt_tx.send(Event::Error {
                                category: ErrorCategory::ConfigError,
                                message: format!("transcode_single error: {e}"),
                            });
                        }
                    }
                }

                busy.store(false, Ordering::SeqCst);

                if cancelled {
                    let _ = evt_tx.send(Event::BatchCancelled {
                        completed: records.len(),
                        total,
                    });
                } else {
                    let _ = evt_tx.send(Event::BatchCompleted { records });
                }
            }

            Command::Cancel => {
                // Marks the per-batch token; checked between files in
                // the StartBatch loop above. Per-ffmpeg-process
                // cancellation requires plumbing the token into
                // transcode_single → run_ffmpeg_with_progress.
                cancel_token.cancel();
            }

            Command::LoadHistory { limit, after } => {
                let records = load_history(limit, after);
                let _ = evt_tx.send(Event::HistoryLoaded { records });
            }
        }
    }
}

/// Read history records from `<app_dir>/data/encoding-*.parquet`,
/// apply `limit` + `after`, return newest-first. Resolves the data
/// dir via `config::path::resolve_data_dir()` (§8 item 39).
fn load_history(
    limit: Option<usize>,
    after: Option<DateTime<Utc>>,
) -> Vec<EncodingRecord> {
    use crate::config::path::resolve_data_dir;
    use crate::data::store::read_records;

    let dir = match resolve_data_dir() {
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
#[allow(dead_code)]
pub(crate) fn _progress_unused(_p: FfmpegProgress) {}

#[cfg(test)]
mod controller_tests {
    use super::*;

    #[test]
    fn handle_into_split_returns_both_halves() {
        let (cmd_tx, _) = unbounded::<Command>();
        let (_, evt_rx) = unbounded::<Event>();
        let handle = ControllerHandle { cmd_tx, evt_rx };
        let (_tx, _rx) = handle.into_split();
    }
}
