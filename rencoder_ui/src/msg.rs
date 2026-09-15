//! UI state + Msg type + reducer (U21 §3.8.7 "everything is msg").
//!
//! The view layer is purely a function of state; state changes
//! only via `reduce(state, msg, cmd_tx)` which also dispatches
//! Commands back to the lib actor as needed.

use std::collections::VecDeque;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use crossbeam_channel::Sender;

use rencoder_lib::job::{Command, EncodingRecord, Event};

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
    /// Dry-run results: populated by Preview (Command::DryRun) →
    /// Event::DryRunFinished. Cleared on each new dry-run.
    #[allow(dead_code)] // populated in #64; UI panel rendering land
    pub dry_run_results: Vec<EncodingRecord>,
    /// Currently selected input directory (set via start_view).
    pub input_dir: Option<PathBuf>,
    /// Last fatal error (if any).
    pub last_error: Option<String>,
    /// U22 / Tier 3 #9: current analyze batch progress, if any.
    /// `None` = no batch running (or never started). Set on
    /// `Event::AnalyzeStarted`; updated on each `Event::AnalyzeProgress`.
    /// Reducer does NOT validate `done` < `total` — trust the lib
    /// monotonically increasing counts (see docs/design.md §3.4
    /// "分析进度上报"). Cleared on next `AnalyzeStarted`.
    #[allow(dead_code)]
    pub analyze_progress: Option<AnalyzeProgressInfo>,
    /// U22: last analyze batch's records. Set on `Event::AnalyzeFinished`.
    /// **Overwrite** semantics — a new analyze batch replaces the
    /// previous one (like `state.history` behavior). User actions
    /// other than Analyze don't clear this.
    #[allow(dead_code)]
    pub analyze_results: Vec<rencoder_lib::analyzer::records::AnalysisRecord>,
}

/// U22: UI-side mirror of `Event::AnalyzeProgress`. Kept as a
/// separate struct (not a 1:1 alias) so the UI can evolve the
/// shape independently from the lib event payload if needed.
#[derive(Debug)]
#[allow(dead_code)] // populated in #73; UI panel reads it
pub struct AnalyzeProgressInfo {
    pub done: usize,
    pub total: usize,
    pub failed: usize,
    /// Last completed file in **completion order** (rayon workers
    /// race). The UI renders this as a "just completed: X" hint —
    /// NOT "next in input order" (per docs/design.md §3.4
    /// "最终结果的展示约定"). After the batch ends, this stays
    /// as the last file seen (not cleared) until the next
    /// `AnalyzeStarted` overwrites it.
    pub last: Option<rencoder_lib::analyzer::records::AnalyzedFile>,
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
    /// Save the loaded AppConfig back to its original path
    /// (ControllerHandle::config_path). Wired by Save button.
    UserClickedSaveSettings,
    /// U22 / Tier 3 #9: trigger a parallel analyze batch from the
    /// current `state.input_dir` (same file collection as
    /// `UserClickedStart` but dispatches `Command::Analyze` instead
    /// of `Command::StartBatch`). Wired by the Analyze button.
    UserClickedAnalyze,
    // UserClickedPreview — wired up in app.rs Preview button. The
    // variant is always constructed in production code; clippy's
    // dead_code lint triggers here because no test in this module
    // exercises the preview flow directly. Tests live in the UI
    // integration suite (manual / playwright).
    UserClickedPreview,
    Lib(Event),
}

/// Reduce a message into a state change; emit Commands to the lib
/// actor as needed. Returns the (possibly) updated state by
/// ownership since we may need to replace it entirely.
pub fn reduce(
    state: &mut AppState,
    msg: Msg,
    cmd_tx: &Sender<Command>,
    config: &rencoder_lib::config::app::AppConfig,
) {
    match msg {
        Msg::UserSelectedDir(path) => {
            state.input_dir = Some(path);
        }
        Msg::UserClickedStart => {
            let Some(dir) = state.input_dir.clone() else {
                state.last_error = Some("no input dir selected".into());
                return;
            };
            // Derive EncodeParams from loaded AppConfig (v1.1: no more
            // hardcoded defaults). Per `Config::from_encode_config` but
            // we need the FULL EncodeParams, not just the runtime
            // Config (which strips max_fps etc).
            let params = build_encode_params_from_config(config);
            // Collect files in the directory, filtered by config's
            // extensions if any (v1.1: extension filter).
            let extensions = config
                .encode
                .as_ref()
                .and_then(|e| e.extensions.clone())
                .unwrap_or_default();
            let files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_file())
                        .filter(|p| extension_matches(p, &extensions))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if files.is_empty() {
                let detail = if extensions.is_empty() {
                    format!("no files in {}", dir.display())
                } else {
                    format!(
                        "no files in {} matching extensions {:?}",
                        dir.display(),
                        extensions
                    )
                };
                state.last_error = Some(detail);
                return;
            }
            state.last_error = None;
            state.progress.clear();
            state.log.clear();
            let _ = cmd_tx.send(Command::StartBatch { params, files });
        }
        Msg::UserClickedPreview => {
            // Dry-run: clear previous results, build params from
            // config, dispatch Command::DryRun.
            state.dry_run_results.clear();
            state.last_error = None;
            let Some(dir) = state.input_dir.clone() else {
                state.last_error = Some("no input dir selected".into());
                return;
            };
            let params = build_encode_params_from_config(config);
            let extensions = config
                .encode
                .as_ref()
                .and_then(|e| e.extensions.clone())
                .unwrap_or_default();
            let files: Vec<_> = std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_file())
                        .filter(|p| extension_matches(p, &extensions))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if files.is_empty() {
                state.last_error = Some(format!("no files in {} for preview", dir.display()));
                return;
            }
            let _ = cmd_tx.send(Command::DryRun { params, files });
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
        Msg::UserClickedSaveSettings => {
            // Round-trip the in-memory AppConfig back to its
            // original path. The lib does the actual save; we just
            // dispatch. Result comes back as Event::ConfigSaved or
            // Event::Error (handled in apply_event).
            let _ = cmd_tx.send(Command::SaveConfig);
        }
        Msg::UserClickedAnalyze => {
            // U22 / Tier 3 #9: same file-collection logic as
            // UserClickedStart (read state.input_dir, apply
            // extension filter from config.encode.extensions). We
            // dispatch Command::Analyze instead of StartBatch —
            // read-only probe, no ffmpeg.
            //
            // File collection is duplicated from UserClickedStart
            // rather than extracted into a helper, because
            // extraction would couple the two flows (e.g. sharing
            // the empty-list error message); the duplication is
            // 10 lines and the two flows can evolve independently.
            state.dry_run_results.clear();
            state.last_error = None;
            let Some(dir) = state.input_dir.clone() else {
                state.last_error = Some("no input dir selected".into());
                return;
            };
            let extensions = config
                .encode
                .as_ref()
                .and_then(|e| e.extensions.clone())
                .unwrap_or_default();
            let files: Vec<_> = std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_file())
                        .filter(|p| extension_matches(p, &extensions))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if files.is_empty() {
                state.last_error = Some(format!("no files in {} for analyze", dir.display()));
                return;
            }
            // Clear stale state for the new batch. analyze_progress
            // gets reset on the next AnalyzeStarted event (the lib
            // is the source of truth for `total`); analyze_results
            // is overwritten on AnalyzeFinished (per task #73
            // description: keep until next analyze).
            let _ = cmd_tx.send(Command::Analyze { files });
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
        Event::DryRunFinished { records } => {
            state.dry_run_results = records;
        }
        Event::AnalyzeStarted { total } => {
            // U22: reset progress for the new batch. analyze_results
            // is preserved (overwrite happens on AnalyzeFinished, not
            // here — keeps the previous batch's table visible while
            // the new one is in flight, until the new Finished
            // arrives).
            state.analyze_progress = Some(AnalyzeProgressInfo {
                done: 0,
                total,
                failed: 0,
                last: None,
            });
        }
        Event::AnalyzeProgress {
            done,
            total,
            failed,
            last,
        } => {
            // U22: monotonic counts from the lib. We don't validate
            // (done <= total, etc.) — the lib is the source of truth.
            // The struct itself stays Some once Started fired; we
            // just overwrite the fields. This makes the latest
            // tick's `last` available to AnalyzePanel as a
            // "just completed: X" hint.
            state.analyze_progress = Some(AnalyzeProgressInfo {
                done,
                total,
                failed,
                last,
            });
        }
        Event::AnalyzeFinished {
            records,
            failed_count,
        } => {
            // U22: overwrite analyze_results (preserved until next
            // batch). Keep analyze_progress as-is so the panel can
            // still show "X/Y done" after the batch ends; the next
            // AnalyzeStarted will reset it.
            //
            // Cross-check: the lib sends failed_count ==
            // records.iter().filter(|r| r.fps.is_none()).count().
            // We don't assert it here (lib is the source of truth)
            // but the invariant is tested in
            // job::tests::analyze_emits_started_progress_finished_sequence.
            state.analyze_results = records;
            // Surface failed count via last_error if any failed,
            // so the user gets feedback even if the panel is
            // scrolled past. Otherwise clear last_error (it may
            // have been set by UserClickedAnalyze's empty-dir check).
            if failed_count > 0 {
                state.last_error = Some(format!(
                    "analyze done: {} files, {} failed",
                    state.analyze_results.len(),
                    failed_count
                ));
            } else {
                state.last_error = None;
            }
        }
        Event::ConfigSaved { path } => {
            // Show feedback ("saved to ...") for a few ticks. We
            // don't reset last_error — if Save raced a real error
            // we want the user to see both. UI decides display.
            state.last_error = Some(format!("saved to {}", path.display()));
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

/// Check whether a path's extension matches any of the user's
/// configured extension filters. Empty `extensions` means "all".
/// Match is case-insensitive and ignores the leading dot.
fn extension_matches(path: &std::path::Path, extensions: &[String]) -> bool {
    if extensions.is_empty() {
        return true;
    }
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext_lower = ext.to_ascii_lowercase();
    extensions.iter().any(|e| {
        let trimmed = e.trim_start_matches('.').to_ascii_lowercase();
        trimmed == ext_lower
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rencoder_lib::config::app::AppConfig;
    use rencoder_lib::types::{EncoderSettings, EncodingOutcome, ErrorContext};

    fn default_config() -> AppConfig {
        AppConfig::default()
    }

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
        reduce(
            &mut state,
            Msg::UserSelectedDir(p.clone()),
            &tx,
            &default_config(),
        );
        assert_eq!(state.input_dir, Some(p));
    }

    // U22 / Tier 3 #9: analyze reducer + Event handlers.
    //
    // Three contracts pinned:
    // 1. UserClickedAnalyze collects files from state.input_dir
    //    (filtered by config.encode.extensions) and dispatches
    //    Command::Analyze. Empty dir → last_error set, no dispatch.
    // 2. Event::AnalyzeProgress updates state.analyze_progress in
    //    place (monotonic counts from the lib are trusted as-is).
    // 3. Event::AnalyzeFinished REPLACES state.analyze_results
    //    (overwrite semantics — old batch replaced by new).

    #[test]
    fn user_clicked_analyze_without_dir_records_error() {
        let mut state = AppState::default();
        let tx = cmd_tx();
        reduce(&mut state, Msg::UserClickedAnalyze, &tx, &default_config());
        assert!(state.last_error.is_some());
        assert!(state.last_error.unwrap().contains("no input dir"));
    }

    #[test]
    fn analyze_progress_updates_state() {
        let mut state = AppState::default();
        // First the Started event sets the initial state.
        reduce(
            &mut state,
            Msg::Lib(Event::AnalyzeStarted { total: 10 }),
            &cmd_tx(),
            &default_config(),
        );
        assert!(state.analyze_progress.is_some());
        let info = state.analyze_progress.as_ref().unwrap();
        assert_eq!(info.done, 0);
        assert_eq!(info.total, 10);
        assert_eq!(info.failed, 0);
        assert!(info.last.is_none());

        // Then a Progress event updates counts and last.
        reduce(
            &mut state,
            Msg::Lib(Event::AnalyzeProgress {
                done: 5,
                total: 10,
                failed: 1,
                last: Some(rencoder_lib::analyzer::records::AnalyzedFile {
                    path: PathBuf::from("/x.mp4"),
                    outcome: rencoder_lib::analyzer::records::FileOutcome::Ok,
                }),
            }),
            &cmd_tx(),
            &default_config(),
        );
        let info = state.analyze_progress.as_ref().unwrap();
        assert_eq!(info.done, 5);
        assert_eq!(info.total, 10);
        assert_eq!(info.failed, 1);
        assert!(info.last.is_some());
        assert_eq!(info.last.as_ref().unwrap().path, PathBuf::from("/x.mp4"));
    }

    #[test]
    fn analyze_finished_replaces_results() {
        // Build state with a stale seed record (not via Default +
        // assignment, to satisfy `field_reassign_with_default`).
        let stale = rencoder_lib::analyzer::records::AnalysisRecord {
            path: PathBuf::from("/stale.mp4"),
            fps: Some(30),
            codec: Some("h264".into()),
            duration_secs: Some(1.0),
            file_bytes: 0,
        };
        let mut state = AppState {
            analyze_results: vec![stale],
            ..Default::default()
        };
        // Finished with new records overwrites.
        let new_records = vec![
            rencoder_lib::analyzer::records::AnalysisRecord {
                path: PathBuf::from("/new1.mp4"),
                fps: None, // failed
                codec: None,
                duration_secs: None,
                file_bytes: 0,
            },
            rencoder_lib::analyzer::records::AnalysisRecord {
                path: PathBuf::from("/new2.mp4"),
                fps: Some(60),
                codec: Some("av1".into()),
                duration_secs: Some(2.0),
                file_bytes: 100,
            },
        ];
        reduce(
            &mut state,
            Msg::Lib(Event::AnalyzeFinished {
                records: new_records.clone(),
                failed_count: 1,
            }),
            &cmd_tx(),
            &default_config(),
        );
        assert_eq!(state.analyze_results.len(), 2);
        assert_eq!(state.analyze_results[0].path, PathBuf::from("/new1.mp4"));
        assert_eq!(state.analyze_results[1].path, PathBuf::from("/new2.mp4"));
        // Stale record gone.
        let stale = PathBuf::from("/stale.mp4");
        assert!(state.analyze_results.iter().all(|r| r.path != stale));
        // last_error surfaces the failure count (1 failed).
        assert!(state.last_error.is_some());
        assert!(state.last_error.unwrap().contains("1 failed"));
    }

    #[test]
    fn user_clicked_start_without_dir_records_error() {
        let mut state = AppState::default();
        let tx = cmd_tx();
        reduce(&mut state, Msg::UserClickedStart, &tx, &default_config());
        assert!(state.last_error.is_some());
        assert!(state.batch.is_none());
    }

    #[test]
    fn batch_started_creates_batch_info() {
        let mut state = AppState::default();
        reduce(
            &mut state,
            Msg::Lib(Event::BatchStarted {
                id: 1,
                total_files: 5,
            }),
            &cmd_tx(),
            &default_config(),
        );
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
        reduce(
            &mut state,
            Msg::Lib(Event::BatchStarted {
                id: 1,
                total_files: 2,
            }),
            &tx,
            &default_config(),
        );
        reduce(
            &mut state,
            Msg::Lib(Event::BatchCompleted { records: vec![] }),
            &tx,
            &default_config(),
        );
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
            &default_config(),
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
                &default_config(),
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
    fn batch_cancelled_sets_cancelled_and_finished() {
        let mut state = AppState::default();
        let tx = cmd_tx();
        reduce(
            &mut state,
            Msg::Lib(Event::BatchStarted {
                id: 1,
                total_files: 5,
            }),
            &tx,
            &default_config(),
        );
        reduce(
            &mut state,
            Msg::Lib(Event::BatchCancelled {
                completed: 2,
                total: 5,
            }),
            &tx,
            &default_config(),
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
        reduce(
            &mut state,
            Msg::Lib(Event::BatchStarted {
                id: 1,
                total_files: 2,
            }),
            &tx,
            &default_config(),
        );
        let mut rec = sample_record();
        rec.source_path = "/first.mp4".into();
        reduce(
            &mut state,
            Msg::Lib(Event::FileCompleted {
                index: 0,
                record: rec.clone(),
            }),
            &tx,
            &default_config(),
        );
        assert_eq!(state.batch.as_ref().unwrap().records.len(), 1);
        assert_eq!(
            state.batch.as_ref().unwrap().records[0].source_path,
            "/first.mp4"
        );
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
            &default_config(),
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

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn build_encode_params_uses_config() {
        use rencoder_lib::config::app::{AppConfig, EncodeConfig, FormatArg};
        let mut cfg = AppConfig::default();
        cfg.encode = Some(EncodeConfig {
            format: Some(FormatArg::Hevc),
            crf: 28,
            ..Default::default()
        });
        let params = build_encode_params_from_config(&cfg);
        assert_eq!(params.format, rencoder_lib::types::VideoFormat::Hevc);
        assert!(params.is_crf);
        assert_eq!(params.bitrate_or_crf, 28);
    }

    #[test]
    fn build_encode_params_defaults_when_section_missing() {
        let cfg = AppConfig::default();
        let params = build_encode_params_from_config(&cfg);
        // Default format is AV1; default crf=45 → is_crf=true.
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
        // File with no extension is rejected when filter is non-empty.
        assert!(!extension_matches(&PathBuf::from("/foo"), &["mp4".into()]));
    }
}
