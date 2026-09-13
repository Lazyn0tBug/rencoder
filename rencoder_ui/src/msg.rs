//! UI state + Msg type + reducer (U21 §3.8.7 "everything is msg").
//!
//! The view layer is purely a function of state; state changes
//! only via `reduce(state, msg, cmd_tx)` which also dispatches
//! Commands back to the lib actor as needed.

use std::collections::VecDeque;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use crossbeam_channel::Sender;

use rencoder_lib::job::{Command, Event, EncodingRecord};

/// Cap on the stderr log ring buffer. Per §3.8.6 + §8 item 30.
const LOG_RING_CAP: usize = 1000;

/// Top-level UI state — single source of truth for all views.
#[allow(dead_code)] // fields are populated by the reducer; UI views read them
#[derive(Debug, Default)]
pub struct AppState {
    /// Currently running batch, if any.
    pub batch: Option<BatchInfo>,
    /// per-file progress keyed by file_index.
    pub progress: std::collections::HashMap<usize, ProgressInfo>,
    /// stderr log ring buffer (newest last).
    pub log: VecDeque<LogLine>,
    /// History loaded via LoadHistory command.
    pub history: Vec<EncodingRecord>,
    /// Currently selected input directory (set via start_view).
    pub input_dir: Option<PathBuf>,
    /// Last fatal error (if any).
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BatchInfo {
    pub id: u64,
    pub total_files: usize,
    /// Set to true once Cancel lands; subsequent Progress events ignored.
    pub cancelled: bool,
    /// Set to true once BatchCompleted or BatchCancelled fires.
    pub finished: bool,
    /// Records accumulated so far.
    pub records: Vec<EncodingRecord>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // fields populated by reducer, read by UI views (yet to be written)
pub struct ProgressInfo {
    pub frame: u64,
    pub fps: f64,
    pub speed: f64,
    pub time_secs: f64,
    pub eta_secs: Option<f64>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // fields populated by reducer, read by UI views (yet to be written)
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
    #[allow(dead_code)] // DirPicker component will produce this
    UserSelectedDir(PathBuf),
    UserClickedStart,
    UserClickedCancel,
    UserClickedLoadHistory,
    Lib(Event),
}

/// Reduce a message into a state change; emit Commands to the lib
/// actor as needed. Returns the (possibly) updated state by
/// ownership since we may need to replace it entirely.
pub fn reduce(state: &mut AppState, msg: Msg, cmd_tx: &Sender<Command>) {
    match msg {
        Msg::UserSelectedDir(path) => {
            state.input_dir = Some(path);
        }
        Msg::UserClickedStart => {
            let Some(dir) = state.input_dir.clone() else {
                state.last_error = Some("no input dir selected".into());
                return;
            };
            // v1: hardcoded AV1 / CRF 45. Real impl reads from AppConfig.
            let params = rencoder_lib::types::EncodeParams::default();
            // Stub file list — for now, the user is expected to have
            // placed files in the dir. Real impl: glob + filter.
            let files = std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_file())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if files.is_empty() {
                state.last_error = Some(format!("no files in {}", dir.display()));
                return;
            }
            state.last_error = None;
            state.progress.clear();
            state.log.clear();
            let _ = cmd_tx.send(Command::StartBatch { params, files });
        }
        Msg::UserClickedCancel => {
            let _ = cmd_tx.send(Command::Cancel);
        }
        Msg::UserClickedLoadHistory => {
            let _ = cmd_tx.send(Command::LoadHistory {
                limit: Some(100),
                after: None,
            });
        }
        Msg::Lib(evt) => apply_event(state, evt),
    }
}

fn apply_event(state: &mut AppState, evt: Event) {
    match evt {
        Event::BatchStarted { id, total_files } => {
            state.batch = Some(BatchInfo {
                id,
                total_files,
                cancelled: false,
                finished: false,
                records: Vec::with_capacity(total_files),
            });
        }
        Event::FileStarted { .. } => {
            // Could update a "current file" display; v1 just lets
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
            if let Some(batch) = &state.batch {
                if batch.finished || batch.cancelled {
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
        Event::FileCompleted { record, .. } => {
            // Move the record's outcome into BatchInfo.records for
            // post-batch display.
            if let Some(batch) = state.batch.as_mut() {
                batch.records.push(record);
            }
            state.progress.clear(); // simplified: clear per-file on completion
        }
        Event::BatchCompleted { records } => {
            if let Some(batch) = state.batch.as_mut() {
                batch.records = records;
                batch.finished = true;
            }
            state.progress.clear();
        }
        Event::BatchCancelled { completed, total } => {
            if let Some(batch) = state.batch.as_mut() {
                batch.cancelled = true;
                batch.finished = true;
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
        }
        // #[non_exhaustive] future variants — silently drop.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rencoder_lib::types::{EncoderSettings, EncodingOutcome, ErrorContext};

    fn sample_record() -> EncodingRecord {
        EncodingRecord {
            source_path: "/x.mp4".into(),
            source_size_bytes: 0,
            source_codec: None,
            source_container: None,
            source_width: None,
            source_height: None,
            source_fps: None,
            source_duration_secs: None,
            source_bit_rate_bps: None,
            started_at: chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
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

    fn cmd_tx() -> Sender<Command> {
        let (tx, _rx) = crossbeam_channel::unbounded::<Command>();
        tx
    }

    #[test]
    fn user_selected_dir_sets_input_dir() {
        let mut state = AppState::default();
        let tx = cmd_tx();
        let p = PathBuf::from("/some/dir");
        reduce(&mut state, Msg::UserSelectedDir(p.clone()), &tx);
        assert_eq!(state.input_dir, Some(p));
    }

    #[test]
    fn user_clicked_start_without_dir_records_error() {
        let mut state = AppState::default();
        let tx = cmd_tx();
        reduce(&mut state, Msg::UserClickedStart, &tx);
        assert!(state.last_error.is_some());
        assert!(state.batch.is_none());
    }

    #[test]
    fn batch_started_creates_batch_info() {
        let mut state = AppState::default();
        reduce(&mut state, Msg::Lib(Event::BatchStarted { id: 1, total_files: 5 }), &cmd_tx());
        let b = state.batch.as_ref().unwrap();
        assert_eq!(b.id, 1);
        assert_eq!(b.total_files, 5);
        assert!(!b.finished);
        assert!(!b.cancelled);
    }

    #[test]
    fn progress_event_after_finished_is_ignored() {
        let mut state = AppState::default();
        let tx = cmd_tx();
        reduce(&mut state, Msg::Lib(Event::BatchStarted { id: 1, total_files: 2 }), &tx);
        reduce(&mut state, Msg::Lib(Event::BatchCompleted { records: vec![] }), &tx);
        // Now feed a Progress — should be ignored because finished.
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
        );
        assert!(state.progress.is_empty());
    }

    #[test]
    fn log_ring_caps_at_1000() {
        let mut state = AppState::default();
        let tx = cmd_tx();
        for i in 0..1500 {
            reduce(
                &mut state,
                Msg::Lib(Event::LogLine {
                    file_index: 0,
                    line: format!("line {i}"),
                }),
                &tx,
            );
        }
        assert_eq!(state.log.len(), 1000);
        // The first 500 are dropped, so the oldest should be "line 500".
        let first = state.log.front().unwrap();
        assert_eq!(first.line, "line 500");
    }

    #[test]
    fn history_loaded_replaces_history() {
        let mut state = AppState::default();
        let tx = cmd_tx();
        let mut rec = sample_record();
        rec.source_path = "/a.mp4".into();
        reduce(
            &mut state,
            Msg::Lib(Event::HistoryLoaded { records: vec![rec.clone()] }),
            &tx,
        );
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].source_path, "/a.mp4");
    }

    #[test]
    fn batch_cancelled_sets_cancelled_and_finished() {
        let mut state = AppState::default();
        let tx = cmd_tx();
        reduce(&mut state, Msg::Lib(Event::BatchStarted { id: 1, total_files: 5 }), &tx);
        reduce(
            &mut state,
            Msg::Lib(Event::BatchCancelled { completed: 2, total: 5 }),
            &tx,
        );
        let b = state.batch.as_ref().unwrap();
        assert!(b.cancelled);
        assert!(b.finished);
        assert_eq!(state.last_error.as_deref(), Some("cancelled after 2/5"));
    }

    #[test]
    fn file_completed_appends_record_to_batch() {
        let mut state = AppState::default();
        let tx = cmd_tx();
        reduce(&mut state, Msg::Lib(Event::BatchStarted { id: 1, total_files: 2 }), &tx);
        let mut rec = sample_record();
        rec.source_path = "/first.mp4".into();
        reduce(
            &mut state,
            Msg::Lib(Event::FileCompleted { index: 0, record: rec.clone() }),
            &tx,
        );
        assert_eq!(state.batch.as_ref().unwrap().records.len(), 1);
        assert_eq!(state.batch.as_ref().unwrap().records[0].source_path, "/first.mp4");
    }

    #[test]
    fn error_event_sets_last_error() {
        let mut state = AppState::default();
        let tx = cmd_tx();
        reduce(
            &mut state,
            Msg::Lib(Event::Error {
                category: rencoder_lib::types::ErrorCategory::ConfigError,
                message: "ffmpeg not found".into(),
            }),
            &tx,
        );
        assert_eq!(state.last_error.as_deref(), Some("ffmpeg not found"));
    }

    #[test]
    fn error_context_construction_smoke() {
        // Smoke: ErrorContext is #[non_exhaustive] so we can't
        // struct-literal it from outside. This just ensures the
        // type is reachable from this crate (compile-time check).
        let _: Option<ErrorContext> = None;
    }
}
