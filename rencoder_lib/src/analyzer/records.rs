//! FPS analysis: probe each file, build per-file `AnalysisRecord`s,
//! then aggregate into `VideoStats`.
//!
//! Per-file failures are recorded with `fps`/`codec`/`duration_secs = None`
//! (only `path` and `file_bytes` set) so the resulting CSV has one row
//! per input file and downstream DuckDB/Polars consumers can `WHERE fps
//! IS NULL` to find failures.
//!
//! The aggregation in `aggregate_records` is plain Rust (fast, simple,
//! no API friction). A Polars `DataFrame` view of the same records is
//! exposed via `records_to_dataframe` for users who want to issue
//! Polars queries from the CLI or run their own analyses.

use std::path::{Path, PathBuf};

use polars::prelude::*;

use crate::data::extract::probe_video_metadata_auto;
use crate::error::EncodeError;
use crate::types::VideoStats;

/// One row per input file. Failed probes yield a row with `path` and
/// `file_bytes` set but metric fields `None`.
///
/// `#[non_exhaustive]` (U22 review-delta #5): this struct is part of
/// the lib's public surface — re-exported, carried by
/// `Event::AnalyzeFinished.records`. The `non_exhaustive` attribute
/// prevents cross-crate struct-expression construction (even with
/// `..Default::default()` — that's only allowed within the defining
/// crate). External consumers wanting to build `AnalysisRecord`
/// values (e.g. UI tests seeding state) must use
/// [`AnalysisRecord::new`]. Matches the protection on `Command` /
/// `Event` / `FileOutcome` (see docs/design.md §8 item 33).
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct AnalysisRecord {
    pub path: PathBuf,
    pub fps: Option<u32>,
    pub codec: Option<String>,
    pub duration_secs: Option<f64>,
    pub file_bytes: u64,
}

impl AnalysisRecord {
    /// Construct an `AnalysisRecord` from outside the defining
    /// crate. Required because `#[non_exhaustive]` blocks
    /// cross-crate struct-expression syntax.
    ///
    /// `path` and `file_bytes` are always set; the metric fields
    /// (`fps` / `codec` / `duration_secs`) are `None` for failed
    /// probes (matching the per-file failure contract of
    /// `analyze_records` and the parallel variants). The lib's own
    /// probe paths build the struct directly without going
    /// through `new`; this constructor is the supported entry
    /// point for downstream (UI tests, future consumers).
    pub fn new(
        path: PathBuf,
        fps: Option<u32>,
        codec: Option<String>,
        duration_secs: Option<f64>,
        file_bytes: u64,
    ) -> Self {
        Self {
            path,
            fps,
            codec,
            duration_secs,
            file_bytes,
        }
    }
}

/// U22 / Tier 3 #9: per-file progress callback payload for
/// [`analyze_records_with_probe_and_progress_parallel`].
///
/// **`done` / `total` / `failed` are monotonic counters**, not per-file
/// indexes. The closure is called once per file completion; the values
/// reflect the global state at that moment, not just "this file's"
/// result. `last` carries the path + outcome of the file that just
/// completed, so the closure can correlate the counter increment with
/// the specific file (useful for emitting a single event per file with
/// a `last` payload).
///
/// Rayon worker order is non-deterministic: the closure may receive
/// `done = 7` while a `last` for an earlier-input-order file hasn't
/// been reported yet. Callers must not assume input-order delivery.
#[cfg(feature = "parallel-analyze")]
#[derive(Debug)]
pub struct AnalyzeProgress {
    pub done: usize,
    pub total: usize,
    pub failed: usize,
    pub last: Option<AnalyzedFile>,
}

/// U22: companion to [`AnalyzeProgress`]. Carries the path + outcome
/// of one file's probe result. **New type, not `AnalysisRecord`** --
/// `AnalysisRecord` doesn't have a `ProbeFailed` outcome field, and
/// adding `error: Option<EncodeError>` would pollute the success path
/// (every successful record would carry a `None` error field).
///
/// No `Clone` / `PartialEq` because `FileOutcome` (and through it
/// `EncodeError`) doesn't implement them. Debug is sufficient for
/// event payloads and UI display.
#[cfg(feature = "parallel-analyze")]
#[derive(Debug)]
pub struct AnalyzedFile {
    pub path: PathBuf,
    pub outcome: FileOutcome,
}

/// U22: outcome of one file's probe. Two variants -- `Ok` for
/// successful probe, `ProbeFailed` carrying the `EncodeError` from
/// the probe backend (c-probe FFI Err or ffprobe CLI Err).
///
/// `#[non_exhaustive]` so adding a new outcome (e.g. `Cancelled` if
/// v2 adds analyze-cancel support) doesn't break external match arms.
///
/// No `Clone` / `PartialEq` because `EncodeError` is thiserror-based
/// (no derives by default). The variant is opaque to UI; Debug is
/// sufficient for log lines and error display.
#[cfg(feature = "parallel-analyze")]
#[derive(Debug)]
#[non_exhaustive]
pub enum FileOutcome {
    Ok,
    ProbeFailed(EncodeError),
}

/// Probe every file and emit one `AnalysisRecord` per input.
///
/// **U19**: this function is now a pure probe -- no scaffold logs.
/// Lifecycle logs (`task N/M: probing / ok / failed`) live in
/// [`crate::pipeline::task::TaskManager::run`]; the CLI's
/// `analyze_cmd` calls `TaskManager::new(None).run(...)` to drive the
/// serial probe loop with scaffold logs.
pub fn analyze_records(files: &[PathBuf]) -> Result<Vec<AnalysisRecord>, EncodeError> {
    Ok(analyze_records_with_probe(files, probe_video_metadata_auto))
}

/// Test seam: same as `analyze_records` but with an injectable probe.
/// Pure probe (U19) -- emits no logs of its own.
pub fn analyze_records_with_probe<F>(files: &[PathBuf], probe: F) -> Vec<AnalysisRecord>
where
    F: Fn(&Path) -> Result<crate::types::VideoMetadata, EncodeError>,
{
    let mut records = Vec::with_capacity(files.len());
    for path in files {
        let file_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        match probe(path) {
            Ok(metadata) => {
                let fps = metadata.best_fps().map(|f| f.round() as u32);
                records.push(AnalysisRecord {
                    path: path.clone(),
                    fps,
                    codec: metadata.codec,
                    duration_secs: metadata.duration_secs,
                    file_bytes,
                });
            }
            Err(_) => {
                records.push(AnalysisRecord {
                    path: path.clone(),
                    fps: None,
                    codec: None,
                    duration_secs: None,
                    file_bytes,
                });
            }
        }
    }
    records
}

/// Parallel variant of `analyze_records` using `rayon`. Each file is
/// probed concurrently; per-thread probe failures become a record with
/// `None` metric fields (matching the serial contract). Output ordering
/// matches input ordering (rayon's `par_iter().collect()` is order-
/// preserving when collected into a Vec).
///
/// Probe failures inside a worker thread can't be propagated as `Result`
/// through `par_iter().collect()` without `try_reduce` gymnastics, so we
/// log + downgrade to a None-metric row, matching the serial behavior.
///
/// **No per-task log lines here**: rayon's parallel iteration runs
/// across worker threads with no guaranteed ordering, so emitting
/// `task 3/8: ...` would be misleading. Per-task logging is the
/// serial path's responsibility.
#[cfg(feature = "parallel-analyze")]
pub fn analyze_records_parallel(files: &[PathBuf]) -> Result<Vec<AnalysisRecord>, EncodeError> {
    use rayon::prelude::*;

    let records: Vec<AnalysisRecord> = files
        .par_iter()
        .map(|path| {
            let file_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            match probe_video_metadata_auto(path) {
                Ok(metadata) => AnalysisRecord {
                    path: path.clone(),
                    fps: metadata.best_fps().map(|f| f.round() as u32),
                    codec: metadata.codec,
                    duration_secs: metadata.duration_secs,
                    file_bytes,
                },
                Err(e) => {
                    log::warn!("probe failed for {}: {e}", path.display());
                    AnalysisRecord {
                        path: path.clone(),
                        fps: None,
                        codec: None,
                        duration_secs: None,
                        file_bytes,
                    }
                }
            }
        })
        .collect();

    Ok(records)
}

/// Test seam for the parallel variant: same shape as
/// `analyze_records_with_probe` but rayon-backed.
#[cfg(feature = "parallel-analyze")]
pub fn analyze_records_with_probe_parallel<F>(files: &[PathBuf], probe: F) -> Vec<AnalysisRecord>
where
    F: Fn(&Path) -> Result<crate::types::VideoMetadata, EncodeError> + Sync + Send,
{
    use rayon::prelude::*;

    files
        .par_iter()
        .map(|path| {
            let file_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            match probe(path) {
                Ok(metadata) => AnalysisRecord {
                    path: path.clone(),
                    fps: metadata.best_fps().map(|f| f.round() as u32),
                    codec: metadata.codec,
                    duration_secs: metadata.duration_secs,
                    file_bytes,
                },
                Err(e) => {
                    log::warn!("probe failed for {}: {e}", path.display());
                    AnalysisRecord {
                        path: path.clone(),
                        fps: None,
                        codec: None,
                        duration_secs: None,
                        file_bytes,
                    }
                }
            }
        })
        .collect()
}

/// U22 / Tier 3 #9: parallel probe with per-file progress callback.
///
/// Same shape as [`analyze_records_with_probe_parallel`], but the
/// caller injects a second closure that fires once per file
/// completion with an [`AnalyzeProgress`] payload. Used by the lib
/// actor to push `Event::AnalyzeProgress` to the UI's mpsc channel as
/// each rayon worker finishes a file.
///
/// **Closure contract**:
/// - `probe` is `Fn + Sync + Send` (rayon worker thread safety).
/// - `progress` is `Fn(AnalyzeProgress) + Sync + Send` (same).
///   Called once per file at the END of the worker's task (success or
///   failure path). `AnalyzeProgress.last` carries the just-finished
///   file's `AnalyzedFile` so the closure knows which file triggered
///   the tick.
/// - `progress` may be called from any rayon worker thread, in
///   non-deterministic order. **The `done` counter is the only
///   ordering guarantee** -- it monotonically increases.
/// - `progress` is fire-and-forget; the closure is responsible for
///   any throttling or buffering (the lib does not chunk).
///
/// **Failure semantics**: `probe` Err is downgraded to a record with
/// `None` metric fields (matching serial contract). The `failed`
/// counter is incremented by 1 for each Err. **One file = one
/// failure max**; there is no retry logic and a file is processed
/// exactly once.
///
/// **Output ordering**: `rayon::par_iter().collect()` preserves input
/// order, so the returned `Vec<AnalysisRecord>` matches the input
/// `files` slice order even though completion events were
/// non-deterministic.
#[cfg(feature = "parallel-analyze")]
pub fn analyze_records_with_probe_and_progress_parallel<F, P>(
    files: &[PathBuf],
    probe: F,
    progress: P,
) -> Vec<AnalysisRecord>
where
    F: Fn(&Path) -> Result<crate::types::VideoMetadata, EncodeError> + Sync + Send,
    P: Fn(AnalyzeProgress) + Sync + Send,
{
    use rayon::prelude::*;

    let total = files.len();
    // AtomicUsize lets workers update the counter without holding a
    // mutex across the probe call. The counter is read once at the
    // start of the progress callback to give the closure a stable
    // snapshot of "what's done now".
    use std::sync::atomic::{AtomicUsize, Ordering};
    let done = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);

    files
        .par_iter()
        .map(|path| {
            let file_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let (record, last) = match probe(path) {
                Ok(metadata) => {
                    let last = AnalyzedFile {
                        path: path.clone(),
                        outcome: FileOutcome::Ok,
                    };
                    (
                        AnalysisRecord {
                            path: path.clone(),
                            fps: metadata.best_fps().map(|f| f.round() as u32),
                            codec: metadata.codec,
                            duration_secs: metadata.duration_secs,
                            file_bytes,
                        },
                        last,
                    )
                }
                Err(e) => {
                    // Per-file failure log: same shape as the no-progress
                    // parallel variant (`log::warn!` for the operator's
                    // stderr; LogPanel surfaces it in the UI).
                    log::warn!("probe failed for {}: {e}", path.display());
                    let last = AnalyzedFile {
                        path: path.clone(),
                        outcome: FileOutcome::ProbeFailed(e),
                    };
                    (
                        AnalysisRecord {
                            path: path.clone(),
                            fps: None,
                            codec: None,
                            duration_secs: None,
                            file_bytes,
                        },
                        last,
                    )
                }
            };

            // Increment counters BEFORE invoking the progress closure
            // so the closure sees the updated snapshot (it represents
            // "this file is now done"). Order: failed first, then done,
            // so `done >= failed` always holds.
            if matches!(last.outcome, FileOutcome::ProbeFailed(_)) {
                failed.fetch_add(1, Ordering::SeqCst);
            }
            let done_now = done.fetch_add(1, Ordering::SeqCst) + 1;

            progress(AnalyzeProgress {
                done: done_now,
                total,
                failed: failed.load(Ordering::SeqCst),
                last: Some(last),
            });

            record
        })
        .collect()
}

/// Build a Polars DataFrame from records. Columns: `path`, `fps` (UInt32
/// nullable), `codec` (str nullable), `duration_secs` (Float64
/// nullable), `file_bytes` (UInt64). Available for downstream queries.
pub fn records_to_dataframe(records: &[AnalysisRecord]) -> PolarsResult<DataFrame> {
    // `to_string_lossy` (not `to_str`) keeps non-UTF-8 path bytes visible
    // as the Unicode replacement char so downstream tools at least see
    // *something* instead of an empty path field.
    let paths: Vec<String> = records
        .iter()
        .map(|r| r.path.to_string_lossy().into_owned())
        .collect();
    let fps: Vec<Option<u32>> = records.iter().map(|r| r.fps).collect();
    let codecs: Vec<Option<String>> = records.iter().map(|r| r.codec.clone()).collect();
    let durations: Vec<Option<f64>> = records.iter().map(|r| r.duration_secs).collect();
    let bytes: Vec<u64> = records.iter().map(|r| r.file_bytes).collect();

    df! {
        "path" => paths,
        "fps" => fps,
        "codec" => codecs,
        "duration_secs" => durations,
        "file_bytes" => bytes,
    }
}

/// Render records as CSV: header row, then one row per record with
/// proper quoting for fields containing commas, quotes, or newlines.
pub fn records_to_csv(records: &[AnalysisRecord]) -> String {
    let mut out = String::new();
    out.push_str("path,fps,codec,duration_secs,file_bytes\n");
    for r in records {
        out.push_str(&csv_escape_field(&r.path.to_string_lossy()));
        out.push(',');
        out.push_str(&r.fps.map(|f| f.to_string()).unwrap_or_default());
        out.push(',');
        out.push_str(&csv_escape_field(r.codec.as_deref().unwrap_or("")));
        out.push(',');
        out.push_str(&r.duration_secs.map(|d| format!("{d}")).unwrap_or_default());
        out.push(',');
        out.push_str(&r.file_bytes.to_string());
        out.push('\n');
    }
    out
}

fn csv_escape_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Aggregate an already-built record list into `VideoStats` using
/// Polars expressions: `group_by([fps, codec]).agg([count, sum(duration),
/// sum(bytes)])` plus manual totals (parsed/failed counts and the
/// failed-files list).
pub fn aggregate_records(records: &[AnalysisRecord]) -> VideoStats {
    // Single pass: parsed/failed counts, total duration, failed paths.
    let mut parsed = 0usize;
    let mut total_duration = 0f64;
    let mut failed_paths: Vec<PathBuf> = Vec::new();
    for r in records {
        if r.fps.is_some() {
            parsed += 1;
            if let Some(d) = r.duration_secs {
                total_duration += d;
            }
        } else {
            failed_paths.push(r.path.clone());
        }
    }
    let total_files = records.len();
    let failed = total_files - parsed;

    // Cross-tab aggregation via Polars expressions. Failed probes
    // (fps = None) are filtered out -- they're tracked in failed_files
    // and failed_paths instead.
    let table = if records.is_empty() {
        VideoStats::new(0).table
    } else {
        records_to_dataframe(records)
            .expect("records_to_dataframe")
            .lazy()
            .filter(col("fps").is_not_null())
            .group_by([col("fps"), col("codec")])
            .agg([
                len().alias("files"),
                col("duration_secs").sum().alias("duration_secs"),
                col("file_bytes").sum().alias("bytes"),
            ])
            .sort(["fps", "codec"], Default::default())
            .collect()
            .expect("polars aggregate")
    };

    VideoStats {
        table,
        total_files,
        parsed_files: parsed,
        failed_files: failed,
        total_duration_secs: total_duration,
        failed_paths,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::VideoMetadata;
    use pretty_assertions::assert_eq;

    fn h264(fps: f64) -> AnalysisRecord {
        AnalysisRecord {
            path: PathBuf::from("/fake.mp4"),
            fps: Some(fps.round() as u32),
            codec: Some("h264".into()),
            duration_secs: Some(60.0),
            file_bytes: 1024,
        }
    }

    #[test]
    fn records_to_csv_round_trip() {
        let records = vec![
            AnalysisRecord {
                path: PathBuf::from("/a.mp4"),
                fps: Some(30),
                codec: Some("h264".into()),
                duration_secs: Some(1.5),
                file_bytes: 100,
            },
            AnalysisRecord {
                path: PathBuf::from("/b,with,commas.mp4"),
                fps: None,
                codec: None,
                duration_secs: None,
                file_bytes: 0,
            },
        ];
        let csv = records_to_csv(&records);
        assert!(csv.starts_with("path,fps,codec,duration_secs,file_bytes\n"));
        assert!(csv.contains("/a.mp4,30,h264,1.5,100\n"));
        // Commas in path get quoted.
        assert!(csv.contains("\"/b,with,commas.mp4\",,,"));
    }

    #[test]
    fn records_to_csv_escapes_quotes_and_newlines() {
        let records = vec![AnalysisRecord {
            path: PathBuf::from("/odd\"path\n.mp4"),
            fps: Some(24),
            codec: Some("av1".into()),
            duration_secs: Some(0.0),
            file_bytes: 0,
        }];
        let csv = records_to_csv(&records);
        assert!(csv.contains("\"/odd\"\"path\n.mp4\""));
    }

    #[test]
    fn aggregate_records_groups_by_fps_and_codec() {
        let records = vec![h264(24.0), h264(30.0), h264(30.0), h264(60.0)];
        let stats = aggregate_records(&records);
        assert_eq!(stats.parsed_files, 4);
        assert_eq!(stats.failed_files, 0);
        // 3 distinct fps values, each with one (fps, codec) combo.
        assert_eq!(stats.table.height(), 3);
        // Total duration: 4 files * 60s = 240s.
        assert!((stats.total_duration_secs - 240.0).abs() < 1e-9);
    }

    #[test]
    fn aggregate_records_handles_failed_probes() {
        let mut recs = vec![h264(30.0), h264(30.0)];
        recs.push(AnalysisRecord {
            path: PathBuf::from("/bad.mp4"),
            fps: None,
            codec: None,
            duration_secs: None,
            file_bytes: 0,
        });
        let stats = aggregate_records(&recs);
        assert_eq!(stats.total_files, 3);
        assert_eq!(stats.parsed_files, 2);
        assert_eq!(stats.failed_files, 1);
        // Only one row in the table (the (30, h264) bucket); the failed
        // probe is excluded from the cross-tab.
        assert_eq!(stats.table.height(), 1);
        assert_eq!(stats.failed_paths, vec![PathBuf::from("/bad.mp4")]);
    }

    #[test]
    fn aggregate_records_tracks_codec_distribution() {
        let records = vec![
            AnalysisRecord {
                path: PathBuf::from("/a"),
                fps: Some(30),
                codec: Some("h264".into()),
                duration_secs: Some(60.0),
                file_bytes: 100,
            },
            AnalysisRecord {
                path: PathBuf::from("/b"),
                fps: Some(30),
                codec: Some("h264".into()),
                duration_secs: Some(30.0),
                file_bytes: 100,
            },
            AnalysisRecord {
                path: PathBuf::from("/c"),
                fps: Some(60),
                codec: Some("vp9".into()),
                duration_secs: Some(120.0),
                file_bytes: 100,
            },
        ];
        let stats = aggregate_records(&records);
        // Three distinct (fps, codec) combinations: (30, h264), (60, vp9).
        // (30, h264) appears once with files=2.
        let table_str = format!("{}", stats.table);
        assert!(table_str.contains("h264"));
        assert!(table_str.contains("vp9"));
        assert!((stats.total_duration_secs - 210.0).abs() < 1e-9);
    }

    #[test]
    fn analyze_records_with_probe_seam() {
        let files = vec![PathBuf::from("/x.mp4"), PathBuf::from("/y.mp4")];
        let probe = |path: &Path| -> Result<VideoMetadata, EncodeError> {
            if path.file_name().unwrap().to_str().unwrap() == "x.mp4" {
                Ok(VideoMetadata {
                    codec: Some("h264".into()),
                    avg_fps: Some(30.0),
                    duration_secs: Some(60.0),
                    ..Default::default()
                })
            } else {
                Err(EncodeError::Probe("canned".into()))
            }
        };
        let records = analyze_records_with_probe(&files, probe);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].fps, Some(30));
        assert_eq!(records[0].duration_secs, Some(60.0));
        assert!(records[1].fps.is_none());
        assert!(records[1].codec.is_none());
    }

    #[test]
    fn records_to_dataframe_builds_polars_df() {
        let records = vec![
            AnalysisRecord {
                path: PathBuf::from("/a.mp4"),
                fps: Some(30),
                codec: Some("h264".into()),
                duration_secs: Some(1.0),
                file_bytes: 100,
            },
            AnalysisRecord {
                path: PathBuf::from("/b.mp4"),
                fps: None,
                codec: None,
                duration_secs: None,
                file_bytes: 0,
            },
        ];
        let df = records_to_dataframe(&records).expect("df build");
        assert_eq!(df.height(), 2);
        assert_eq!(df.width(), 5);
        let fps_col = df.column("fps").unwrap();
        assert_eq!(fps_col.len(), 2);
    }

    #[test]
    fn aggregate_records_composes_with_probe_seam() {
        // U20: replaces the deleted `analyze_fps` smoke test. The
        // composition `aggregate_records(analyze_records_with_probe(...))`
        // is what callers build themselves now that the wrapper is gone.
        let files = vec![PathBuf::from("/x.mp4")];
        let probe = |_: &Path| -> Result<VideoMetadata, EncodeError> {
            Ok(VideoMetadata {
                codec: Some("vp9".into()),
                avg_fps: Some(60.0),
                duration_secs: Some(10.0),
                ..Default::default()
            })
        };
        let records = analyze_records_with_probe(&files, probe);
        let stats = aggregate_records(&records);
        assert_eq!(stats.parsed_files, 1);
        let table_str = format!("{}", stats.table);
        assert!(table_str.contains("vp9"));
        assert_eq!(stats.table.height(), 1);
    }

    // ---- parallel variant (only compiled with the feature) ----

    #[cfg(feature = "parallel-analyze")]
    #[test]
    fn parallel_output_matches_serial_sorted_by_path() {
        let files = vec![
            PathBuf::from("/a.mp4"),
            PathBuf::from("/b.mp4"),
            PathBuf::from("/c.mp4"),
            PathBuf::from("/d.mp4"),
        ];
        let probe = |path: &Path| -> Result<VideoMetadata, EncodeError> {
            let fps = match path.file_name().unwrap().to_str().unwrap() {
                "a.mp4" => 24.0,
                "b.mp4" => 30.0,
                "c.mp4" => 60.0,
                "d.mp4" => 30.0,
                _ => 0.0,
            };
            Ok(VideoMetadata {
                codec: Some("h264".into()),
                avg_fps: Some(fps),
                duration_secs: Some(60.0),
                ..Default::default()
            })
        };
        let mut serial = analyze_records_with_probe(&files, probe);
        let mut parallel = analyze_records_with_probe_parallel(&files, probe);

        // par_iter() preserves order, but compare sorted to make the
        // assertion robust against future rayon changes. pretty_assertions
        // gives a field-by-field diff when this fails -- useful since
        // AnalysisRecord has 5 fields.
        serial.sort_by_key(|r| r.path.clone());
        parallel.sort_by_key(|r| r.path.clone());
        assert_eq!(serial, parallel);
    }

    #[cfg(feature = "parallel-analyze")]
    #[test]
    fn parallel_failure_becomes_none_row_not_error() {
        let files = vec![
            PathBuf::from("/ok.mp4"),
            PathBuf::from("/bad.mp4"),
            PathBuf::from("/ok2.mp4"),
        ];
        let probe = |path: &Path| -> Result<VideoMetadata, EncodeError> {
            if path.file_name().unwrap().to_str().unwrap() == "bad.mp4" {
                Err(EncodeError::Probe("canned".into()))
            } else {
                Ok(VideoMetadata {
                    codec: Some("h264".into()),
                    avg_fps: Some(30.0),
                    duration_secs: Some(10.0),
                    ..Default::default()
                })
            }
        };
        let records = analyze_records_with_probe_parallel(&files, probe);
        assert_eq!(records.len(), 3);
        let parsed: Vec<_> = records.iter().filter(|r| r.fps.is_some()).collect();
        let failed: Vec<_> = records.iter().filter(|r| r.fps.is_none()).collect();
        assert_eq!(parsed.len(), 2);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].path, PathBuf::from("/bad.mp4"));
    }

    #[cfg(feature = "parallel-analyze")]
    #[test]
    fn parallel_aggregate_matches_serial() {
        let files = vec![
            PathBuf::from("/x.mp4"),
            PathBuf::from("/y.mp4"),
            PathBuf::from("/z.mp4"),
        ];
        let probe = |path: &Path| -> Result<VideoMetadata, EncodeError> {
            let (fps, codec) = match path.file_name().unwrap().to_str().unwrap() {
                "x.mp4" => (24.0, "h264"),
                "y.mp4" => (30.0, "vp9"),
                "z.mp4" => (60.0, "av1"),
                _ => (0.0, "h264"),
            };
            Ok(VideoMetadata {
                codec: Some(codec.into()),
                avg_fps: Some(fps),
                duration_secs: Some(60.0),
                ..Default::default()
            })
        };

        // Run twice with the same probe -- once serially, once parallel.
        // The aggregated Polars table (sorted by fps) should match.
        let serial = aggregate_records(&analyze_records_with_probe(&files, probe));
        let parallel = aggregate_records(&analyze_records_with_probe_parallel(&files, probe));

        assert_eq!(serial.total_files, parallel.total_files);
        assert_eq!(serial.parsed_files, parallel.parsed_files);
        assert_eq!(serial.failed_files, parallel.failed_files);
        assert_eq!(serial.table.height(), parallel.table.height());
        assert!((serial.total_duration_secs - parallel.total_duration_secs).abs() < 1e-9);
    }

    #[cfg(feature = "parallel-analyze")]
    #[test]
    fn parallel_returns_records_in_input_order() {
        let files = vec![
            PathBuf::from("/01.mp4"),
            PathBuf::from("/02.mp4"),
            PathBuf::from("/03.mp4"),
            PathBuf::from("/04.mp4"),
            PathBuf::from("/05.mp4"),
        ];
        let probe = |path: &Path| -> Result<VideoMetadata, EncodeError> {
            let _ = path; // not used; same fps for all
            Ok(VideoMetadata {
                codec: Some("h264".into()),
                avg_fps: Some(30.0),
                ..Default::default()
            })
        };
        let records = analyze_records_with_probe_parallel(&files, probe);
        let names: Vec<_> = records
            .iter()
            .map(|r| r.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["01.mp4", "02.mp4", "03.mp4", "04.mp4", "05.mp4",]
        );
    }

    // U22 / Tier 3 #9: tests for analyze_records_with_probe_and_progress_parallel.
    //
    // Three behaviors pinned:
    // 1. progress closure called once per file (n ticks for n files).
    // 2. failed count is correct for mixed Ok/Err input.
    // 3. done counter is monotonic non-decreasing across all ticks
    //    (rayon workers may deliver ticks out of order, but `done` is
    //    atomic — every tick sees a value >= every prior tick).

    #[cfg(feature = "parallel-analyze")]
    fn ok_probe(_path: &Path) -> Result<VideoMetadata, EncodeError> {
        Ok(VideoMetadata {
            codec: Some("h264".into()),
            avg_fps: Some(30.0),
            ..Default::default()
        })
    }

    #[cfg(feature = "parallel-analyze")]
    #[test]
    fn progress_called_once_per_file() {
        use std::sync::Mutex;
        let files: Vec<PathBuf> = (0..100)
            .map(|i| PathBuf::from(format!("/f/{i:03}.mp4")))
            .collect();
        let ticks: Mutex<Vec<AnalyzeProgress>> = Mutex::new(Vec::new());
        let probe = ok_probe;
        let progress = |p: AnalyzeProgress| {
            ticks.lock().unwrap().push(p);
        };
        let _ = analyze_records_with_probe_and_progress_parallel(&files, probe, progress);
        let ticks = ticks.into_inner().unwrap();
        assert_eq!(ticks.len(), 100, "one tick per file");
        // Every tick's `last` is Some and `total` is the file count.
        for t in &ticks {
            assert_eq!(t.total, 100);
            assert!(t.last.is_some(), "last should be populated per file");
            assert!(t.failed == 0);
        }
        // Max `done` across all ticks equals total. We can't use
        // `ticks.last()` because Vec order = lock-acquisition order,
        // not worker completion order (rayon workers race).
        let max_done = ticks.iter().map(|t| t.done).max().unwrap();
        assert_eq!(max_done, 100);
    }

    #[cfg(feature = "parallel-analyze")]
    #[test]
    fn progress_failed_count_correct_for_mixed_input() {
        use std::sync::Mutex;
        let files: Vec<PathBuf> = (0..10)
            .map(|i| PathBuf::from(format!("/f/{i:02}.mp4")))
            .collect();
        // Fail on files whose name ends in "even" index (0, 2, 4, 6, 8).
        let probe = |path: &Path| -> Result<VideoMetadata, EncodeError> {
            let name = path.file_name().unwrap().to_string_lossy();
            // Index embedded in the filename "00.mp4".."09.mp4".
            let idx: usize = name
                .trim_start_matches('0')
                .trim_end_matches(".mp4")
                .parse()
                .unwrap_or(0);
            if idx.is_multiple_of(2) {
                Err(EncodeError::Probe(format!("mock fail: {name}")))
            } else {
                Ok(VideoMetadata::default())
            }
        };
        let ticks: Mutex<Vec<AnalyzeProgress>> = Mutex::new(Vec::new());
        let progress = |p: AnalyzeProgress| {
            ticks.lock().unwrap().push(p);
        };
        let _ = analyze_records_with_probe_and_progress_parallel(&files, probe, progress);
        let ticks = ticks.into_inner().unwrap();
        assert_eq!(ticks.len(), 10);
        // Note: we can't read `ticks.last()` for done/failed because
        // Vec order = lock-acquisition order, NOT worker completion
        // order. The worker that produced `done_now = 10` may have
        // pushed earlier and another worker (with a smaller done_now)
        // pushed later. Verify the max value across all ticks instead.
        let max_done = ticks.iter().map(|t| t.done).max().unwrap();
        let max_failed = ticks.iter().map(|t| t.failed).max().unwrap();
        assert_eq!(max_done, 10, "final done count = 10");
        assert_eq!(max_failed, 5, "final failed count = 5 (indices 0,2,4,6,8)");

        // U22 review-delta #7: assert the per-tick `last` field
        // carries FileOutcome::ProbeFailed for failed ticks (and
        // Ok for successful ones), and the path matches one of
        // the input paths. Without this, a regression that
        // silently dropped the Err variant or pointed `last` at
        // the wrong file wouldn't be caught.
        let input_paths: std::collections::HashSet<PathBuf> = files.iter().cloned().collect();
        for tick in &ticks {
            let last = tick.last.as_ref().expect("last is Some per tick");
            // The path is one of the input paths.
            assert!(
                input_paths.contains(&last.path),
                "tick.last.path {:?} not in input files",
                last.path
            );
            // The outcome matches: even-indexed files (0, 2, 4, 6, 8)
            // failed, odd-indexed succeeded. The name pattern is
            // "/f/{idx:02}.mp4" — recover idx from the path.
            let name = last.path.file_name().unwrap().to_string_lossy();
            let idx: usize = name
                .trim_start_matches('0')
                .trim_end_matches(".mp4")
                .parse()
                .unwrap_or(0);
            match &last.outcome {
                FileOutcome::Ok => assert!(
                    !idx.is_multiple_of(2),
                    "even index {idx} should have failed, got Ok"
                ),
                FileOutcome::ProbeFailed(_) => assert!(
                    idx.is_multiple_of(2),
                    "odd index {idx} should have succeeded, got ProbeFailed"
                ),
            }
        }
    }

    #[cfg(feature = "parallel-analyze")]
    #[test]
    fn progress_empty_input_emits_no_ticks() {
        // U22 review-delta #8: pin the contract for
        // `files.len() == 0`. rayon par_iter on an empty slice is
        // a no-op, so the closure never fires; the returned
        // records Vec is empty. The actor arm's behavior for
        // empty input is tested separately (job::tests::
        // analyze_empty_files_emits_started_then_finished); this
        // test pins the lib-level contract independently.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let call_count = AtomicUsize::new(0);
        let probe = |_path: &Path| -> Result<VideoMetadata, EncodeError> {
            call_count.fetch_add(1, Ordering::SeqCst);
            Ok(VideoMetadata::default())
        };
        let progress = |_p: AnalyzeProgress| {
            // Should never fire on empty input.
            panic!("progress closure called on empty input");
        };
        let records = analyze_records_with_probe_and_progress_parallel(&[], probe, progress);
        assert!(records.is_empty());
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
    }

    #[cfg(feature = "parallel-analyze")]
    #[test]
    fn progress_done_counter_monotonic() {
        use std::sync::Mutex;
        // 200 files to make sure rayon parallelizes (otherwise the
        // serial fallback path would trivially be monotonic).
        let files: Vec<PathBuf> = (0..200)
            .map(|i| PathBuf::from(format!("/f/{i:03}.mp4")))
            .collect();
        let ticks: Mutex<Vec<AnalyzeProgress>> = Mutex::new(Vec::new());
        let probe = ok_probe;
        let progress = |p: AnalyzeProgress| {
            ticks.lock().unwrap().push(p);
        };
        let _ = analyze_records_with_probe_and_progress_parallel(&files, probe, progress);
        let ticks = ticks.into_inner().unwrap();
        // done must be strictly non-decreasing across the whole sequence.
        // We don't require strictly +1 between adjacent ticks (workers
        // race), but no tick may report a smaller `done` than a prior
        // tick. AND no tick may skip a value (done takes every value
        // 1..=total exactly once, just possibly out of order... actually
        // we can't guarantee that either: two workers may both increment
        // before the first one's progress fires, so the second one's
        // tick sees done=2 while the first one's tick (still pending)
        // will also see done=2).
        //
        // The contract is: `done` values across all ticks are exactly
        // {1, 2, ..., total} (multiset) AND the largest value equals
        // total. The closure always sees a snapshot >= 1.
        let mut done_values: Vec<usize> = ticks.iter().map(|t| t.done).collect();
        done_values.sort();
        let expected: Vec<usize> = (1..=files.len()).collect();
        assert_eq!(
            done_values, expected,
            "done values should be 1..=total exactly"
        );
        let max_done = done_values.last().unwrap();
        assert_eq!(*max_done, files.len());
    }
}
