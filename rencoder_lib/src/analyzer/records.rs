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
#[derive(Clone, Debug, PartialEq)]
pub struct AnalysisRecord {
    pub path: PathBuf,
    pub fps: Option<u32>,
    pub codec: Option<String>,
    pub duration_secs: Option<f64>,
    pub file_bytes: u64,
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

/// Probe files and aggregate into `VideoStats`. Public entry point for
/// the CLI's text summary path.
pub fn analyze_fps(files: &[PathBuf]) -> Result<VideoStats, EncodeError> {
    let records = analyze_records(files)?;
    Ok(aggregate_records(&records))
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
    fn analyze_fps_routes_through_records() {
        // Smoke: analyze_fps == aggregate_records(analyze_records(...))
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
}
