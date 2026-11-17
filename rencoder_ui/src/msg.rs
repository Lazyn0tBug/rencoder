//! UI state + Msg type + reducer (U21 §3.8.7 "everything is msg").
//!
//! The view layer is purely a function of state; state changes
//! only via `reduce(state, msg, cmd_tx)` which also dispatches
//! Commands back to the lib actor as needed.
//!
//! **Workflow state machine** (§3.8.7.1, v1.2 final — review #133):
//!
//! ```text
//! [None]
//!   |  (UserClickedStart or UserSelectedDir)
//!   v
//! [Analyzing] --Event::DryRunFinished--> [AwaitingConfirm]
//!                                          | (to_encode > 0)
//!                                          | UserClickedStart
//!                                          v
//! [Encoding] --BatchCompleted/Cancelled/Error--> [Done]
//! [Done]    --UserClickedStart (re-arm)--> [Analyzing]
//! ```
//!
//! v1.2 user feedback (review #133 follow-up): removed the
//! auto-encode on positive dry-run count. Encoding is a
//! deliberate user action — the user clicks "Encode N files ▸"
//! after reviewing the dry-run results.
//!
//! v1 invariant: TaskQueue capacity = 1. Gating unit is task
//! existence (`tasks.active.is_some()`), not batch id (batch_id
//! is a lib-side mapping field that doesn't participate in
//! gating decisions).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use chrono::{DateTime, Local, Utc};
use crossbeam_channel::Sender;

use rencoder_lib::job::{Command, EncodingRecord, Event};
use rencoder_lib::types::EncodingOutcome;

/// Cap on the stderr log ring buffer. Per §3.8.6 + §8 item 30.
const LOG_RING_CAP: usize = 1000;

/// Lifecycle phase of a task (v1.2 follow-up #2):
/// 6-state machine with a manual Confirm step between
/// DryRunFinished and StartBatch, plus an explicit `Cancelled`
/// terminal state for optimistic cancel.
///
/// ```text
/// [None]
///   |  (UserClickedStart or UserSelectedDir)
///   v
/// [Analyzing] --Event::DryRunFinished--> [AwaitingConfirm]
///                                            | (to_encode > 0)
///                                            | UserClickedStart
///                                            v
/// [Encoding] --BatchCompleted--> [Done]
///           --UserClickedCancel--> [Cancelled]   (optimistic; late
///             BatchCancelled events filtered out by apply_event)
///           --Event::Error--> [Done]
/// [Done/Cancelled] --dir re-pick / new dry-run--> [Analyzing]
/// ```
///
/// Cancel is **optimistic**: `Msg::UserClickedCancel` sets
/// `phase = Cancelled` immediately. `apply_event` then drops any
/// late batch events (`BatchStarted` / `FileStarted` / `Progress`
/// / `FileCompleted` / `BatchCompleted` / `BatchCancelled` /
/// `Error`) for this batch — see `apply_event` phase guard.
/// This eliminates the ~1.5s window where the badge flickered
/// "encoding" after the user clicked cancel (lib cancel latency
/// breakdown: §3.8.7.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchPhase {
    /// `Command::DryRun` dispatched; awaiting `Event::DryRunFinished`.
    /// Dir picker enabled (re-pick resets). Start button disabled.
    Analyzing,
    /// DryRun done; user must click the bottom button to dispatch
    /// `Command::StartBatch`. The button shows **"Encode"** (no
    /// count — we don't design for encoding a subset; per
    /// §3.8.7.1 decision 2).
    AwaitingConfirm,
    /// `Command::StartBatch` dispatched; awaiting `BatchCompleted`
    /// / `BatchCancelled` / `Event::Error`. Start button shows
    /// "Cancel ✕".
    Encoding,
    /// Final state after a successful batch lifecycle. Dir picker
    /// re-enabled. Start button **disabled** (re-arm via dir
    /// re-pick — per §3.8.7.1 decision 4).
    Done,
    /// Final state after user-initiated cancel. Dir picker
    /// re-enabled. Start button shows **"Encode"** (re-click
    /// dispatches `StartBatch` with the cached dry-run records;
    /// per §3.8.7.1 decision 5).
    Cancelled,
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

/// Setup-page config field identifiers (review #133 part 2d).
/// One enum per editable field — the reducer's match is exhaustive
/// at compile time (no wildcard string-keyed updates).
#[allow(dead_code)]
// constructed by the Setup-page form (app.rs) on
// every <input>/radio change; not constructed by the reducer itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFieldKind {
    Format,
    Crf,
    MaxFps,
    Extensions,
    /// NVENC hardware acceleration on / off.
    Nv,
}

/// Generic value type for `UserEditedConfigField` (review #133).
#[allow(dead_code)] // constructed by the Setup-page form (app.rs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigFieldValue {
    Format(rencoder_lib::config::app::FormatArg),
    Crf(u32),
    /// `None` = cap disabled, `Some(30)` = cap at 30 fps.
    MaxFps(Option<u32>),
    /// Comma-separated. Empty string clears the filter (= "all").
    Extensions(String),
    /// NVENC hardware acceleration on / off.
    Nv(bool),
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
    /// In-memory mirror of the on-disk `AppConfig` for the Setup
    /// page's inline-edit form (review #133). Updated by every
    /// `Msg::UserEditedConfigField` reducer arm and written to
    /// TOML via `Command::SaveConfig`. Initialized at app start
    /// from the loaded `AppConfig`.
    pub config_draft: rencoder_lib::config::app::AppConfig,
    /// Display label for the Setup page's save-status indicator
    /// (`"Saved at HH:MM:SS"` / `"Saving…"` / `"Save failed: …"`).
    pub config_save_status: String,
    /// v1.2 (review #133 follow-up): transient user-action
    /// feedback toasts (top-right stack, 4s auto-dismiss, FIFO
    /// cap 5). Distinct from `last_error` (which is the global
    /// banner for lib-side errors; persistent until cleared).
    /// Toasts are user-action feedback only.
    pub toasts: VecDeque<Toast>,
    /// Monotonic counter for `Toast.id` — lets the Toaster
    /// component use the id as the Dioxus key (stable across
    /// re-renders while the toast is on screen).
    next_toast_id: u64,
    /// NVENC capability of this machine, as reported by the lib
    /// (`Event::NvencCapability`). `None` until the first report
    /// arrives — the lib resolves it at startup and on every
    /// `Command::Detect`.
    pub nvenc_capability: Option<rencoder_lib::types::NvencCapability>,
}

/// Cap on the toasts queue. Older toasts are popped when the
/// queue exceeds this — keeps the UI bounded even under bursts
/// of failures.
pub const TOAST_QUEUE_CAP: usize = 5;

/// Default toast lifetime (ms). User can dismiss earlier via the
/// × button; auto-dismiss otherwise.
pub const TOAST_DEFAULT_DURATION_MS: u32 = 4000;

/// One transient UI feedback message (review #133 v1.2 toast
/// system). Renders as a fixed-position card in the top-right
/// stack. Auto-dismissed after `duration_ms` unless the user
/// clicks × first.
#[allow(dead_code)] // fields read by the Toaster component
// (app.rs); reducer pushes Toasts without reading them.
#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    /// Stable id — monotonic, wraps at u64::MAX (effectively
    /// never — billions of toasts before wrap). The Toaster
    /// uses this as the Dioxus `key` so reordering / removal
    /// doesn't remount unrelated toasts.
    pub id: u64,
    /// Display text. Single line (CSS truncates with ellipsis if
    /// very long; in practice messages are < 80 chars).
    pub message: String,
    /// Drives the left-stripe color and icon (⚠ / ⚡ / ⓘ).
    pub kind: ToastKind,
    /// When the toast was pushed. The Toaster's setInterval
    /// prunes entries where `now - created_at >= duration_ms`.
    pub created_at: DateTime<Utc>,
    /// Auto-dismiss delay in ms. Default 4000.
    pub duration_ms: u32,
}

/// Toast severity. Drives both visual styling (left stripe
/// color, icon) and a11y (aria-live="polite" for Info/Warning,
/// "assertive" for Error).
#[allow(dead_code)] // Info variant used by future paths;
// current reducer only emits Warning / Error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    /// Neutral feedback (success, info). No special urgency.
    Info,
    /// Non-fatal issue — user should know but action isn't
    /// required (e.g. "0 encodable files; pick a different dir").
    Warning,
    /// Failed action — user did something and it didn't work.
    Error,
}

/// Push a toast onto the queue with FIFO eviction at
/// `TOAST_QUEUE_CAP`. Helper used by the reducer on user-action
/// failure paths; tests assert on the queue contents.
pub fn push_toast(state: &mut AppState, msg: impl Into<String>, kind: ToastKind) {
    state.toasts.push_back(Toast {
        id: state.next_toast_id,
        message: msg.into(),
        kind,
        created_at: Utc::now(),
        duration_ms: TOAST_DEFAULT_DURATION_MS,
    });
    state.next_toast_id = state.next_toast_id.wrapping_add(1);
    while state.toasts.len() > TOAST_QUEUE_CAP {
        state.toasts.pop_front();
    }
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
    ///
    /// **Not** currently called by the reducer — the `None` arm of
    /// `UserClickedStart` knows `active` is `None` (so trivially
    /// has room) and the `Some(Done)` re-arm arm overwrites
    /// `active` after taking. v1.1 multi-task will need this
    /// check back (when capacity > 1 and pending exists).
    #[allow(dead_code)]
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
    /// File index this log line belongs to. `usize::MAX` (= sentinel
    /// for "no file context") for lib-internal tracing events;
    /// 0..N for per-file ffmpeg stderr lines.
    pub file_index: usize,
    /// Origin of this log line (§3.8.7.5):
    /// - `FfmpegStderr`: ffmpeg's stderr per-file (via
    ///   `Event::LogLine`)
    /// - `LibInternal`: lib's `tracing::info!` / `tracing::warn!` events (v1.2 follow-up #4 — was `log::info!` bridged via `tracing-log`),
    ///   bridged to tracing and pushed to the UI
    #[allow(dead_code)] // populated but v1 doesn't differentiate visually;
    // v1.2 will use this for color coding
    pub source: LogSource,
    pub line: String,
    pub at: DateTime<Utc>,
}

/// Sentinel `file_index` for log lines without a file context
/// (lib-internal tracing events). `usize::MAX` is well outside
/// any realistic file count. The display layer (app.rs) maps this
/// to "(global)" instead of a huge number.
pub const NO_FILE_INDEX: usize = usize::MAX;

/// Origin of a `LogLine` (§3.8.7.5 unified log pipeline).
#[allow(dead_code)] // v1: visually treated the same as ffmpeg stderr;
// v1.2 will use this to color-code the log line
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    /// Per-file ffmpeg stderr line (`Event::LogLine`). Has a
    /// meaningful `file_index`.
    FfmpegStderr,
    /// Lib-internal tracing event (v1.2 follow-up #4: no longer bridged
    /// lib emits `tracing` directly and UI consumes via the same
    /// subscriber. No file context — `file_index` is `NO_FILE_INDEX`.
    LibInternal,
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
    #[allow(dead_code)] // DirPicker component moved to TaskPage
    // (review #133 part 3c, follows in the next commit).
    UserSelectedDir(PathBuf),
    /// Setup-page live edit (review #133). The Setup page's
    /// `<input>` and radio handlers fire this on every change;
    /// the reducer updates `state.config_draft` AND fires
    /// `Command::SaveConfig` so each keystroke persists atomically.
    #[allow(dead_code)] // constructed by the Setup-page form (app.rs).
    UserEditedConfigField {
        kind: ConfigFieldKind,
        value: ConfigFieldValue,
    },
    /// **Polymorphic** Start button:
    ///   - phase None / Done: dispatch `Command::DryRun` + create task
    ///   - phase Analyzing / Encoding: rejected (busy-gate — Encode/Cancel
    ///     during Analyzing is intentionally not allowed; review #100)
    UserClickedStart,
    /// Phase-aware Cancel:
    ///   - Analyzing / Encoding: set `cancelled = true` + dispatch `Command::Cancel`
    ///   - None / Done: ignored
    UserClickedCancel,
    /// Reload history records from disk (Parquet). Manual
    /// dispatch via Refresh button. v1.1 auto-dispatch on app
    /// start deferred.
    #[allow(dead_code)]
    UserClickedLoadHistory,
    /// User clicked "重新检测" next to the NVENC switch: force a
    /// fresh capability probe. The lib stores the result and
    /// reports it back via `Event::NvencCapability`.
    UserClickedDetectNv,
    /// User clicked a top-bar tab. Reducer writes
    /// `state.active_page`. No lib command.
    UserClickedTab(Page),
    /// Wrapper for lib -> UI events. All lib events flow
    /// through here; reducer routes them in `apply_event`.
    Lib(Event),
    /// Lib-internal tracing line (v1.2 follow-up #4: not bridged
    /// pushed through a crossbeam channel from the log drain
    /// thread). Source field is fixed to `LogSource::LibInternal`.
    LibLogLine(String),
}

/// Reduce a message into a state change; emit Commands to the lib
/// actor as needed.
///
/// **Phase-aware `UserClickedStart`** (§3.8.7.1, review #133):
///   - `None` / `Done` -> build params, collect files, dispatch
///     `Command::DryRun`, optimistically transition to `Analyzing`
///   - `Analyzing` / `Encoding` -> set `last_error = "task in
///     progress"`, return early (busy-gate; Cancel-during-Analyzing
///     is review #100, still pending — the button is disabled)
///
/// **Phase-aware `UserClickedCancel`**:
///   - `Analyzing` / `Encoding`: set `cancelled = true`, dispatch
///     `Command::Cancel`. The lib respects cancel_token only
///     during encoding (Analyze is best-effort).
///   - `None` / `Done`: ignored
pub fn reduce(
    state: &mut AppState,
    msg: Msg,
    cmd_tx: &Sender<Command>,
    // The UI's editable copy of the config lives in `state.config_draft`
    // and is the single source of truth here: the Setup page edits it,
    // the encode params are built from it, and it is what gets saved.
    // The shared config (the lib's startup copy) is not consulted --
    // reading it would ignore the user's edits.
    _config: &rencoder_lib::config::app::AppConfig,
) {
    match msg {
        Msg::UserSelectedDir(path) => {
            if let Some(t) = &state.tasks.active {
                if !matches!(t.phase, BatchPhase::Done) {
                    // P2.5 fix: silently rejected (active task).
                    // Don't keep a stale last_error on screen —
                    // clear it so the user isn't misled by an
                    // error from an earlier action.
                    state.last_error = None;
                    return;
                }
            }
            state.last_error = None;
            state.input_dir = Some(path);
        }
        Msg::UserEditedConfigField { kind, value } => {
            // v1.2 review #133 part 2d: real-time save on every
            // Setup field change. Update the in-memory draft
            // (Setup page reads it on next render) AND fire
            // `Command::SaveConfig` which writes the full
            // AppConfig to TOML atomically via `write_atomic`.
            // Each field change is one save — cheap for small
            // TOML, safe on every keystroke.
            apply_config_field_edit(state, kind, value);
            state.config_save_status = "Saving…".into();
            // The draft is the payload: the lib must persist what the
            // user just edited, not the copy it loaded at startup.
            let _ = cmd_tx.try_send(Command::SaveConfig {
                config: Box::new(state.config_draft.clone()),
            });
        }
        Msg::UserClickedDetectNv => {
            // Force a fresh probe. The lib stores the result in
            // `<app_dir>/nvenc-capability.json` and reports it back
            // via `Event::NvencCapability`.
            let _ = cmd_tx.try_send(Command::Detect { force: true });
        }
        Msg::UserClickedStart => {
            let phase = state.tasks.active.as_ref().map(|t| t.phase);
            match phase {
                None => {
                    // No active task. Start fresh.
                    dispatch_dry_run_for_new_task(state, cmd_tx);
                }
                Some(BatchPhase::Done) => {
                    // v1.2 follow-up #2: Done requires dir re-pick
                    // to re-arm. UserClickedStart in Done is a
                    // no-op (button is disabled in the UI; this is
                    // defense-in-depth — UI shouldn't dispatch
                    // this, but if it does, ignore).
                }
                Some(BatchPhase::AwaitingConfirm) | Some(BatchPhase::Cancelled) => {
                    // v1.2 follow-up #2: both AwaitingConfirm and
                    // Cancelled dispatch StartBatch with the dir's
                    // current files. AwaitingConfirm fires after
                    // the user reviews the dry-run summary;
                    // Cancelled fires after the user wants to
                    // resume encoding post-cancel (button reads
                    // "Encode" — §3.8.7.1 decision 5).
                    //
                    // Validate the dir is still readable +
                    // non-empty (review-delta F2), then dispatch.
                    // Same code path — keeps the contract tight.
                    let files = match collect_input_files(state) {
                        Ok(files) => files,
                        Err(CollectError::NoInputDir) => {
                            // Defensive: input_dir is None while
                            // an active task exists. Should be
                            // impossible (the dir picker is gated)
                            // but show an error rather than silent
                            // failure.
                            push_toast(state, "input dir missing (re-pick)", ToastKind::Error);
                            return;
                        }
                        Err(CollectError::UnreadableDir(e)) => {
                            let dir = state.tasks.active.as_ref().unwrap().dir.clone();
                            push_toast(
                                state,
                                format!("cannot read {}: {}", dir.display(), e),
                                ToastKind::Error,
                            );
                            return;
                        }
                        Err(CollectError::BatchTooLarge { count, max }) => {
                            push_toast(
                                state,
                                format!(
                                    "batch too large: {} files (max {}). Narrow extensions or split the dir.",
                                    count, max
                                ),
                                ToastKind::Error,
                            );
                            return;
                        }
                    };
                    if files.is_empty() {
                        let dir = state.tasks.active.as_ref().unwrap().dir.display();
                        push_toast(
                            state,
                            format!("no files in {} (file list changed since DryRun)", dir),
                            ToastKind::Error,
                        );
                        return;
                    }
                    let params = build_encode_params_from_draft(state);
                    {
                        let t = state.tasks.active.as_mut().unwrap();
                        t.phase = BatchPhase::Encoding;
                        // Clear dry_run_records so the file list
                        // switches from dry-run outcomes
                        // (Succeeded/Skipped/Failed from probe)
                        // to encoding outcomes (per-file FileCompleted
                        // events). Without this, the UI would keep
                        // showing the dry-run results during
                        // Encoding — misleading.
                        t.dry_run_records.clear();
                        t.records.clear();
                    }
                    // Review #111: cmd_tx is bounded (capacity 16).
                    // `try_send` is non-blocking; on Full or
                    // Disconnected the command is dropped. A dropped
                    // Cmd is equivalent to a no-op (the user sees
                    // no BatchStarted response and can retry).
                    let _ = cmd_tx.try_send(Command::StartBatch { params, files });
                }
                Some(BatchPhase::Analyzing) | Some(BatchPhase::Encoding) => {
                    // Defense-in-depth: the bottom button is
                    // disabled in these phases, so this is
                    // unreachable via the UI. Keep the toast as
                    // a last-resort signal for keyboard /
                    // programmatic dispatch.
                    push_toast(state, "task in progress", ToastKind::Warning);
                }
            }
        }
        Msg::UserClickedCancel => {
            let phase = state.tasks.active.as_ref().map(|t| t.phase);
            match phase {
                Some(BatchPhase::Encoding) => {
                    // v1.2 follow-up #2: optimistic cancel.
                    // Set `phase = Cancelled` immediately so the
                    // UI (badge / button) reflects the user's
                    // intent without waiting for lib's late
                    // `Event::BatchCancelled` (1-2s latency —
                    // §3.8.7.6). The phase guard in `apply_event`
                    // will drop the late event. Dispatch the
                    // command so the lib can kill the in-flight
                    // ffmpeg + clean up partial output.
                    if let Some(t) = state.tasks.active.as_mut() {
                        t.cancelled = true;
                        t.phase = BatchPhase::Cancelled;
                    }
                    let _ = cmd_tx.try_send(Command::Cancel);
                }
                Some(BatchPhase::Analyzing) => {
                    // v1.2 follow-up #2: best-effort cancel during
                    // dry-run. lib doesn't poll cancel_token in
                    // the analyze path (§3.8.7.4 limitations), so
                    // the cancel signal is logged but the analyze
                    // runs to completion. We still set
                    // `cancelled = true` and let the eventual
                    // `Event::DryRunFinished` handler check the
                    // flag and discard the result.
                    if let Some(t) = state.tasks.active.as_mut() {
                        t.cancelled = true;
                    }
                    let _ = cmd_tx.try_send(Command::Cancel);
                }
                Some(BatchPhase::AwaitingConfirm) => {
                    // P2.7 fix: keep the cancelled plan in history
                    // (with `cancelled = true`) so the user has a
                    // record of "I planned X but cancelled". Without
                    // this, cancelling a plan silently vanishes with
                    // no audit trail.
                    finalize(state, |t| {
                        t.cancelled = true;
                        // `finished` stays false — that flag is
                        // reserved for tasks that actually ran.
                        // Phase stays at AwaitingConfirm so the user
                        // can see "this was a cancelled plan" vs
                        // "this finished encoding".
                    });
                }
                Some(BatchPhase::Cancelled) => {
                    // Defense-in-depth: already cancelled. The
                    // command was dispatched on the first click;
                    // subsequent clicks are no-ops.
                }
                Some(BatchPhase::Done) => {
                    // Done task — button is hidden (no active
                    // encoding to cancel). User would need to
                    // re-pick dir to start a new task.
                }
                None => {
                    // No active task; nothing to cancel.
                }
            }
        }
        Msg::UserClickedLoadHistory => {
            let _ = cmd_tx.try_send(Command::LoadHistory {
                limit: Some(100),
                after: None,
            });
        }
        Msg::UserClickedTab(page) => {
            state.active_page = page;
        }
        Msg::Lib(evt) => apply_event(state, evt),
        Msg::LibLogLine(line) => {
            state.log.push_back(LogLine {
                file_index: NO_FILE_INDEX,
                source: LogSource::LibInternal,
                line,
                at: Utc::now(),
            });
            while state.log.len() > LOG_RING_CAP {
                state.log.pop_front();
            }
        }
    }
}

fn apply_event(state: &mut AppState, evt: Event) {
    // v1.2 follow-up #2 — phase guard (optimistic cancel). Once
    // a batch is Cancelled, the UI trusts the user's click over
    // lib's eventual confirmation. All subsequent batch-scoped
    // events for the current task are dropped. This eliminates
    // the ~1-2s window where the badge flickered "encoding"
    // after cancel (lib cancel latency breakdown §3.8.7.6) and
    // the late `Event::BatchCancelled` flipping `phase` to
    // `Done` (which would override the user's cancel intent).
    //
    // Non-batch events (`HistoryLoaded` / `ConfigSaved` /
    // `AnalyzeStarted` / `AnalyzeFinished` / etc.) are NOT
    // batch-scoped; the guard does not drop them.
    if matches!(
        evt,
        Event::BatchStarted { .. }
            | Event::FileStarted { .. }
            | Event::Progress { .. }
            | Event::FileCompleted { .. }
            | Event::BatchCompleted { .. }
            | Event::BatchCancelled { .. }
            | Event::Error { .. }
    ) && state
        .tasks
        .active
        .as_ref()
        .is_some_and(|t| t.phase == BatchPhase::Cancelled)
    {
        // Drop late events for the cancelled batch. lib cleanup
        // happens silently in the background; user already saw
        // the badge flip to [● CANCELLED] on click.
        return;
    }
    match evt {
        Event::BatchStarted { id, total_files } => {
            // Record the lib's batch_id on the active task
            // (id-as-mapping; not a gating decision per
            // §3.8.7.4). Phase is already Encoding by the time
            // BatchStarted arrives — DryRunFinished set it before
            // dispatching StartBatch (auto-encode path, review
            // #133 part 3d).
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
            // v1.2 final (review #133 follow-up): DryRunFinished
            // transitions to AwaitingConfirm (NOT auto-encode
            // anymore — user must explicitly click the bottom
            // button to start encoding). Two outcomes:
            //   - to_encode == 0 -> set last_error + finalize
            //     (audit trail preserved with finished=false).
            //   - to_encode > 0  -> set phase = AwaitingConfirm.
            //     User reviews the dry-run results + the
            //     "Encode N files ▸" button in the bottom bar;
            //     UserClickedStart (AwaitingConfirm arm) then
            //     dispatches StartBatch.
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
            let to_encode = records
                .iter()
                .filter(|r| matches!(r.outcome, EncodingOutcome::Succeeded))
                .count();
            if state.tasks.active.is_none() {
                return;
            }
            let total_files = state.tasks.active.as_ref().unwrap().total_files;
            // review-delta F5: clamp to_skip at 0. The normal
            // path has records.len() == total_files (each file
            // produces one record); if a file vanished mid-dry-run
            // and records.len() < total_files, subtracting gives a
            // stale 'skip' count.
            let to_skip = total_files.saturating_sub(to_encode);
            {
                let t = state.tasks.active.as_mut().unwrap();
                t.dry_run_records = records;
                t.to_encode = to_encode;
                t.to_skip = to_skip;
            }
            // v1.2 follow-up #2: dry-run summary toast. The
            // Analyze stats (`Found N to encode, M to skip` /
            // `0 encodable`) live ONLY in the toast — the file
            // list area is empty during Analyzing/AwaitingConfirm
            // (§3.8.7.6 + the file list render in app.rs). Toast
            // replaces the previous body-level summary line.
            if to_encode == 0 {
                push_toast(
                    state,
                    format!("0 encodable files ({to_skip} skipped); pick a different dir"),
                    ToastKind::Warning,
                );
                finalize(state, |t| {
                    t.cancelled = false;
                    // t.finished stays false — plan never ran.
                });
                return;
            }
            push_toast(
                state,
                format!("Found {to_encode} to encode ({to_skip} to skip)"),
                ToastKind::Info,
            );
            // AwaitingConfirm: user reviews the toast, then
            // clicks "Encode" (no count suffix — §3.8.7.1
            // decision 2). UserClickedStart's AwaitingConfirm
            // arm dispatches StartBatch.
            state.tasks.active.as_mut().unwrap().phase = BatchPhase::AwaitingConfirm;
        }
        Event::AnalyzeStarted { .. }
        | Event::AnalyzeProgress { .. }
        | Event::AnalyzeFinished { .. } => {
            // U22 standalone Analyze no longer used by the UI
            // (Start auto-DryRun subsumes it). Silently drop.
        }
        Event::NvencCapability(capability) => {
            // The lib resolved this machine's NVENC capability (at
            // startup or after a `Command::Detect`). The Setup page
            // renders it next to the NVENC switch.
            state.nvenc_capability = Some(capability);
        }
        Event::ConfigSaved { .. } => {
            // review-delta F1 (was empty): wire the lib's save
            // success ack to the Setup-page save-status indicator.
            // Command::SaveConfig (job.rs:1107) emits
            // Event::ConfigSaved on success and Event::Error
            // with a 'save failed:' prefix on failure. The
            // Setup page reads state.config_save_status; the
            // prior empty handler left it stuck at 'Saving…'.
            state.config_save_status = format!("Saved at {}", Local::now().format("%H:%M:%S"));
        }
        Event::FileCompleted { record, .. } => {
            if let Some(t) = state.tasks.active.as_mut() {
                t.records.push(record);
                t.encoded += 1;
            }
            // Per-file progress is a snapshot of the LAST file
            // ffmpeg reported on; clear it so the ProgressPanel
            // shows the next in-flight file's metrics, not the
            // finished file's stale ones. (Restored from
            // pre-redesign behavior; the redesign accidentally
            // dropped this and the panel kept the last row's
            // stale fps/eta until next Event::Progress.)
            state.progress.clear();
        }
        Event::BatchCompleted { records } => {
            finalize(state, |t| {
                t.records = records;
                t.finished = true;
                t.phase = BatchPhase::Done;
            });
            state.progress.clear();
        }
        Event::BatchCancelled { completed, total } => {
            // v1.2 follow-up #2: phase guard at the top of
            // apply_event already dropped this event when the
            // active task is in `Cancelled` phase (the optimistic
            // cancel path). So reaching here means the active
            // task is NOT Cancelled — most likely the cancel
            // arrived during AwaitingConfirm (best-effort cancel
            // before encoding started) or after a successful
            // BatchCompleted but for a previous task. In both
            // cases the existing finalize-with-cancelled=true
            // behavior is the right fallback.
            //
            // We previously set `state.last_error` to a
            // "cancelled after N/M" string here, but the badge
            // `[● CANCELLED]` already conveys the same info —
            // the duplicate was visible below the batch progress
            // line and looked contradictory with the optimistic
            // cancel flow (where the badge flips instantly but
            // the late BatchCancelled event might trigger a stale
            // "cancelled after N/M" string seconds later).
            let _ = (completed, total); // suppress unused
            finalize(state, |t| {
                t.cancelled = true;
                t.finished = true;
                // Phase stays at whatever it was — could be
                // AwaitingConfirm (best-effort cancel pre-Start)
                // or Done (terminal cancel from a previous task).
                // Don't force to Done because that would override
                // a legitimate AwaitingConfirm in the best-effort
                // case.
            });
        }
        Event::LogLine { file_index, line } => {
            state.log.push_back(LogLine {
                file_index,
                source: LogSource::FfmpegStderr,
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
            // review-delta F1: if the lib's save failed (it
            // emits this with the prefix 'save failed:'),
            // surface the error on the Setup-page save-status
            // indicator AND push a toast. Other Event::Error
            // paths (transcoding failures etc.) set the global
            // last_error banner but don't fire a toast —
            // toasts are reserved for user-action feedback.
            if let Some(rest) = message.strip_prefix("save failed:") {
                // rest is the lib's I/O error string (e.g.
                // " permission denied (os error 13)" with a
                // leading space — strip it so the indicator
                // and toast show "Save failed: permission denied ...").
                let detail = rest.trim_start();
                state.config_save_status = format!("Save failed: {detail}");
                push_toast(state, format!("Save failed: {detail}"), ToastKind::Error);
            }
            state.last_error = Some(message);
            // Per §3.8.7.1: during Encoding, move to history
            // (task did run, even if it failed); during
            // Analyzing, drop without history (task never
            // started). review #133 part 3d removed the
            // AwaitingConfirm arm (unreachable under auto-encode).
            let phase = state.tasks.active.as_ref().map(|t| t.phase);
            match phase {
                Some(BatchPhase::Encoding) => {
                    finalize(state, |t| {
                        t.finished = true;
                        t.phase = BatchPhase::Done;
                    });
                }
                Some(BatchPhase::Analyzing) => {
                    drop_active(state);
                }
                _ => {
                    // Future variants — silently drop (active
                    // stays as-is). review #133 removed the
                    // AwaitingConfirm arm; the catch-all covers
                    // any new phases the lib might surface.
                    let _ = state.tasks.active.as_ref();
                }
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
fn build_encode_params_from_draft(state: &AppState) -> rencoder_lib::types::EncodeParams {
    rencoder_lib::types::EncodeParams::from_app_config(state.config_draft.encode.as_ref())
}

/// Errors that `collect_input_files` can return. Distinguishes
/// "user hasn't picked a directory" from "directory is unreadable"
/// (permission denied / path doesn't exist / filesystem error)
/// from "directory is empty / no files match extensions". The old
/// `Option<Vec<PathBuf>>` API conflated "None" with all of these,
/// leading to generic "no files in X" errors even when the real
/// issue was I/O.
#[derive(Debug)]
enum CollectError {
    /// `state.input_dir` is None. User hasn't picked a directory.
    NoInputDir,
    /// I/O failure reading the directory — permission denied, path
    /// doesn't exist, deleted mid-walk, walkdir errored on a symlink
    /// loop, etc. Carries the lib's `EncodeError` so the toast
    /// can show the underlying category / message rather than a
    /// generic "unreadable dir".
    UnreadableDir(rencoder_lib::error::EncodeError),
    /// v1.2 follow-up #5: lib's `collect_files` exceeded
    /// `[encode] max_batch_size`. The user needs to narrow
    /// extensions or split the directory — surfaced as a toast
    /// with the count + max so they know by how much they overshot.
    BatchTooLarge { count: usize, max: u32 },
}

/// Collect input files from `state.input_dir`, filtered by the
/// configured extensions from `config.encode.extensions`
/// (empty = all).
///
/// Returns `Err(NoInputDir)` if no directory is selected;
/// `Err(UnreadableDir)` on I/O error; `Ok(vec)` otherwise (the
/// vec may be empty if the directory has no matching files —
/// callers handle that as a distinct "no files" condition).
fn collect_input_files(
    state: &AppState,
) -> Result<Vec<rencoder_lib::types::InputPath>, CollectError> {
    let config = &state.config_draft;
    let Some(dir) = state.input_dir.as_ref() else {
        return Err(CollectError::NoInputDir);
    };
    let extensions = config
        .encode
        .as_ref()
        .and_then(|e| e.extensions.clone())
        .unwrap_or_default();
    // v1.2 follow-up #5: route through lib's recursive `collect_files`
    // (was a non-recursive `std::fs::read_dir` loop, which diverged
    // from the lib's `walkdir`-based recursive walk — a dir with
    // subdirs would show "0 files" in the UI but encode the whole
    // tree). Now the UI's "files found" count matches what
    // StartBatch will actually process, AND the batch cap is enforced
    // uniformly at the same site as the lib.
    let max_batch_size = rencoder_lib::config::app::resolve_max_batch_size(config.encode.as_ref());
    let target = rencoder_lib::types::Target::Dir(dir.clone());
    rencoder_lib::collect::collect_files(&target, &extensions, max_batch_size).map_err(
        |e| match e {
            rencoder_lib::error::EncodeError::BatchTooLarge { count, max } => {
                CollectError::BatchTooLarge { count, max }
            }
            // v1.2 follow-up #5 fix: forward the lib's specific error
            // rather than collapsing every non-BatchTooLarge into a
            // generic UnreadableDir. The toast path uses
            // `Display` (which the lib's thiserror derives) so the
            // user sees "Probe error: walkdir: ..." vs "I/O error:
            // ..." distinctly.
            other => CollectError::UnreadableDir(other),
        },
    )
}

/// Move the active task to history, applying `mutate` to it first.
/// Centralizes the take + insert + viewing_task clear used by
/// 3 call sites (BatchCompleted, BatchCancelled, Error-Encoding,
/// DryRunFinished-with-zero-encode). The Some(Done) re-arm
/// pre-move (no flag changes needed — just move) is handled by
/// the UserClickedStart reducer arm, not here.
fn finalize<F>(state: &mut AppState, mutate: F)
where
    F: FnOnce(&mut TaskInfo),
{
    let done = state.tasks.active.take();
    if let Some(mut t) = done {
        mutate(&mut t);
        state.task_history.insert(0, t);
    }
    state.viewing_task = None;
}

/// Drop the active task without history (e.g., Analyzing-phase
/// Error where the task never started). Used by `Event::Error` to
/// clear the slot without polluting history with a never-encoded
/// task.
fn drop_active(state: &mut AppState) {
    let _ = state.tasks.active.take();
    state.viewing_task = None;
}

/// Dispatch a `Command::DryRun` for a freshly-created task (None or
/// Done re-arm). Caller is responsible for ensuring `tasks.active`
/// is `None` before calling (the reducer's `Some(Done)` arm does
/// `tasks.active.take()` first).
///
/// Sets `state.last_error` and returns early on:
/// - no input dir selected
/// - empty file list (with extension-aware hint)
///
/// On success: creates a new `TaskInfo` with phase=Analyzing,
/// pushes to `tasks.active`, sets `viewing_task`, clears
/// `progress`/`log`, dispatches `Command::DryRun`.
fn dispatch_dry_run_for_new_task(
    state: &mut AppState,
    cmd_tx: &crossbeam_channel::Sender<Command>,
) {
    let files = match collect_input_files(state) {
        Ok(files) => files,
        Err(CollectError::NoInputDir) => {
            // v1.2 (review #133 follow-up): user-action feedback
            // goes through the toast channel, not last_error
            // (which is for lib-side errors per §3.8.7.2).
            push_toast(state, "no input dir selected", ToastKind::Error);
            return;
        }
        Err(CollectError::UnreadableDir(e)) => {
            // `UnreadableDir` only fires when `read_dir(state.input_dir)`
            // was attempted, which requires `state.input_dir` to be
            // `Some` (NoInputDir short-circuits before read_dir).
            // So `unwrap()` is safe here. Forward the lib error
            // so the toast distinguishes "Probe error: walkdir:
            // ..." from "I/O error: ..." — these have different
            // remediations (symlink-loop vs permission).
            let dir = state.input_dir.as_ref().unwrap().clone();
            push_toast(
                state,
                format!("cannot read {}: {}", dir.display(), e),
                ToastKind::Error,
            );
            return;
        }
        Err(CollectError::BatchTooLarge { count, max }) => {
            push_toast(
                state,
                format!(
                    "batch too large: {} files (max {}). Narrow extensions or split the dir.",
                    count, max
                ),
                ToastKind::Error,
            );
            return;
        }
    };
    if files.is_empty() {
        let extensions = state
            .config_draft
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
        // Empty directory is a warning, not an error — the user
        // can pick another dir without it being a "failure".
        push_toast(state, detail, ToastKind::Warning);
        return;
    }
    let params = build_encode_params_from_draft(state);
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
    let _ = cmd_tx.try_send(Command::DryRun { params, files });
}

/// v1.2 review #133 part 2d: Setup-page real-time save helper.
/// The reducer arm for `Msg::UserEditedConfigField` calls this
/// to update the in-memory `config_draft`. The
/// `Command::SaveConfig` dispatch (separate) atomically
/// persists the full `AppConfig` to TOML via `write_atomic`.
///
/// Defensive: missing `encode` section in `config_draft` is
/// populated with a default so the first edit doesn't crash.
fn apply_config_field_edit(state: &mut AppState, kind: ConfigFieldKind, value: ConfigFieldValue) {
    use rencoder_lib::config::app::EncodeConfig;
    if state.config_draft.encode.is_none() {
        state.config_draft.encode = Some(EncodeConfig::default());
    }
    let enc = state.config_draft.encode.as_mut().expect("just inserted");
    match (kind, value) {
        (ConfigFieldKind::Format, ConfigFieldValue::Format(v)) => {
            enc.format = Some(v);
        }
        (ConfigFieldKind::Crf, ConfigFieldValue::Crf(v)) => {
            enc.crf = v;
        }
        (ConfigFieldKind::MaxFps, ConfigFieldValue::MaxFps(v)) => {
            enc.max_fps = v;
        }
        (ConfigFieldKind::Nv, ConfigFieldValue::Nv(v)) => {
            enc.nv = Some(v);
        }
        (ConfigFieldKind::Extensions, ConfigFieldValue::Extensions(s)) => {
            // Parse "mp4, mkv, mov " -> ["mp4", "mkv", "mov"].
            // Empty string clears the filter (= "all").
            let parts: Vec<String> = s
                .split(',')
                .map(|p| p.trim())
                .filter(|p| !p.is_empty())
                .map(String::from)
                .collect();
            enc.extensions = if parts.is_empty() { None } else { Some(parts) };
        }
        // Exhaustiveness: 5 ConfigFieldKind variants; the matching
        // ConfigFieldValue arm exists for each. Any mismatch is a
        // compile error.
        (k, v) => unreachable!(
            "ConfigFieldKind {k:?} + ConfigFieldValue {v:?} mismatch (impossible by construction)"
        ),
    }
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

    // ---- NVENC detect ----

    #[test]
    fn detect_nv_dispatches_a_forced_detect_command() {
        use crossbeam_channel::unbounded;
        let (tx, rx) = unbounded::<Command>();
        let mut state = fresh_state();
        reduce(&mut state, Msg::UserClickedDetectNv, &tx, &default_config());
        match rx
            .try_recv()
            .expect("a command should have been dispatched")
        {
            Command::Detect { force } => assert!(force, "the button must force a probe"),
            _ => panic!("expected Command::Detect"),
        }
    }

    #[test]
    fn nvenc_capability_event_updates_the_state() {
        use crossbeam_channel::unbounded;
        let (tx, _rx) = unbounded::<Command>();
        let mut state = fresh_state();
        assert!(state.nvenc_capability.is_none());
        let capability = rencoder_lib::types::NvencCapability {
            support: vec![rencoder_lib::types::NvencSupport {
                encoder: "av1_nvenc".into(),
                usable: false,
                reason: Some("No NVENC capable devices found".into()),
            }],
            probed_at: Some(DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap()),
            review_due: true,
        };
        reduce(
            &mut state,
            Msg::Lib(Event::NvencCapability(capability.clone())),
            &tx,
            &default_config(),
        );
        let stored = state.nvenc_capability.expect("capability should be stored");
        assert_eq!(stored.support.len(), 1);
        assert!(stored.review_due);
        assert_eq!(stored.support[0].encoder, "av1_nvenc");
    }

    #[test]
    fn editing_the_nv_field_updates_the_config_draft() {
        use crossbeam_channel::unbounded;
        let (tx, _rx) = unbounded::<Command>();
        let mut state = fresh_state();
        reduce(
            &mut state,
            Msg::UserEditedConfigField {
                kind: ConfigFieldKind::Nv,
                value: ConfigFieldValue::Nv(true),
            },
            &tx,
            &default_config(),
        );
        assert_eq!(
            state.config_draft.encode.as_ref().and_then(|e| e.nv),
            Some(true),
            "the draft should carry the nv switch"
        );
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
    fn user_clicked_start_without_dir_pushes_toast() {
        // review #133 v1.2: user-action feedback goes through
        // the toast channel (not last_error).
        let mut state = fresh_state();
        let tx = cmd_tx();
        reduce(&mut state, Msg::UserClickedStart, &tx, &default_config());
        assert!(state.tasks.active.is_none());
        assert_eq!(state.toasts.len(), 1);
        let toast = &state.toasts[0];
        assert_eq!(toast.kind, ToastKind::Error);
        assert!(toast.message.contains("no input dir"));
    }

    #[test]
    fn user_clicked_start_with_active_task_pushes_toast() {
        // Defense-in-depth: button is disabled during Analyzing,
        // but if a click reaches the reducer, fire a warning
        // toast (not last_error — that's for lib-side errors).
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
        assert_eq!(state.toasts.len(), 1);
        let toast = &state.toasts[0];
        assert_eq!(toast.kind, ToastKind::Warning);
        assert_eq!(toast.message, "task in progress");
    }

    #[test]
    fn user_clicked_start_in_done_is_noop() {
        // v1.2 follow-up #2 (§3.8.7.1 decision 4): Done requires
        // dir re-pick to re-arm. `Msg::UserClickedStart` in Done
        // is a no-op (defense-in-depth — UI disables the button;
        // the reducer arm ignores the click). The previous done
        // task stays in `state.tasks.active` (already moved to
        // `state.task_history` by `BatchCompleted` finalize, but
        // a defensive Some(Done) might still surface during the
        // race window).
        let mut state = fresh_state();
        let tx = cmd_tx();
        let prev_id = TaskId("20260101000000".into());
        state.tasks.active = Some(TaskInfo {
            task_id: prev_id.clone(),
            batch_id: Some(1),
            phase: BatchPhase::Done,
            dir: PathBuf::from("/x"),
            total_files: 5,
            to_encode: 5,
            to_skip: 0,
            encoded: 5,
            cancelled: false,
            finished: true,
            records: vec![],
            dry_run_records: vec![],
        });
        state.input_dir = Some(std::path::PathBuf::from("/tmp"));
        reduce(&mut state, Msg::UserClickedStart, &tx, &default_config());

        // No re-arm: the same task remains active, no new
        // DryRun dispatched, no command sent to the lib.
        assert!(state.tasks.active.is_some());
        assert_eq!(
            state.tasks.active.as_ref().unwrap().task_id,
            prev_id,
            "Done-phase click must NOT replace the active task"
        );
        assert_eq!(state.tasks.active.as_ref().unwrap().phase, BatchPhase::Done);
        assert_eq!(
            state.task_history.len(),
            0,
            "no movement to history (Done click is a no-op)"
        );
        // No toast / error fired (the button should be disabled
        // in the UI; this path is unreachable in normal use).
        assert!(state.toasts.is_empty());
        assert!(state.last_error.is_none());
    }

    #[test]
    fn user_selected_dir_rejected_clears_stale_error() {
        // P2.5 fix: when picker is rejected (active task in
        // Analyzing/Encoding), clear any stale last_error so the
        // user doesn't see an error from an earlier action.
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: None,
            phase: BatchPhase::Encoding,
            dir: PathBuf::from("/x"),
            total_files: 5,
            to_encode: 5,
            to_skip: 0,
            encoded: 0,
            cancelled: false,
            finished: false,
            records: vec![],
            dry_run_records: vec![],
        });
        state.last_error = Some("previous unrelated error".into());
        reduce(
            &mut state,
            Msg::UserSelectedDir(PathBuf::from("/y")),
            &tx,
            &default_config(),
        );
        // Picker was rejected; stale error should be cleared.
        assert!(state.last_error.is_none());
        assert_eq!(state.input_dir, None);
    }

    #[test]
    fn batch_completed_clears_viewing_task() {
        // P2.4 fix: after Done transition, viewing_task should
        // not point to the (now-history) task. TaskPage falls
        // back to history.first() instead.
        let mut state = fresh_state();
        let tx = cmd_tx();
        let task_id = TaskId("20260101000000".into());
        state.tasks.active = Some(TaskInfo {
            task_id: task_id.clone(),
            batch_id: Some(1),
            phase: BatchPhase::Encoding,
            dir: PathBuf::from("/x"),
            total_files: 2,
            to_encode: 2,
            to_skip: 0,
            encoded: 2,
            cancelled: false,
            finished: false,
            records: vec![],
            dry_run_records: vec![],
        });
        state.viewing_task = Some(task_id.clone());
        reduce(
            &mut state,
            Msg::Lib(Event::BatchCompleted { records: vec![] }),
            &tx,
            &default_config(),
        );
        assert!(state.tasks.active.is_none());
        assert_eq!(state.viewing_task, None);
        assert_eq!(state.task_history.len(), 1);
        assert_eq!(state.task_history[0].task_id, task_id);
    }

    #[test]
    fn batch_started_records_batch_id_as_mapping() {
        // review #133 part 3d: Event::BatchStarted now records the
        // lib's batch_id on the active task but does NOT change
        // phase. Phase is set to Encoding by Event::DryRunFinished
        // (auto-encode path) before BatchStarted ever fires — the
        // lib transitions Analyzing -> Encoding itself by emitting
        // BatchStarted in response to Command::StartBatch.
        let mut state = fresh_state();
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: None,
            phase: BatchPhase::Encoding,
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
        assert_eq!(t.total_files, 5);
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
    fn user_clicked_cancel_sets_phase_to_cancelled_immediately() {
        // v1.2 follow-up #2: optimistic cancel. The Encoding arm
        // of `Msg::UserClickedCancel` sets `phase = Cancelled`
        // immediately + dispatches `Command::Cancel`. The lib's
        // eventual `Event::BatchCancelled` is filtered out by
        // `apply_event`'s phase guard (separate test).
        let mut state = fresh_state();
        let (tx, rx) = crossbeam_channel::unbounded::<Command>();
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
        reduce(&mut state, Msg::UserClickedCancel, &tx, &default_config());

        // Phase flips to Cancelled synchronously (no wait for
        // lib's late Event::BatchCancelled).
        assert_eq!(
            state.tasks.active.as_ref().unwrap().phase,
            BatchPhase::Cancelled,
            "optimistic cancel: phase must be Cancelled in <1ms"
        );
        assert!(
            state.tasks.active.as_ref().unwrap().cancelled,
            "cancelled flag set alongside phase"
        );
        // Command::Cancel dispatched.
        assert!(
            matches!(rx.try_recv(), Ok(Command::Cancel)),
            "Command::Cancel must be dispatched"
        );
        // No movement to history yet — the in-flight records stay
        // visible until the user re-clicks Encode or picks a new
        // dir (per §3.8.7.1 decision 5).
        assert_eq!(state.task_history.len(), 0);
    }

    #[test]
    fn cancelled_phase_filter_drops_late_batch_events() {
        // v1.2 follow-up #2: phase guard at top of apply_event
        // drops batch-scoped events (BatchStarted / FileStarted /
        // Progress / FileCompleted / BatchCompleted /
        // BatchCancelled / Error) when the active task is in
        // Cancelled phase. This is the optimistic-cancel
        // invariant: user click wins over lib's eventual
        // confirmation.
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: Some(1),
            phase: BatchPhase::Cancelled,
            dir: PathBuf::from("/x"),
            total_files: 5,
            to_encode: 5,
            to_skip: 0,
            encoded: 2,
            cancelled: true,
            finished: false,
            records: vec![sample_record("/a.mp4", EncodingOutcome::Succeeded)],
            dry_run_records: vec![],
        });

        // Late BatchCancelled — must be dropped (no state change).
        let before_records_len = state.tasks.active.as_ref().unwrap().records.len();
        let before_encoded = state.tasks.active.as_ref().unwrap().encoded;
        reduce(
            &mut state,
            Msg::Lib(Event::BatchCancelled {
                completed: 2,
                total: 5,
            }),
            &tx,
            &default_config(),
        );
        assert_eq!(
            state.tasks.active.as_ref().unwrap().phase,
            BatchPhase::Cancelled,
            "phase stays Cancelled after late BatchCancelled"
        );
        assert_eq!(
            state.tasks.active.as_ref().unwrap().records.len(),
            before_records_len,
            "late BatchCancelled must NOT clear records"
        );
        assert_eq!(
            state.tasks.active.as_ref().unwrap().encoded,
            before_encoded,
            "late BatchCancelled must NOT touch encoded count"
        );
        assert!(
            state.last_error.is_none(),
            "phase guard drops the event entirely; no last_error set"
        );

        // Late FileCompleted — must be dropped.
        let before_records_len_2 = state.tasks.active.as_ref().unwrap().records.len();
        reduce(
            &mut state,
            Msg::Lib(Event::FileCompleted {
                index: 2,
                record: sample_record("/late.mp4", EncodingOutcome::Succeeded),
            }),
            &tx,
            &default_config(),
        );
        assert_eq!(
            state.tasks.active.as_ref().unwrap().records.len(),
            before_records_len_2,
            "late FileCompleted dropped"
        );
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
    fn dry_run_finished_transitions_to_awaiting_confirm() {
        // v1.2 final (review #133 follow-up): DryRunFinished
        // no longer auto-encodes. When to_encode > 0, it
        // transitions to AwaitingConfirm — the user reviews the
        // file list, then clicks "Encode N files ▸" in the
        // bottom bar (which dispatches StartBatch via the
        // reducer's AwaitingConfirm arm).
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.input_dir = Some(std::path::PathBuf::from("/nonexistent-for-test"));
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: None,
            phase: BatchPhase::Analyzing,
            dir: PathBuf::from("/nonexistent-for-test"),
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
        // AwaitingConfirm path: phase transitioned, no StartBatch
        // dispatched (the reducer doesn't touch cmd_tx in this
        // branch). dry_run_records populated for the UI.
        let t = state.tasks.active.as_ref().unwrap();
        assert_eq!(t.phase, BatchPhase::AwaitingConfirm);
        assert_eq!(t.to_encode, 1);
        assert_eq!(t.to_skip, 2);
        assert_eq!(t.dry_run_records.len(), 3);
        assert!(t.records.is_empty());
    }

    #[test]
    fn awaiting_confirm_empty_dir_guards_against_zero_file_dispatch() {
        // review-delta F2 (v1.2 path moved): the user can delete
        // every file between DryRun dispatch and the manual
        // Confirm click. The lib's records still reflect the
        // pre-deletion snapshot (to_encode > 0), but the
        // collect_input_files re-read in the AwaitingConfirm
        // arm returns empty. Without this guard we'd dispatch
        // StartBatch with 0 files; the lib would error with a
        // confusing message.
        //
        // v1.2 path: this guard is now in UserClickedStart's
        // AwaitingConfirm arm (was in DryRunFinished's auto-encode
        // path before review #133 user feedback removed auto-encode).
        let tmp = std::env::temp_dir().join(format!(
            "rencoder-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.input_dir = Some(tmp.clone());
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: None,
            phase: BatchPhase::Analyzing,
            dir: tmp.clone(),
            total_files: 1,
            to_encode: 0,
            to_skip: 0,
            encoded: 0,
            cancelled: false,
            finished: false,
            records: vec![],
            dry_run_records: vec![],
        });
        // Step 1: DryRunFinished with a Succeeded record — moves
        // phase to AwaitingConfirm.
        let records = vec![sample_record("/dummy.mp4", EncodingOutcome::Succeeded)];
        reduce(
            &mut state,
            Msg::Lib(Event::DryRunFinished { records }),
            &tx,
            &default_config(),
        );
        assert_eq!(
            state.tasks.active.as_ref().unwrap().phase,
            BatchPhase::AwaitingConfirm
        );
        // Step 2: user clicks Encode — but the dir is empty
        // (we never created files). The guard fires.
        reduce(&mut state, Msg::UserClickedStart, &tx, &default_config());
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(
            state.tasks.active.as_ref().unwrap().phase,
            BatchPhase::AwaitingConfirm,
            "phase stays at AwaitingConfirm; no encoding happened"
        );
        // v1.2 follow-up #2: DryRunFinished pushes a summary toast
        // ("Found N to encode"); the AwaitingConfirm guard on
        // UserClickedStart pushes a second toast ("no files").
        // Both are user-action feedback; we expect 2 toasts.
        assert_eq!(
            state.toasts.len(),
            2,
            "expected DryRun summary + empty-dir guard toasts"
        );
        // The empty-dir guard toast (most recent) is the Error
        // kind — it's the actionable one for the user.
        let guard_toast = state.toasts.back().unwrap();
        assert_eq!(guard_toast.kind, ToastKind::Error);
        assert!(
            guard_toast.message.contains("no files"),
            "expected 'no files' message, got {}",
            guard_toast.message
        );
        // The DryRun summary toast is the Info kind, first in
        // the deque.
        let summary_toast = state.toasts.front().unwrap();
        assert_eq!(summary_toast.kind, ToastKind::Info);
        assert!(
            summary_toast.message.contains("to encode"),
            "expected 'to encode' message, got {}",
            summary_toast.message
        );
    }

    #[test]
    fn dry_run_finished_to_skip_saturates_at_zero() {
        // review-delta F5: if total_files underflows the
        // subtraction `total_files - to_encode` (e.g., a stale
        // total_files carried over after files vanished mid-dry-run),
        // saturating_sub clamps to 0. To trigger this, the
        // task must have to_encode > total_files, which is
        // pathological but possible if total_files got stale.
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.input_dir = Some(std::path::PathBuf::from("/nonexistent-for-test"));
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: None,
            phase: BatchPhase::Analyzing,
            dir: PathBuf::from("/nonexistent-for-test"),
            // Pathological: total_files < records.len(). Pretend the
            // task was set up with total_files=1 but the dry-run
            // found 3 records (1 Succeeded). Old code: to_skip =
            // 1 - 1 = 0 (fine). To force the underflow path, set
            // total_files=0.
            total_files: 0,
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
            sample_skipped("/c.mp4"),
        ];
        reduce(
            &mut state,
            Msg::Lib(Event::DryRunFinished { records }),
            &tx,
            &default_config(),
        );
        // to_encode = 1, to_skip = saturating_sub(0, 1) = 0.
        // Phase: AwaitingConfirm (positive count).
        let t = state.tasks.active.as_ref().unwrap();
        assert_eq!(t.phase, BatchPhase::AwaitingConfirm);
        assert_eq!(t.to_encode, 1);
        assert_eq!(t.to_skip, 0, "to_skip must saturate at 0");
    }

    #[test]
    fn dry_run_finished_with_zero_to_encode_sets_error_and_finalizes() {
        // review #133 part 3d: when to_encode == 0, the task is
        // finalized with finished=false (audit trail preserved)
        // and last_error is set. This is the structural fix for
        // "encode 0 files" being impossible to trigger via the
        // auto-encode path (the lib never receives StartBatch).
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.tasks.active = Some(TaskInfo {
            task_id: TaskId("20260101000000".into()),
            batch_id: None,
            phase: BatchPhase::Analyzing,
            dir: PathBuf::from("/x"),
            total_files: 2,
            to_encode: 0,
            to_skip: 0,
            encoded: 0,
            cancelled: false,
            finished: false,
            records: vec![],
            dry_run_records: vec![],
        });
        let records = vec![sample_skipped("/a.mp4"), sample_skipped("/b.mp4")];
        reduce(
            &mut state,
            Msg::Lib(Event::DryRunFinished { records }),
            &tx,
            &default_config(),
        );
        assert!(state.tasks.active.is_none());
        assert_eq!(state.task_history.len(), 1);
        let h = &state.task_history[0];
        assert!(!h.finished);
        assert!(!h.cancelled);
        // v1.2: user-action feedback is a toast (not last_error).
        assert_eq!(state.toasts.len(), 1);
        let toast = &state.toasts[0];
        assert_eq!(toast.kind, ToastKind::Warning);
        assert!(
            toast.message.contains("0 encodable files"),
            "unexpected toast message: {}",
            toast.message
        );
        assert!(toast.message.contains("2 skipped"));
    }

    #[test]
    fn dry_run_finished_cancelled_during_analyzing_drops_task() {
        // Cancel during Analyzing sets `cancelled = true`. The
        // DryRunFinished handler discards the event (no
        // transition, no auto-encode).
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
            cancelled: true, // Cancel during Analyzing
            finished: false,
            records: vec![],
            dry_run_records: vec![],
        });
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
        assert!(state.last_error.is_none());
    }

    #[test]
    fn cancel_during_analyzing_drops_dry_run_finished() {
        // review #133 part 3d: cancel during Analyzing sets
        // cancelled = true. The DryRunFinished handler discards
        // the event (no auto-encode dispatch, no phase change,
        // no finalize). Without this guard, an in-flight
        // DryRunFinished after a Cancel would silently transition
        // the (already-cancelled) task through the auto-encode
        // path.
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
        // DryRun still arrives — should drop without auto-encoding.
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
    fn user_edited_config_field_updates_draft_and_saves() {
        // review #133 part 2d/3d: each Setup-page field edit
        //   (a) updates state.config_draft (the Setup form reads
        //       it on next render),
        //   (b) sets config_save_status to "Saving…",
        //   (c) fires Command::SaveConfig for atomic TOML write.
        // The receiver side is tested separately by the lib;
        // here we verify the reducer's three side effects.
        use crossbeam_channel::unbounded;
        let (tx, rx) = unbounded::<Command>();
        let mut state = fresh_state();
        reduce(
            &mut state,
            Msg::UserEditedConfigField {
                kind: ConfigFieldKind::Crf,
                value: ConfigFieldValue::Crf(28),
            },
            &tx,
            &default_config(),
        );
        // (a) config_draft updated.
        let enc = state
            .config_draft
            .encode
            .as_ref()
            .expect("encode section should be auto-populated");
        assert_eq!(enc.crf, 28);
        // (b) save status set.
        assert_eq!(state.config_save_status, "Saving…");
        // (c) SaveConfig dispatched -- carrying the edited draft, not
        // the lib's startup copy. Without the payload the lib would
        // write back the config it loaded and the edit would be lost.
        let cmd = rx.try_recv().expect("expected a SaveConfig command");
        match cmd {
            Command::SaveConfig { config } => {
                assert_eq!(
                    config.encode.as_ref().map(|e| e.crf),
                    Some(28),
                    "the payload must carry the edit"
                );
            }
            _ => panic!("expected SaveConfig"),
        }
    }

    #[test]
    fn nv_edit_reaches_the_save_payload() {
        // The NVENC switch has to reach the lib, otherwise the toggle
        // is cosmetic.
        use crossbeam_channel::unbounded;
        let (tx, rx) = unbounded::<Command>();
        let mut state = fresh_state();
        reduce(
            &mut state,
            Msg::UserEditedConfigField {
                kind: ConfigFieldKind::Nv,
                value: ConfigFieldValue::Nv(true),
            },
            &tx,
            &default_config(),
        );
        match rx.try_recv().expect("expected a SaveConfig command") {
            Command::SaveConfig { config } => {
                assert_eq!(config.encode.as_ref().and_then(|e| e.nv), Some(true));
            }
            _ => panic!("expected SaveConfig"),
        }
    }

    #[test]
    fn user_edited_config_field_extensions_parses_csv() {
        // review #133 part 2d: Extensions edit parses "mp4, mkv, mov "
        // into ["mp4", "mkv", "mov"]. Empty string clears the filter
        // (= all). The apply_config_field_edit helper is the source of
        // truth for this parsing — verify it directly.
        let mut state = fresh_state();
        let tx = cmd_tx();
        reduce(
            &mut state,
            Msg::UserEditedConfigField {
                kind: ConfigFieldKind::Extensions,
                value: ConfigFieldValue::Extensions("mp4, mkv , mov".into()),
            },
            &tx,
            &default_config(),
        );
        let enc = state.config_draft.encode.as_ref().unwrap();
        assert_eq!(
            enc.extensions,
            Some(vec!["mp4".into(), "mkv".into(), "mov".into()])
        );

        reduce(
            &mut state,
            Msg::UserEditedConfigField {
                kind: ConfigFieldKind::Extensions,
                value: ConfigFieldValue::Extensions(String::new()),
            },
            &tx,
            &default_config(),
        );
        let enc = state.config_draft.encode.as_ref().unwrap();
        assert_eq!(enc.extensions, None, "empty csv should clear filter");
    }

    #[test]
    fn event_config_saved_updates_save_status_to_timestamp() {
        // review-delta F3: the Setup-page save-status indicator
        // must transition from 'Saving…' to 'Saved at HH:MM:SS'
        // when the lib acknowledges a successful save. The
        // previous empty handler left it stuck at 'Saving…'
        // forever, which the new test would have caught.
        use chrono::Local;
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.config_save_status = "Saving…".into();
        reduce(
            &mut state,
            Msg::Lib(Event::ConfigSaved {
                path: std::path::PathBuf::from("/tmp/rencoder.toml"),
            }),
            &tx,
            &default_config(),
        );
        let status = &state.config_save_status;
        // Format: "Saved at HH:MM:SS" — assert prefix + 8 chars
        // (the timestamp portion).
        assert!(
            status.starts_with("Saved at "),
            "expected 'Saved at ...' prefix, got {status:?}"
        );
        // Verify the timestamp portion parses as HH:MM:SS.
        let ts = status.strip_prefix("Saved at ").unwrap();
        let expected_now = Local::now().format("%H:%M:%S").to_string();
        // Allow ±1s tolerance for the boundary case where the
        // test crosses a second between the two `now()` calls.
        let drift_ok = ts == expected_now || ts == Local::now().format("%H:%M:%S").to_string();
        assert!(drift_ok, "timestamp {ts} != {expected_now}");
    }

    #[test]
    fn event_error_save_failed_updates_save_status() {
        // review-delta F1: the lib emits Event::Error with the
        // prefix 'save failed:' on Command::SaveConfig failure
        // (job.rs:1119). The UI must surface this on the
        // Setup-page save-status indicator (separate from the
        // global last_error banner).
        let mut state = fresh_state();
        let tx = cmd_tx();
        state.config_save_status = "Saving…".into();
        reduce(
            &mut state,
            Msg::Lib(Event::Error {
                category: rencoder_lib::types::ErrorCategory::ConfigError,
                message: "save failed: permission denied".into(),
            }),
            &tx,
            &default_config(),
        );
        assert_eq!(
            state.config_save_status, "Save failed: permission denied",
            "save-failure should overwrite the save-status indicator"
        );
        // last_error still set (global banner).
        assert_eq!(
            state.last_error.as_deref(),
            Some("save failed: permission denied")
        );
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
        let mut state = fresh_state();
        state.config_draft = cfg.clone();
        let params = build_encode_params_from_draft(&state);
        assert_eq!(params.format, rencoder_lib::types::VideoFormat::Hevc);
        assert!(params.is_crf);
        assert_eq!(params.bitrate_or_crf, 28);
    }

    #[test]
    fn build_encode_params_defaults_when_section_missing() {
        let cfg = AppConfig::default();
        let mut state = fresh_state();
        state.config_draft = cfg.clone();
        let params = build_encode_params_from_draft(&state);
        assert_eq!(params.format, rencoder_lib::types::VideoFormat::Av1);
        assert!(params.is_crf);
        assert_eq!(params.bitrate_or_crf, 45);
    }
    // extension_matches tests were removed in v1.2 follow-up #5 —
    // the helper was deleted when the UI started routing through
    // lib's `collect_files` (which handles extension matching
    // internally with walkdir).
}
