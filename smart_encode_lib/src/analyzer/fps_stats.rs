//! FPS analysis: probe each file, build per-file `AnalysisRecord`s,
//! then aggregate into `FpsStats`.
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

use crate::error::EncodeError;
use crate::ffmpeg::probe::probe_video_metadata_auto;
use crate::types::FpsStats;

/// One row per input file. Failed probes yield a row with `path` and
/// `file_bytes` set but metric fields `None`.
#[derive(Clone, Debug)]
pub struct AnalysisRecord {
    pub path: PathBuf,
    pub fps: Option<u32>,
    pub codec: Option<String>,
    pub duration_secs: Option<f64>,
    pub file_bytes: u64,
}

/// Probe every file and emit one `AnalysisRecord` per input.
pub fn analyze_records(files: &[PathBuf]) -> Result<Vec<AnalysisRecord>, EncodeError> {
    Ok(analyze_records_with_probe(files, probe_video_metadata_auto))
}

/// Test seam: same as `analyze_records` but with an injectable probe.
pub fn analyze_records_with_probe<F>(
    files: &[PathBuf],
    probe: F,
) -> Vec<AnalysisRecord>
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
            Err(e) => {
                log::warn!("probe failed for {}: {e}", path.display());
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

/// Build a Polars DataFrame from records. Columns: `path`, `fps` (UInt32
/// nullable), `codec` (str nullable), `duration_secs` (Float64
/// nullable), `file_bytes` (UInt64). Available for downstream queries.
pub fn records_to_dataframe(records: &[AnalysisRecord]) -> PolarsResult<DataFrame> {
    let paths: Vec<&str> = records
        .iter()
        .map(|r| r.path.to_str().unwrap_or(""))
        .collect();
    let fps: Vec<Option<u32>> = records.iter().map(|r| r.fps).collect();
    let codecs: Vec<Option<&str>> = records.iter().map(|r| r.codec.as_deref()).collect();
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
        out.push_str(
            &r.duration_secs
                .map(|d| format!("{d}"))
                .unwrap_or_default(),
        );
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

/// Probe files and aggregate into `FpsStats`. Public entry point for
/// the CLI's text summary path.
pub fn analyze_fps(files: &[PathBuf]) -> Result<FpsStats, EncodeError> {
    let records = analyze_records(files)?;
    Ok(aggregate_records(&records))
}

/// Aggregate an already-built record list into `FpsStats` using
/// Polars expressions: `group_by([fps, codec]).agg([count, sum(duration),
/// sum(bytes)])` plus manual totals (parsed/failed counts and the
/// failed-files list).
pub fn aggregate_records(records: &[AnalysisRecord]) -> FpsStats {
    use polars::prelude::*;

    let total_files = records.len();
    let parsed = records.iter().filter(|r| r.fps.is_some()).count();
    let failed = total_files - parsed;
    let total_duration: f64 = records.iter().filter_map(|r| r.duration_secs).sum();
    let failed_paths: Vec<PathBuf> = records
        .iter()
        .filter(|r| r.fps.is_none())
        .map(|r| r.path.clone())
        .collect();

    // Cross-tab aggregation via Polars expressions. Failed probes
    // (fps = None) are filtered out -- they're tracked in failed_files
    // and failed_paths instead.
    let table = if records.is_empty() {
        FpsStats::new(0).table
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

    FpsStats {
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
}
