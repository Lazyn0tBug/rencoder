//! UI state + Msg type + reducer (U21 §3.8.7 "everything is msg").
//!
//! The view layer is purely a function of state; state changes
//! only via `reduce(state, msg, cmd_tx)` which also dispatches
//! Commands back to the lib actor as needed.
//!
//! **Workflow state machine** (§3.8.7.1):
//!
//! ```text
//! [None]  --UserClickedStart-->  [Analyzing]
//!                                     |
//!                                     v
//!                              [AwaitingConfirm]
//!                                     |
//!                                     +-- UserClickedStart (dispatches Command::StartBatch)
//!                                     v
//!                                [Encoding]
//!                                     |
//!                  +-- BatchCompleted ---+
//!                  +-- BatchCancelled ---+--> [Done] (moves to task_history)
//!                  +-- Error (Encoding) -+
//! ```
//!
//! v1 invariant: TaskQueue capacity = 1. Gating unit is task
//! existence (`tasks.active.is_some()`), not batch id (batch_id
//! is a lib-side mapping field that doesn't participate in
//! gating decisions).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, Utc};
use crossbeam_channel::Sender;

use rencoder_lib::job::{Command, EncodingRecord, Event};
use rencoder_lib::types::EncodingOutcome;

/// Cap on the stderr log ring buffer. Per §3.8.6 + §8 item 30.
const LOG_RING_CAP: usize = 1000;

/// Lifecycle phase of a task. Drives the Task page header badge,
/// the contextual button label, and the dir picker's enabled state
/// (§3.8.7.1 state machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchPhase {
    /// `Command::DryRun` dispatched; awaiting `Event::DryRunFinished`.
    /// Dir picker disabled. Cancel button shown (best-effort).
    Analyzing,
    /// DryRun done; user must explicitly confirm before encoding
    /// begins. Start button label flips to "encode N files".
    /// Dir picker disabled.
    AwaitingConfirm,
    /// `Command::StartBatch` dispatched; awaiting `BatchCompleted`
    /// / `BatchCancelled` / `Event::Error`. Progress bar active.
    /// Dir picker disabled. Cancel button shown.
    Encoding,
    /// Final state after the batch lifecycle ends. Dir picker
    /// re-enables. Start button re-arms (clicking it transitions
    /// back to `Analyzing` for a new cycle).
    Done,
}

/// 4 page tabs (per §3.8.7.2). `Setup` is the initial value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    Setup,
    Task,
    History,
    Log,
}

/// UI-side task id — timestamp-based `yyyymmddhhmmss` (per spec).
/// Queue and history share the same id space; the id is preserved
/// across active -> history moves (no re-key).
///
/// Format: `chrono::Local::now().format("%Y%m%d%H%M%S")` -> 14
/// chars. Same-second collisions get a `-N` suffix (allocated by
/// `TaskQueue::next_task_id`).
///
/// Session-unique only. Cross-session uniqueness is handled by the
/// lib's batch_id + Parquet reload (per §3.8.7.4 limitations).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskId(String);

impl TaskId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Top-level UI state — single source of truth for all views.
#[derive(Debug, Default)]
pub struct AppState {
    /// Task queue (v1: capacity = 1). All tasks share the lib
    /// actor via busy-gate. Guard on `tasks.active.is_some()` —
    /// task is the gating unit, not batch id.
    pub tasks: TaskQueue,
    /// Session-completed tasks, newest-first. Inserted on
    /// BatchCompleted / BatchCancelled / Encoding-phase Error.
    /// `task_history` is session UX cache; the primary store is
    /// Parquet (`encoding-*.parquet`, written by lib on every
    /// `BatchCompleted`). v1.1 reload via `Command::LoadHistory`
    /// gives cross-session `state.history` (per-file records).
    pub task_history: Vec<TaskInfo>,
    /// Active page tab (§3.8.7.2). Default `Setup`. Tab clicks
    /// dispatch `Msg::UserClickedTab(Page)` -> reducer writes
    /// this field (testable).
    pub active_page: Page,
    /// Currently-viewed task on the `[TASK]` page. v1: always
    /// `tasks.active.task_id` when active is Some; v1.1: user
    /// can switch between active and history entries.
    pub viewing_task: Option<TaskId>,
    /// per-file progress keyed by file_index. v1: single active
    /// task's progress. v1.1: `HashMap<TaskId, HashMap<...>>`.
    pub progress: HashMap<usize, ProgressInfo>,
    /// stderr log ring buffer (newest last).
    pub log: VecDeque<LogLine>,
    /// History loaded via `Command::LoadHistory` from Parquet
    /// (cross-session records). Different concept from
    /// `task_history` (this-session UX cache).
    pub history: Vec<EncodingRecord>,
    /// Currently selected input directory (set via DirPicker).
    pub input_dir: Option<PathBuf>,
    /// Last fatal error (any source). UI renders it as the global
    /// status badge.
    pub last_error: Option<String>,
}

/// Task queue. v1: capacity = 1, `pending` always empty. v1.1
/// lifts capacity to N and uses `pending` for FIFO queuing.
///
/// `current_second_count` tracks same-second TaskId collisions
/// (per spec: `-N` suffix when `TaskId::new` is called twice
/// in the same second).
#[derive(Debug, Default)]
pub struct TaskQueue {
    /// v1: 1. v1.1: N (configurable via `RENENCODER_MAX_CONCURRENT`
    /// env var or AppConfig field).
    pub capacity: usize,
    /// Currently-active task. v1: at most 1.
    pub active: Option<TaskInfo>,
    /// Pending tasks (v1: always empty). v1.1: FIFO.
    pub pending: Vec<TaskInfo>,
    /// Same-second TaskId collision counter. Key: base timestamp
    /// string (`yyyymmddhhmmss`); Value: next available N suffix.
    /// Monotonically incremented even on aborted tasks (so
    /// TaskIds stay unique within a second).
    current_second_count: HashMap<String, usize>,
}

impl TaskQueue {
    #[allow(dead_code)] // used by tests; future v1.1 entry path
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            active: None,
            pending: Vec::new(),
            current_second_count: HashMap::new(),
        }
    }

    /// Allocate a fresh TaskId for the current second. First call
    /// in a given second returns the bare timestamp; subsequent
    /// calls return `-N` suffixed forms.
    pub fn next_task_id(&mut self) -> TaskId {
        let base = Local::now().format("%Y%m%d%H%M%S").to_string();
        let count = self.current_second_count.entry(base.clone()).or_insert(0);
        let id = if *count == 0 {
            TaskId(base)
        } else {
            TaskId(format!("{}-{}", base, count))
        };
        *count += 1;
        id
    }

    /// True iff a new task can be started (capacity has room).
    /// v1 with capacity=1: returns true only when `active` is None.
    pub fn has_room(&self) -> bool {
        let total = (self.active.is_some() as usize) + self.pending.len();
        total < self.capacity
    }
}

/// Single task's metadata. v1: always appears in
/// `TaskQueue.active` (capacity=1); v1.1: may also appear in
/// `pending`.
///
/// `task_id` is the UI-side gating identity; `batch_id` is the
/// lib-side mapping (per `Event::BatchStarted.id`) and is `None`
/// until the lib accepts the task.
#[allow(dead_code)] // populated by reducer; some fields not yet read by UI components
#[derive(Debug, Clone)]
pub struct TaskInfo {
    /// UI-side id, allocated by `TaskQueue::next_task_id`.
    pub task_id: TaskId,
    /// Lib-side batch id mapping. `None` until `Event::BatchStarted`.
    pub batch_id: Option<u64>,
    /// Lifecycle phase.
    pub phase: BatchPhase,
    /// Input directory captured at Start time.
    pub dir: PathBuf,
    /// Total file count discovered by DryRun.
    pub total_files: usize,
    /// Files that would actually be encoded (outcome =
    /// `Succeeded`). Set on `Event::DryRunFinished`.
    pub to_encode: usize,
    /// Files that would NOT be encoded (outcome = `Skipped(...)`
    /// or `Failed`). Set on `Event::DryRunFinished`.
    pub to_skip: usize,
    /// Files encoded so far during `Encoding`. Updated on each
    /// `Event::FileCompleted`.
    pub encoded: usize,
    /// Set true once Cancel lands; reducer ignores subsequent
    /// `Event::Progress` / `FileCompleted` until `BatchCancelled`
    /// or new `BatchStarted`.
    pub cancelled: bool,
    /// Set true once `BatchCompleted` / `BatchCancelled` /
    /// Encoding-phase `Event::Error` fires. Combined with
    /// `phase = Done` for re-arm.
    pub finished: bool,
    /// Records accumulated during `Encoding`. `FileCompleted`
    /// pushes one per file; `BatchCompleted` replaces with the
    /// lib's authoritative vec.
    pub records: Vec<EncodingRecord>,
    /// Records from the DryRun phase. Kept until the next task
    /// lifecycle starts.
    pub dry_run_records: Vec<EncodingRecord>,
}

#[allow(dead_code)] // fields populated by lib, not all read yet by UI
#[derive(Debug, Clone)]
pub struct ProgressInfo {
    pub frame: u64,
    pub fps: f64,
    pub speed: f64,
    pub time_secs: f64,
    pub eta_secs: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub file_index: usize,
    pub line: String,
    pub at: DateTime<Utc>,
}

/// UI messages. Translating user gestures into Commands happens
/// in the reducer (`User*` variants); translating lib Events into
/// state changes happens too (the `Lib(Event)` wrapper).
#[allow(clippy::large_enum_variant)] // same rationale as Event — single mpsc emission
#[derive(Debug)]
pub enum Msg {
    /// User picked a directory. Updates `state.input_dir`.
    /// Rejected if a task is in active phase (defense-in-depth;
    /// the picker is also disabled in the UI).
    UserSelectedDir(PathBuf),
    /// **Polymorphic** Start button:
    ///   - phase None / Done: dispatch `Command::DryRun` + create task
    ///   - phase AwaitingConfirm: dispatch `Command::StartBatch`
    ///   - phase Analyzing / Encoding: rejected (busy-gate)
    UserClickedStart,
    /// Phase-aware Cancel:
    ///   - Analyzing / Encoding: set `cancelled = true` + dispatch `Command::Cancel`
    ///   - AwaitingConfirm: clear `tasks.active` (no command)
    ///   - None / Done: ignored
    UserClickedCancel,
    /// Reload history records from disk (Parquet). Manual
    /// dispatch via Refresh button. v1.1 auto-dispatch on app
    /// start deferred.
    #[allow(dead_code)]
    UserClickedLoadHistory,
    /// Save the loaded AppConfig back to its original path.
    /// Wired by Save button (v1: removed — see §3.8.7.2; kept
    /// for v1.1).
    #[allow(dead_code)]
    UserClickedSaveSettings,
    /// User clicked a top-bar tab. Reducer writes
    /// `state.active_page`. No lib command.
    UserClickedTab(Page),
    /// Wrapper for lib -> UI events. All lib events flow
    /// through here; reducer routes them in `apply_event`.
    Lib(Event),
}

/// Reduce a message into a state change; emit Commands to the lib
/// actor as needed.
///
/// **Phase-aware `UserClickedStart`** (§3.8.7.1):
///   - `None` / `Done` -> build params, collect files, dispatch
///     `Command::DryRun`, optimistically transition to `Analyzing`
///   - `AwaitingConfirm` -> dispatch `Command::StartBatch`,
///     transition phase to `Encoding`
///   - `Analyzing` / `Encoding` -> set `last_error = "task in
///     progress"`, return early (busy-gate)
///
/// **Phase-aware `UserClickedCancel`**:
///   - `Analyzing` / `Encoding`: set `cancelled = true`, dispatch
///     `Command::Cancel`. The lib respects cancel_token only
///     during encoding (Analyze is best-effort).
///   - `AwaitingConfirm`: clear `tasks.active` (no command)
///   - `None` / `Done`: ignored
pub fn reduce(
    state: &mut AppState,
    msg: Msg,
    cmd_tx: &Sender<Command>,
    config: &rencoder_lib::config::app::AppConfig,
) {
    match msg {
        Msg::UserSelectedDir(path) => {
            if let Some(t) = &state.tasks.active {
                if !matches!(t.phase, BatchPhase::Done) {
                    return;
                }
            }
            state.input_dir = Some(path);
        }
        Msg::UserClickedStart => {
            let phase = state.tasks.active.as_ref().map(|t| t.phase);
            match phase {
                None | Some(BatchPhase::Done) => {
                    // Capacity check (v1: capacity=1, only fires
                    // when active is None, so always true here).
                    if !state.tasks.has_room() {
                        state.last_error = Some("task queue full".into());
                        return;
                    }
                    if state.input_dir.is_none() {
                        state.last_error = Some("no input dir selected".into());
                        return;
                    }
                    let params = build_encode_params_from_config(config);
                    let Some(files) = collect_input_files(state, config) else {
                        state.last_error = Some("no input dir selected".into());
                        return;
                    };
                    if files.is_empty() {
                        let extensions = config
                            .encode
                            .as_ref()
                            .and_then(|e| e.extensions.clone())
                            .unwrap_or_default();
                        let detail = if extensions.is_empty() {
                            format!(
                                "no files in {}",
                                state.input_dir.as_ref().unwrap().display()
                            )
                        } else {
                            format!(
                                "no files in {} matching extensions {:?}",
                                state.input_dir.as_ref().unwrap().display(),
                                extensions
                            )
                        };
                        state.last_error = Some(detail);
                        return;
                    }
                    state.last_error = None;
                    state.progress.clear();
                    state.log.clear();

                    let task_id = state.tasks.next_task_id();
                    let dir = state.input_dir.clone().unwrap();
                    let total_files = files.len();
                    state.tasks.active = Some(TaskInfo {
                        task_id: task_id.clone(),
                        batch_id: None,
                        phase: BatchPhase::Analyzing,
                        dir,
                        total_files,
                        to_encode: 0,
                        to_skip: 0,
                        encoded: 0,
                        cancelled: false,
                        finished: false,
                        records: Vec::new(),
                        dry_run_records: Vec::new(),
                    });
                    state.viewing_task = Some(task_id);
                    let _ = cmd_tx.send(Command::DryRun { params, files });
                }
                Some(BatchPhase::AwaitingConfirm) => {
                    let params = build_encode_params_from_config(config);
                    let Some(files) = collect_input_files(state, config) else {
                        return;
                    };
                    if let Some(t) = state.tasks.active.as_mut() {
                        t.phase = BatchPhase::Encoding;
                        t.records.clear();
                    }
                    let _ = cmd_tx.send(Command::StartBatch { params, files });
                }
                Some(BatchPhase::Analyzing) | Some(BatchPhase::Encoding) => {
                    state.last_error = Some("task in progress".into());
                }
            }
        }
        Msg::UserClickedCancel => {
            let phase = state.tasks.active.as_ref().map(|t| t.phase);
            match phase {
                Some(BatchPhase::Analyzing) | Some(BatchPhase::Encoding) => {
                    if let Some(t) = state.tasks.active.as_mut() {
                        t.cancelled = true;
                    }
                    let _ = cmd_tx.send(Command::Cancel);
                }
                Some(BatchPhase::AwaitingConfirm) => {
                    state.tasks.active = None;
                }
                None | Some(BatchPhase::Done) => {}
            }
        }
        Msg::UserClickedLoadHistory => {
            let _ = cmd_tx.send(Command::LoadHistory {
                limit: Some(100),
                after: None,
            });
        }
        Msg::UserClickedSaveSettings => {
            let _ = cmd_tx.send(Command::SaveConfig);
        }
        Msg::UserClickedTab(page) => {
            state.active_page = page;
        }
        Msg::Lib(evt) => apply_event(state, evt),
    }
}

fn apply_event(state: &mut AppState, evt: Event) {
    match evt {
        Event::BatchStarted { id, total_files } => {
            // AwaitingConfirm -> Encoding. Record lib's
            // batch_id on the active task (id-as-mapping; not a
            // gating decision per §3.8.7.4).
            if let Some(t) = state.tasks.active.as_mut() {
                t.batch_id = Some(id);
                t.total_files = total_files;
                t.phase = BatchPhase::Encoding;
                t.records.clear();
            }
        }
        Event::FileStarted { .. } => {
            // Could update a "current file" display; v1 lets
            // Progress / FileCompleted drive the view.
        }
        Event::Progress {
            file_index,
            frame,
            fps,
            speed,
            time_secs,
            eta_secs,
        } => {
            if let Some(t) = &state.tasks.active {
                if t.finished || t.cancelled {
                    return;
                }
            }
            state.progress.insert(
                file_index,
                ProgressInfo {
                    frame,
                    fps,
                    speed,
                    time_secs,
                    eta_secs,
                },
            );
        }
        Event::DryRunFinished { records } => {
            // Analyzing -> AwaitingConfirm. Compute
            // to_encode / to_skip from records.
            //
            // EncodingOutcome match scope: DryRun records only
            // carry `Succeeded` / `Skipped(...)` / `Failed` —
            // ReducedFps requires running ffmpeg to detect
            // source-fps > cap, so it cannot appear here (DryRun
            // doesn't invoke ffmpeg). Matching only `Succeeded`
            // is correct.
            //
            // Cancel-during-Analyzing guard: if user clicked
            // Cancel during Analyzing (set `cancelled = true`),
            // DryRunFinished still arrives — discard it.
            let cancelled = state
                .tasks
                .active
                .as_ref()
                .map(|t| t.cancelled)
                .unwrap_or(false);
            if cancelled {
                state.tasks.active = None;
                return;
            }
            if let Some(t) = state.tasks.active.as_mut() {
                t.dry_run_records = records.clone();
                t.to_encode = records
                    .iter()
                    .filter(|r| matches!(r.outcome, EncodingOutcome::Succeeded))
                    .count();
                t.to_skip = t.total_files - t.to_encode;
                t.phase = BatchPhase::AwaitingConfirm;
            }
        }
        Event::AnalyzeStarted { .. }
        | Event::AnalyzeProgress { .. }
        | Event::AnalyzeFinished { .. } => {
            // U22 standalone Analyze no longer used by the UI
            // (Start auto-DryRun subsumes it). Silently drop.
        }
        Event::ConfigSaved { path } => {
            state.last_error = Some(format!("saved to {}", path.display()));
        }
        Event::FileCompleted { record, .. } => {
            if let Some(t) = state.tasks.active.as_mut() {
                t.records.push(record);
                t.encoded += 1;
            }
        }
        Event::BatchCompleted { records } => {
            // Move-to-history (per §3.8.7.1): tasks.active.take()
            // -> set phase = Done -> task_history.insert(0, t).
            let done = state.tasks.active.take();
            if let Some(mut t) = done {
                t.records = records;
                t.finished = true;
                t.phase = BatchPhase::Done;
                state.task_history.insert(0, t);
            }
            state.progress.clear();
        }
        Event::BatchCancelled { completed, total } => {
            let done = state.tasks.active.take();
            if let Some(mut t) = done {
                t.cancelled = true;
                t.finished = true;
                t.phase = BatchPhase::Done;
                state.task_history.insert(0, t);
            }
            state.last_error = Some(format!("cancelled after {completed}/{total}"));
        }
        Event::LogLine { file_index, line } => {
            state.log.push_back(LogLine {
                file_index,
                line,
                at: Utc::now(),
            });
            while state.log.len() > LOG_RING_CAP {
                state.log.pop_front();
            }
        }
        Event::HistoryLoaded { records } => {
            state.history = records;
        }
        Event::Error { message, .. } => {
            state.last_error = Some(message);
            // Per §3.8.7.1: during Encoding, move to history
            // (task did run, even if it failed); during
            // Analyzing, drop without history (task never
            // started); AwaitingConfirm is no-op (lib hasn't
            // been asked to do anything yet).
            let active = state.tasks.active.take();
            match active {
                Some(mut t) if matches!(t.phase, BatchPhase::Encoding) => {
                    t.finished = true;
                    t.phase = BatchPhase::Done;
                    state.task_history.insert(0, t);
                }
                Some(t) if matches!(t.phase, BatchPhase::Analyzing) => {
                    drop(t);
                }
                Some(t) => {
                    state.tasks.active = Some(t);
                }
                None => {}
            }
        }
        // #[non_exhaustive] future variants — silently drop.
        _ => {}
    }
}

/// Build an `EncodeParams` from the loaded `AppConfig`. This is
/// a thin wrapper around the canonical helper in the lib
/// (`rencoder_lib::types::EncodeParams::from_encode_config`) so
/// there's a single source of truth for "how a TOML `[encode]`
/// block becomes an `EncodeParams`".
fn build_encode_params_from_config(
    config: &rencoder_lib::config::app::AppConfig,
) -> rencoder_lib::types::EncodeParams {
    rencoder_lib::types::EncodeParams::from_encode_config(config.encode.as_ref())
}

/// Collect input files from `state.input_dir`, filtered by the
/// configured extensions from `config.encode.extensions`
/// (empty = all).
///
/// Returns `None` if no input dir is selected; otherwise returns
/// the file list (possibly empty). Caller decides action-specific
/// empty-dir error message.
fn collect_input_files(
    state: &AppState,
    config: &rencoder_lib::config::app::AppConfig,
) -> Option<Vec<PathBuf>> {
    let dir = state.input_dir.as_ref()?;
    let extensions = config
        .encode
        .as_ref()
        .and_then(|e| e.extensions.clone())
        .unwrap_or_default();
    let files: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .filter(|p| extension_matches(p, &extensions))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(files)
}

/// Check whether a path's extension matches any of the user's
/// configured extension filters. Empty `extensions` means "all".
/// Match is case-insensitive and ignores the leading dot.
fn extension_matches(path: &Path, extensions: &[String]) -> bool {
    if extensions.is_empty() {
        return true;
    }
    let ext: String = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None => return false,
    };
    extensions.iter().any(|e| {
        let trimmed = e.trim_start_matches('.').to_ascii_lowercase();
        trimmed == ext
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rencoder_lib::config::app::AppConfig;
    use rencoder_lib::types::{EncoderSettings, EncodingOutcome};

    fn default_config() -> AppConfig {
        AppConfig::default()
    }

    fn sample_record(path: &str, outcome: EncodingOutcome) -> EncodingRecord {
        EncodingRecord {
            source_path: path.into(),
            source_size_bytes: 0,
            source_codec: None,
            source_container: None,
            source_width: None,
            source_height: None,
            source_fps: None,
            source_duration_secs: None,
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
            outcome,
            error: None,
        }
    }

    fn sample_skipped(path: &str) -> EncodingRecord {
        use rencoder_lib::types::SkippedReason;
        sample_record(path, EncodingOutcome::Skipped(SkippedReason::SameCodec))
    }

    fn cmd_tx() -> Sender<Command> {
        let (tx, _rx) = crossbeam_channel::unbounded::<Command>();
        tx
    }

    /// Construct a minimal valid `AppState` with the v1 task
    /// queue capacity (1) initialised.
    fn fresh_state() -> AppState {
        AppState {
            tasks: TaskQueue::new(1),
            ..Default::default()
        }
    }

    #[test]
    fn user_selected_dir_sets_input_dir() {
        let mut state = fresh_state();
        let tx = cmd_tx();
        let p = PathBuf::from("/some/dir");
        reduce(
            &mut state,
            Msg::UserSelectedDir(p.clone()),
            &tx,
            &default_config(),
        );
        assert_eq!(state.input_dir, Some(p));
    }

    #[test]
    fn user_selected_dir_rejected_when_task_active() {
        let mut state = fresh_state();
        let tx = cmd_tx();
        // Active task in Analyzing phase.
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: None,
            phase: BatchPhase::Analyzing,
            dir: PathBuf::from("/x"),
            total_files: 0,
            to_encode: 0,
            to_skip: 0,
            encoded: 0,
            cancelled: false,
            finished: false,
            records: vec![],
            dry_run_records: vec![],
        });
        reduce(
            &mut state,
            Msg::UserSelectedDir(PathBuf::from("/y")),
            &tx,
            &default_config(),
        );
        assert!(state.input_dir.is_none());
    }

    #[test]
    fn user_clicked_start_without_dir_records_error() {
        let mut state = fresh_state();
        let tx = cmd_tx();
        reduce(&mut state, Msg::UserClickedStart, &tx, &default_config());
        assert!(state.last_error.is_some());
        assert!(state.last_error.unwrap().contains("no input dir"));
        assert!(state.tasks.active.is_none());
    }

    #[test]
    fn user_clicked_start_with_active_task_records_busy_error() {
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: None,
            phase: BatchPhase::Analyzing,
            dir: PathBuf::from("/x"),
            total_files: 5,
            to_encode: 0,
            to_skip: 0,
            encoded: 0,
            cancelled: false,
            finished: false,
            records: vec![],
            dry_run_records: vec![],
        });
        state.input_dir = Some(PathBuf::from("/y"));
        reduce(&mut state, Msg::UserClickedStart, &tx, &default_config());
        assert_eq!(
            state.last_error.as_deref(),
            Some("task in progress"),
            "second Start click during Analyzing should be rejected"
        );
    }

    #[test]
    fn batch_started_records_batch_id_as_mapping() {
        let mut state = fresh_state();
        // Drive into AwaitingConfirm.
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: None,
            phase: BatchPhase::AwaitingConfirm,
            dir: PathBuf::from("/x"),
            total_files: 5,
            to_encode: 4,
            to_skip: 1,
            encoded: 0,
            cancelled: false,
            finished: false,
            records: vec![],
            dry_run_records: vec![],
        });
        reduce(
            &mut state,
            Msg::Lib(Event::BatchStarted {
                id: 7,
                total_files: 5,
            }),
            &cmd_tx(),
            &default_config(),
        );
        let t = state.tasks.active.as_ref().unwrap();
        assert_eq!(t.batch_id, Some(7));
        assert_eq!(t.phase, BatchPhase::Encoding);
    }

    #[test]
    fn progress_event_after_finished_is_ignored() {
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: None,
            phase: BatchPhase::Encoding,
            dir: PathBuf::from("/x"),
            total_files: 2,
            to_encode: 2,
            to_skip: 0,
            encoded: 0,
            cancelled: false,
            finished: true, // simulate post-BatchCompleted but still in active
            records: vec![],
            dry_run_records: vec![],
        });
        reduce(
            &mut state,
            Msg::Lib(Event::Progress {
                file_index: 0,
                frame: 100,
                fps: 30.0,
                speed: 1.0,
                time_secs: 1.0,
                eta_secs: None,
            }),
            &tx,
            &default_config(),
        );
        assert!(state.progress.is_empty());
    }

    #[test]
    fn log_ring_caps_at_1000() {
        let mut state = fresh_state();
        let tx = cmd_tx();
        for i in 0..1500 {
            reduce(
                &mut state,
                Msg::Lib(Event::LogLine {
                    file_index: 0,
                    line: format!("line {i}"),
                }),
                &tx,
                &default_config(),
            );
        }
        assert_eq!(state.log.len(), 1000);
        let first = state.log.front().unwrap();
        assert_eq!(first.line, "line 500");
    }

    #[test]
    fn history_loaded_replaces_history() {
        let mut state = fresh_state();
        let tx = cmd_tx();
        let rec = sample_record("/a.mp4", EncodingOutcome::Succeeded);
        reduce(
            &mut state,
            Msg::Lib(Event::HistoryLoaded {
                records: vec![rec.clone()],
            }),
            &tx,
            &default_config(),
        );
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].source_path, "/a.mp4");
    }

    #[test]
    fn batch_cancelled_moves_to_task_history() {
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: Some(1),
            phase: BatchPhase::Encoding,
            dir: PathBuf::from("/x"),
            total_files: 5,
            to_encode: 5,
            to_skip: 0,
            encoded: 2,
            cancelled: false,
            finished: false,
            records: vec![],
            dry_run_records: vec![],
        });
        reduce(
            &mut state,
            Msg::Lib(Event::BatchCancelled {
                completed: 2,
                total: 5,
            }),
            &tx,
            &default_config(),
        );
        // Active should be None; history has the task with same id.
        assert!(state.tasks.active.is_none());
        assert_eq!(state.task_history.len(), 1);
        let h = &state.task_history[0];
        assert_eq!(h.task_id, TaskId("20260101000000".into()));
        assert!(h.cancelled);
        assert!(h.finished);
        assert_eq!(h.phase, BatchPhase::Done);
        assert_eq!(state.last_error.as_deref(), Some("cancelled after 2/5"));
    }

    #[test]
    fn file_completed_pushes_record_and_increments_encoded() {
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: Some(1),
            phase: BatchPhase::Encoding,
            dir: PathBuf::from("/x"),
            total_files: 2,
            to_encode: 2,
            to_skip: 0,
            encoded: 0,
            cancelled: false,
            finished: false,
            records: vec![],
            dry_run_records: vec![],
        });
        let rec = sample_record("/first.mp4", EncodingOutcome::Succeeded);
        reduce(
            &mut state,
            Msg::Lib(Event::FileCompleted {
                index: 0,
                record: rec.clone(),
            }),
            &tx,
            &default_config(),
        );
        let t = state.tasks.active.as_ref().unwrap();
        assert_eq!(t.records.len(), 1);
        assert_eq!(t.records[0].source_path, "/first.mp4");
        assert_eq!(t.encoded, 1);
    }

    #[test]
    fn error_event_sets_last_error() {
        let mut state = fresh_state();
        let tx = cmd_tx();
        reduce(
            &mut state,
            Msg::Lib(Event::Error {
                category: rencoder_lib::types::ErrorCategory::ConfigError,
                message: "ffmpeg not found".into(),
            }),
            &tx,
            &default_config(),
        );
        assert_eq!(state.last_error.as_deref(), Some("ffmpeg not found"));
    }

    #[test]
    fn error_during_encoding_moves_task_to_history() {
        // Per §3.8.7.1: Encoding-phase Error is a completed
        // task (it ran); goes to task_history.
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: Some(1),
            phase: BatchPhase::Encoding,
            dir: PathBuf::from("/x"),
            total_files: 5,
            to_encode: 5,
            to_skip: 0,
            encoded: 1,
            cancelled: false,
            finished: false,
            records: vec![],
            dry_run_records: vec![],
        });
        reduce(
            &mut state,
            Msg::Lib(Event::Error {
                category: rencoder_lib::types::ErrorCategory::FfmpegNonzeroExit,
                message: "encode failed".into(),
            }),
            &tx,
            &default_config(),
        );
        assert!(state.tasks.active.is_none());
        assert_eq!(state.task_history.len(), 1);
        let h = &state.task_history[0];
        assert_eq!(h.phase, BatchPhase::Done);
        assert!(h.finished);
    }

    #[test]
    fn error_during_analyzing_drops_without_history() {
        // Per §3.8.7.1: Analyzing-phase Error means task never
        // started; not filed in task_history.
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: None,
            phase: BatchPhase::Analyzing,
            dir: PathBuf::from("/x"),
            total_files: 5,
            to_encode: 0,
            to_skip: 0,
            encoded: 0,
            cancelled: false,
            finished: false,
            records: vec![],
            dry_run_records: vec![],
        });
        reduce(
            &mut state,
            Msg::Lib(Event::Error {
                category: rencoder_lib::types::ErrorCategory::ConfigError,
                message: "another batch is already running".into(),
            }),
            &tx,
            &default_config(),
        );
        assert!(state.tasks.active.is_none());
        assert!(state.task_history.is_empty());
    }

    #[test]
    fn dry_run_finished_computes_to_encode_and_to_skip() {
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: None,
            phase: BatchPhase::Analyzing,
            dir: PathBuf::from("/x"),
            total_files: 3,
            to_encode: 0,
            to_skip: 0,
            encoded: 0,
            cancelled: false,
            finished: false,
            records: vec![],
            dry_run_records: vec![],
        });
        let records = vec![
            sample_record("/a.mp4", EncodingOutcome::Succeeded),
            sample_skipped("/b.mp4"),
            sample_record("/c.mp4", EncodingOutcome::Failed),
        ];
        reduce(
            &mut state,
            Msg::Lib(Event::DryRunFinished { records }),
            &tx,
            &default_config(),
        );
        let t = state.tasks.active.as_ref().unwrap();
        assert_eq!(t.phase, BatchPhase::AwaitingConfirm);
        assert_eq!(t.to_encode, 1);
        assert_eq!(t.to_skip, 2);
        assert_eq!(t.dry_run_records.len(), 3);
    }

    #[test]
    fn cancel_during_analyzing_drops_dry_run_finished() {
        // Phantom-AwaitingConfirm guard: cancel during Analyzing,
        // then DryRunFinished arrives -> drop, not a phantom plan.
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: None,
            phase: BatchPhase::Analyzing,
            dir: PathBuf::from("/x"),
            total_files: 1,
            to_encode: 0,
            to_skip: 0,
            encoded: 0,
            cancelled: false,
            finished: false,
            records: vec![],
            dry_run_records: vec![],
        });
        // User cancels (Cancel sends Command::Cancel + sets
        // cancelled = true per §3.8.7.1 cancel-during-Analyzing
        // race fix).
        reduce(&mut state, Msg::UserClickedCancel, &tx, &default_config());
        assert!(state.tasks.active.as_ref().unwrap().cancelled);
        // DryRun still arrives — should drop without
        // transitioning to AwaitingConfirm.
        reduce(
            &mut state,
            Msg::Lib(Event::DryRunFinished {
                records: vec![sample_record("/a.mp4", EncodingOutcome::Succeeded)],
            }),
            &tx,
            &default_config(),
        );
        assert!(state.tasks.active.is_none());
        assert!(state.task_history.is_empty());
    }

    #[test]
    fn user_clicked_tab_updates_active_page() {
        let mut state = fresh_state();
        let tx = cmd_tx();
        reduce(
            &mut state,
            Msg::UserClickedTab(Page::History),
            &tx,
            &default_config(),
        );
        assert_eq!(state.active_page, Page::History);
        reduce(
            &mut state,
            Msg::UserClickedTab(Page::Task),
            &tx,
            &default_config(),
        );
        assert_eq!(state.active_page, Page::Task);
    }

    #[test]
    fn task_id_collision_gets_suffix() {
        // Two TaskIds allocated in the same second get -N suffix.
        // (Don't actually sleep — fake by direct construction;
        // the counter logic is what's under test.)
        let mut q = TaskQueue::new(1);
        let base = q.next_task_id();
        let id2 = q.next_task_id();
        assert_eq!(id2.as_str(), format!("{}-1", base.as_str()));
        let id3 = q.next_task_id();
        assert_eq!(id3.as_str(), format!("{}-2", base.as_str()));
    }

    #[test]
    fn has_room_respects_capacity() {
        let mut q = TaskQueue::new(1);
        assert!(q.has_room());
        q.active = Some(TaskInfo {
            task_id: TaskId("t".into()),
            batch_id: None,
            phase: BatchPhase::Analyzing,
            dir: PathBuf::from("/x"),
            total_files: 0,
            to_encode: 0,
            to_skip: 0,
            encoded: 0,
            cancelled: false,
            finished: false,
            records: vec![],
            dry_run_records: vec![],
        });
        assert!(!q.has_room());
    }

    #[test]
    fn error_context_construction_smoke() {
        use rencoder_lib::types::ErrorContext;
        let _: Option<ErrorContext> = None;
    }

    #[test]
    fn build_encode_params_uses_config() {
        use rencoder_lib::config::app::{AppConfig, EncodeConfig, FormatArg};
        let cfg = AppConfig {
            encode: Some(EncodeConfig {
                format: Some(FormatArg::Hevc),
                crf: 28,
                ..Default::default()
            }),
            ..Default::default()
        };
        let params = build_encode_params_from_config(&cfg);
        assert_eq!(params.format, rencoder_lib::types::VideoFormat::Hevc);
        assert!(params.is_crf);
        assert_eq!(params.bitrate_or_crf, 28);
    }

    #[test]
    fn build_encode_params_defaults_when_section_missing() {
        let cfg = AppConfig::default();
        let params = build_encode_params_from_config(&cfg);
        assert_eq!(params.format, rencoder_lib::types::VideoFormat::Av1);
        assert!(params.is_crf);
        assert_eq!(params.bitrate_or_crf, 45);
    }

    #[test]
    fn extension_matches_empty_means_all() {
        assert!(extension_matches(&PathBuf::from("/foo.mp4"), &[]));
        assert!(extension_matches(&PathBuf::from("/foo.bar"), &[]));
    }

    #[test]
    fn extension_matches_case_insensitive() {
        assert!(extension_matches(
            &PathBuf::from("/FOO.MP4"),
            &["mp4".into()]
        ));
        assert!(extension_matches(
            &PathBuf::from("/foo.mp4"),
            &["MP4".into()]
        ));
    }

    #[test]
    fn extension_matches_strips_leading_dot() {
        assert!(extension_matches(
            &PathBuf::from("/foo.mp4"),
            &[".mp4".into()]
        ));
    }

    #[test]
    fn extension_matches_rejects_non_matching() {
        assert!(!extension_matches(
            &PathBuf::from("/foo.txt"),
            &["mp4".into(), "mkv".into()]
        ));
        assert!(!extension_matches(&PathBuf::from("/foo"), &["mp4".into()]));
    }
}
