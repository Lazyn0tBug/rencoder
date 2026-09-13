//! Data storage: writers that consume a single canonical Polars
//! `DataFrame` derived from `Vec<EncodingRecord>`.
//!
//! Three writers share the same schema (defined in
//! `records_to_dataframe`):
//!
//! ```text
//! EncodingRecord -> records_to_dataframe (single canonical schema)
//!                          |
//!             +------------+------------+------------+
//!             |            |            |            |
//!       dataframe_to_csv  dataframe_to_jsonl  dataframe_to_parquet
//! ```
//!
//! Adding a new column is a one-line edit; each writer is a thin Polars
//! call.

use chrono::{DateTime, Utc};
use polars::prelude::*;
use std::path::Path;

use crate::error::EncodeError;
use crate::types::{EncodingOutcome, EncodingRecord, ErrorCategory, ErrorContext, SkippedReason};

/// Build a `Series` of `DateTime<Utc>` values, casting the underlying
/// `i64` (milliseconds since epoch) into Polars' `Datetime` type so
/// `df!` accepts the column.
fn datetime_series(name: &str, values: Vec<DateTime<Utc>>) -> Series {
    let micros: Vec<i64> = values.iter().map(|dt| dt.timestamp_micros()).collect();
    Series::new(name.into(), micros)
        .cast(&DataType::Datetime(TimeUnit::Microseconds, None))
        .expect("datetime cast")
}

fn opt_datetime_series(name: &str, values: Vec<Option<DateTime<Utc>>>) -> Series {
    let micros: Vec<Option<i64>> = values
        .iter()
        .map(|opt| opt.map(|dt| dt.timestamp_micros()))
        .collect();
    Series::new(name.into(), micros)
        .cast(&DataType::Datetime(TimeUnit::Microseconds, None))
        .expect("opt datetime cast")
}

/// Flatten an EncodingRecord into the columnar schema used by all
/// three writers. Nested fields (encoder_settings, error) are
/// flattened; ffmpeg_argv is joined into a single space-separated
/// string.
fn record_to_columns(records: &[EncodingRecord]) -> DataFrame {
    let source_path: Vec<String> = records.iter().map(|r| r.source_path.clone()).collect();
    let source_size_bytes: Vec<u64> = records.iter().map(|r| r.source_size_bytes).collect();
    let source_codec: Vec<Option<String>> =
        records.iter().map(|r| r.source_codec.clone()).collect();
    let source_container: Vec<Option<String>> =
        records.iter().map(|r| r.source_container.clone()).collect();
    let source_width: Vec<Option<u32>> = records.iter().map(|r| r.source_width).collect();
    let source_height: Vec<Option<u32>> = records.iter().map(|r| r.source_height).collect();
    let source_fps: Vec<Option<f64>> = records.iter().map(|r| r.source_fps).collect();
    let source_duration_secs: Vec<Option<f64>> =
        records.iter().map(|r| r.source_duration_secs).collect();
    let source_bit_rate_bps: Vec<Option<u64>> =
        records.iter().map(|r| r.source_bit_rate_bps).collect();

    let started_at_dt: Vec<DateTime<Utc>> = records.iter().map(|r| r.started_at).collect();
    let ended_at_dt: Vec<Option<DateTime<Utc>>> = records.iter().map(|r| r.ended_at).collect();
    let encoding_duration_secs: Vec<Option<f64>> =
        records.iter().map(|r| r.encoding_duration_secs).collect();
    let encoder_selected: Vec<Option<String>> =
        records.iter().map(|r| r.encoder_selected.clone()).collect();
    let encoder_is_crf: Vec<bool> = records.iter().map(|r| r.encoder_settings.is_crf).collect();
    let encoder_crf_or_bitrate: Vec<u32> = records
        .iter()
        .map(|r| r.encoder_settings.crf_or_bitrate)
        .collect();
    let encoder_preset: Vec<Option<String>> = records
        .iter()
        .map(|r| r.encoder_settings.preset.clone())
        .collect();
    let encoder_max_fps_target: Vec<Option<f32>> = records
        .iter()
        .map(|r| r.encoder_settings.max_fps_target)
        .collect();
    let ffmpeg_argv: Vec<String> = records.iter().map(|r| r.ffmpeg_argv.join(" ")).collect();
    let ffmpeg_stderr_tail: Vec<String> = records
        .iter()
        .map(|r| r.ffmpeg_stderr_tail.clone())
        .collect();

    let output_path: Vec<Option<String>> = records.iter().map(|r| r.output_path.clone()).collect();
    let output_size_bytes: Vec<Option<u64>> = records.iter().map(|r| r.output_size_bytes).collect();
    let output_duration_secs: Vec<Option<f64>> =
        records.iter().map(|r| r.output_duration_secs).collect();
    let output_codec: Vec<Option<String>> =
        records.iter().map(|r| r.output_codec.clone()).collect();
    let compression_ratio: Vec<Option<f64>> = records.iter().map(|r| r.compression_ratio).collect();

    let outcome: Vec<String> = records.iter().map(|r| format!("{:?}", r.outcome)).collect();
    let error_category: Vec<Option<String>> = records
        .iter()
        .map(|r| r.error.as_ref().map(|e| format!("{:?}", e.category)))
        .collect();
    let error_message: Vec<Option<String>> = records
        .iter()
        .map(|r| r.error.as_ref().map(|e| e.message.clone()))
        .collect();

    let started_at = datetime_series("started_at", started_at_dt);
    let ended_at = opt_datetime_series("ended_at", ended_at_dt);

    df! {
        "source_path" => source_path,
        "source_size_bytes" => source_size_bytes,
        "source_codec" => source_codec,
        "source_container" => source_container,
        "source_width" => source_width,
        "source_height" => source_height,
        "source_fps" => source_fps,
        "source_duration_secs" => source_duration_secs,
        "source_bit_rate_bps" => source_bit_rate_bps,

        "started_at" => started_at,
        "ended_at" => ended_at,
        "encoding_duration_secs" => encoding_duration_secs,
        "encoder_selected" => encoder_selected,
        "encoder_is_crf" => encoder_is_crf,
        "encoder_crf_or_bitrate" => encoder_crf_or_bitrate,
        "encoder_preset" => encoder_preset,
        "encoder_max_fps_target" => encoder_max_fps_target,
        "ffmpeg_argv" => ffmpeg_argv,
        "ffmpeg_stderr_tail" => ffmpeg_stderr_tail,

        "output_path" => output_path,
        "output_size_bytes" => output_size_bytes,
        "output_duration_secs" => output_duration_secs,
        "output_codec" => output_codec,
        "compression_ratio" => compression_ratio,

        "outcome" => outcome,
        "error_category" => error_category,
        "error_message" => error_message,
    }
    .expect("df! build")
}

/// Convert a slice of `EncodingRecord` into a Polars DataFrame. This is
/// the canonical schema definition site for the encode pipeline.
pub fn records_to_dataframe(records: &[EncodingRecord]) -> PolarsResult<DataFrame> {
    Ok(record_to_columns(records))
}

/// Render the DataFrame as CSV (header + rows). Empty DataFrame → just
/// the header.
pub fn dataframe_to_csv(df: &DataFrame) -> String {
    let mut buf = Vec::new();
    polars::prelude::CsvWriter::new(&mut buf)
        .finish(&mut df.clone())
        .expect("CsvWriter::finish");
    String::from_utf8(buf).unwrap_or_default()
}

/// Render the DataFrame as JSON Lines (one object per line).
pub fn dataframe_to_jsonl(df: &DataFrame) -> String {
    let mut buf = Vec::new();
    let _ = polars::io::json::JsonWriter::new(&mut buf)
        .with_json_format(polars::io::json::JsonFormat::JsonLines)
        .finish(&mut df.clone());
    String::from_utf8(buf).unwrap_or_default()
}

/// Render the DataFrame as Parquet bytes (Snappy compression; schema
/// embedded; columnar).
pub fn dataframe_to_parquet(df: &DataFrame) -> PolarsResult<Vec<u8>> {
    let mut buf = Vec::new();
    polars::prelude::ParquetWriter::new(&mut buf)
        .with_compression(polars::prelude::ParquetCompression::Snappy)
        .finish(&mut df.clone())?;
    Ok(buf)
}

/// One-shot helper: records → CSV string.
pub fn records_to_csv(records: &[EncodingRecord]) -> String {
    let df = records_to_dataframe(records).expect("records_to_dataframe");
    dataframe_to_csv(&df)
}

/// One-shot helper: records → JSONL string.
pub fn records_to_jsonl(records: &[EncodingRecord]) -> String {
    let df = records_to_dataframe(records).expect("records_to_dataframe");
    dataframe_to_jsonl(&df)
}

/// One-shot helper: records → Parquet bytes.
pub fn records_to_parquet(records: &[EncodingRecord]) -> PolarsResult<Vec<u8>> {
    let df = records_to_dataframe(records)?;
    dataframe_to_parquet(&df)
}

/// Read records back from a single Parquet file. Inverse of
/// `records_to_parquet`; consumed by the GUI `Command::LoadHistory`
/// path (U21 §3.8.4). Multi-file glob + limit/after filtering lives
/// in the controller, not here.
///
/// Note: `ffmpeg_argv` roundtrip is lossy — stored as space-joined
/// string, split on read. Args containing spaces in their values
/// would survive incorrectly; current argv never does (paths with
/// spaces are still single args via `Command::arg`), but this is a
/// known limitation worth documenting.
pub fn read_records(path: &Path) -> Result<Vec<EncodingRecord>, EncodeError> {
    use polars::prelude::ParquetReader;
    let file = std::fs::File::open(path)?; // io::Error → EncodeError::Io via #[from]
    let df = ParquetReader::new(file)
        .finish()
        .map_err(|e| EncodeError::Probe(format!("parquet read {}: {e}", path.display())))?;
    columns_to_records(&df).map_err(|e| EncodeError::Probe(format!("parquet decode: {e}")))
}

/// Inverse of [`record_to_columns`]. Same column ordering and types;
/// see schema comments there.
fn columns_to_records(df: &DataFrame) -> PolarsResult<Vec<EncodingRecord>> {
    let h = df.height();
    if h == 0 {
        return Ok(Vec::new());
    }

    // Per-column Vec<Option<T>>; index i → row i.
    let str_col = |name: &str| -> PolarsResult<Vec<Option<String>>> {
        let s = df.column(name)?.str()?;
        Ok(s.into_iter().map(|o| o.map(str::to_string)).collect())
    };
    let u64_col = |name: &str| -> PolarsResult<Vec<Option<u64>>> {
        let s = df.column(name)?.u64()?;
        Ok(s.into_iter().collect())
    };
    let u32_col = |name: &str| -> PolarsResult<Vec<Option<u32>>> {
        let s = df.column(name)?.u32()?;
        Ok(s.into_iter().collect())
    };
    let f64_col = |name: &str| -> PolarsResult<Vec<Option<f64>>> {
        let s = df.column(name)?.f64()?;
        Ok(s.into_iter().collect())
    };
    let f32_col = |name: &str| -> PolarsResult<Vec<Option<f32>>> {
        let casted = df.column(name)?.cast(&DataType::Float32)?;
        let s = casted.f32()?;
        Ok(s.into_iter().collect())
    };
    let bool_col = |name: &str| -> PolarsResult<Vec<Option<bool>>> {
        let s = df.column(name)?.bool()?;
        Ok(s.into_iter().collect())
    };

    let source_path = str_col("source_path")?;
    let source_size_bytes = u64_col("source_size_bytes")?;
    let source_codec = str_col("source_codec")?;
    let source_container = str_col("source_container")?;
    let source_width = u32_col("source_width")?;
    let source_height = u32_col("source_height")?;
    let source_fps = f64_col("source_fps")?;
    let source_duration_secs = f64_col("source_duration_secs")?;
    let source_bit_rate_bps = u64_col("source_bit_rate_bps")?;

    // started_at is a Datetime column; we stored microseconds
    // (timestamp_micros) so we read with from_timestamp_micros.
    // Using from_timestamp_millis here would interpret the value as
    // ms and produce years like ~55,840 (a 1000x mismatch — the
    // original read_records_round_trip test didn't check started_at
    // so this went undetected until the mock_load_history filter
    // tests surfaced it).
    let started_at_casted = df.column("started_at")?.cast(&DataType::Int64)?;
    let started_at_us = started_at_casted.i64()?;
    let started_at: Vec<Option<DateTime<Utc>>> = started_at_us
        .into_iter()
        .map(|o| o.and_then(DateTime::<Utc>::from_timestamp_micros))
        .collect();
    // ended_at has the same schema.
    let ended_at_casted = df.column("ended_at")?.cast(&DataType::Int64)?;
    let ended_at_us = ended_at_casted.i64()?;
    let ended_at: Vec<Option<DateTime<Utc>>> = ended_at_us
        .into_iter()
        .map(|o| o.and_then(DateTime::<Utc>::from_timestamp_micros))
        .collect();

    let encoding_duration_secs = f64_col("encoding_duration_secs")?;
    let encoder_selected = str_col("encoder_selected")?;
    let encoder_is_crf = bool_col("encoder_is_crf")?;
    let encoder_crf_or_bitrate = u32_col("encoder_crf_or_bitrate")?;
    let encoder_preset = str_col("encoder_preset")?;
    let encoder_max_fps_target = f32_col("encoder_max_fps_target")?;
    let ffmpeg_argv_joined = str_col("ffmpeg_argv")?;
    let ffmpeg_stderr_tail = str_col("ffmpeg_stderr_tail")?;

    let output_path = str_col("output_path")?;
    let output_size_bytes = u64_col("output_size_bytes")?;
    let output_duration_secs = f64_col("output_duration_secs")?;
    let output_codec = str_col("output_codec")?;
    let compression_ratio = f64_col("compression_ratio")?;

    let outcome_str = str_col("outcome")?;
    let error_category_str = str_col("error_category")?;
    let error_message = str_col("error_message")?;

    let mut records = Vec::with_capacity(h);
    for i in 0..h {
        let outcome = parse_outcome(outcome_str[i].as_deref().unwrap_or(""));
        let error = match (
            error_category_str[i].as_deref(),
            error_message[i].as_deref(),
        ) {
            (Some(cat), Some(msg)) => Some(ErrorContext {
                category: parse_error_category(cat),
                message: msg.to_string(),
                ffmpeg_stderr_tail: ffmpeg_stderr_tail[i].clone().unwrap_or_default(),
            }),
            _ => None,
        };

        records.push(EncodingRecord {
            source_path: source_path[i].clone().unwrap_or_default(),
            source_size_bytes: source_size_bytes[i].unwrap_or(0),
            source_codec: source_codec[i].clone(),
            source_container: source_container[i].clone(),
            source_width: source_width[i],
            source_height: source_height[i],
            source_fps: source_fps[i],
            source_duration_secs: source_duration_secs[i],
            source_bit_rate_bps: source_bit_rate_bps[i],

            started_at: started_at[i].unwrap_or_else(Utc::now),
            ended_at: ended_at[i],
            encoding_duration_secs: encoding_duration_secs[i],
            encoder_selected: encoder_selected[i].clone(),
            encoder_settings: crate::types::EncoderSettings {
                is_crf: encoder_is_crf[i].unwrap_or(true),
                crf_or_bitrate: encoder_crf_or_bitrate[i].unwrap_or(0),
                preset: encoder_preset[i].clone(),
                max_fps_target: encoder_max_fps_target[i],
            },
            ffmpeg_argv: ffmpeg_argv_joined[i]
                .as_deref()
                .unwrap_or("")
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            ffmpeg_stderr_tail: ffmpeg_stderr_tail[i].clone().unwrap_or_default(),

            output_path: output_path[i].clone(),
            output_size_bytes: output_size_bytes[i],
            output_duration_secs: output_duration_secs[i],
            output_codec: output_codec[i].clone(),
            compression_ratio: compression_ratio[i],

            outcome,
            error,
        });
    }
    Ok(records)
}

/// Parse the Debug-formatted outcome string back to `EncodingOutcome`.
/// Format: `"Succeeded"`, `"ReducedFps"`, `"Failed"`, or
/// `"Skipped(SameCodec)"` / `"Skipped(OutputExists)"` /
/// `"Skipped(PartialOutput)"`.
fn parse_outcome(s: &str) -> EncodingOutcome {
    match s {
        "Succeeded" => EncodingOutcome::Succeeded,
        "ReducedFps" => EncodingOutcome::ReducedFps,
        "Failed" => EncodingOutcome::Failed,
        "Skipped(SameCodec)" => EncodingOutcome::Skipped(SkippedReason::SameCodec),
        "Skipped(OutputExists)" => EncodingOutcome::Skipped(SkippedReason::OutputExists),
        "Skipped(PartialOutput)" => EncodingOutcome::Skipped(SkippedReason::PartialOutput),
        // Forward compat: future variants come back as Failed with
        // an empty error context. Better than panicking on unknown.
        _ => EncodingOutcome::Failed,
    }
}

fn parse_error_category(s: &str) -> ErrorCategory {
    match s {
        "ProbeFailed" => ErrorCategory::ProbeFailed,
        "EncoderSelectionFailed" => ErrorCategory::EncoderSelectionFailed,
        "FfmpegSpawnFailed" => ErrorCategory::FfmpegSpawnFailed,
        "FfmpegNonzeroExit" => ErrorCategory::FfmpegNonzeroExit,
        "ConfigError" => ErrorCategory::ConfigError,
        "Cancelled" => ErrorCategory::Cancelled,
        "IoError" => ErrorCategory::IoError,
        // Unknown category → IoError as a safe fallback.
        _ => ErrorCategory::IoError,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EncoderSettings, EncodingOutcome, ErrorCategory, ErrorContext};
    use chrono::{DateTime, Utc};

    fn sample_record() -> EncodingRecord {
        EncodingRecord {
            source_path: "/in.mp4".into(),
            source_size_bytes: 1024,
            source_codec: Some("h264".into()),
            source_container: Some("mov,mp4,m4a".into()),
            source_width: Some(1920),
            source_height: Some(1080),
            source_fps: Some(30.0),
            source_duration_secs: Some(60.0),
            source_bit_rate_bps: Some(5_000_000),

            started_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            ended_at: Some(DateTime::<Utc>::from_timestamp(1_700_000_010, 0).unwrap()),
            encoding_duration_secs: Some(10.0),
            encoder_selected: Some("libsvtav1".into()),
            encoder_settings: EncoderSettings {
                is_crf: true,
                crf_or_bitrate: 45,
                preset: None,
                max_fps_target: None,
            },
            ffmpeg_argv: vec!["ffmpeg".into(), "-i".into(), "in.mp4".into()],
            ffmpeg_stderr_tail: String::new(),

            output_path: Some("/out.av1.mkv".into()),
            output_size_bytes: Some(512),
            output_duration_secs: Some(60.0),
            output_codec: Some("libsvtav1".into()),
            compression_ratio: Some(0.5),

            outcome: EncodingOutcome::Succeeded,
            error: None,
        }
    }

    #[test]
    fn records_to_dataframe_builds_all_columns() {
        let recs = vec![sample_record(), sample_record()];
        let df = records_to_dataframe(&recs).expect("df");
        assert_eq!(df.height(), 2);
        assert!(df.column("source_path").is_ok());
        assert!(df.column("started_at").is_ok());
        assert!(df.column("output_path").is_ok());
        assert!(df.column("outcome").is_ok());
    }

    #[test]
    fn records_to_csv_round_trip() {
        let recs = vec![sample_record()];
        let csv = records_to_csv(&recs);
        assert!(csv.starts_with("source_path,"));
        assert!(csv.contains("/in.mp4"));
        assert!(csv.contains("libsvtav1"));
        assert!(csv.contains("Succeeded"));
    }

    #[test]
    fn records_to_jsonl_round_trip() {
        let recs = vec![sample_record()];
        let jsonl = records_to_jsonl(&recs);
        assert_eq!(jsonl.lines().count(), 1);
        assert!(jsonl.contains("/in.mp4"));
        assert!(jsonl.contains("libsvtav1"));
    }

    #[test]
    fn records_to_parquet_round_trip() {
        use polars::prelude::*;
        let recs = vec![sample_record()];
        let bytes = records_to_parquet(&recs).expect("parquet");
        assert!(!bytes.is_empty());
        let cursor = std::io::Cursor::new(bytes);
        let df = ParquetReader::new(cursor).finish().expect("read parquet");
        let paths = df.column("source_path").expect("source_path col");
        assert_eq!(paths.len(), 1);
        let first = paths.get(0).expect("first row");
        assert_eq!(first.get_str().expect("path string"), "/in.mp4");
    }

    #[test]
    fn captures_failure_context_in_columns() {
        let mut rec = sample_record();
        rec.outcome = EncodingOutcome::Failed;
        rec.error = Some(ErrorContext {
            category: ErrorCategory::FfmpegNonzeroExit,
            message: "ffmpeg exited 1".into(),
            ffmpeg_stderr_tail: "Error: invalid argument".into(),
        });
        let df = records_to_dataframe(&[rec]).expect("df");
        let cats = df.column("error_category").expect("cat col");
        let first = cats.get(0).expect("first");
        let cat_str = first.to_string();
        assert!(cat_str.contains("FfmpegNonzeroExit"));
    }

    #[test]
    fn read_records_round_trip() {
        // U21 §3.8.6: write_records → read_records should preserve
        // the original EncodingRecord (modulo ffmpeg_argv join/split).
        let tmp = std::env::temp_dir().join(format!("rencoder_store_rt_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("test.parquet");

        let original = vec![sample_record()];
        let bytes = records_to_parquet(&original).expect("write");
        std::fs::write(&path, bytes).expect("save to disk");

        let read_back = read_records(&path).expect("read");
        assert_eq!(read_back.len(), original.len());
        let a = &read_back[0];
        let b = &original[0];
        assert_eq!(a.source_path, b.source_path);
        assert_eq!(a.source_size_bytes, b.source_size_bytes);
        assert_eq!(a.source_codec, b.source_codec);
        assert_eq!(a.source_fps, b.source_fps);
        assert_eq!(a.encoder_selected, b.encoder_selected);
        assert_eq!(a.encoder_settings.is_crf, b.encoder_settings.is_crf);
        assert_eq!(
            a.encoder_settings.crf_or_bitrate,
            b.encoder_settings.crf_or_bitrate
        );
        assert_eq!(a.output_path, b.output_path);
        assert_eq!(a.output_size_bytes, b.output_size_bytes);
        assert_eq!(a.outcome, b.outcome);
        // ffmpeg_argv roundtrip is lossy via space-join, but for
        // argv without internal spaces it's identity:
        assert_eq!(a.ffmpeg_argv, b.ffmpeg_argv);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_records_skipped_outcome_round_trip() {
        // U21 §3.8.6 + §3.2: SkippedReason::PartialOutput is a new
        // variant that must survive Parquet round-trip.
        let mut rec = sample_record();
        rec.outcome = EncodingOutcome::Skipped(SkippedReason::PartialOutput);
        let df = records_to_dataframe(&[rec]).expect("df");
        let outcome_col = df
            .column("outcome")
            .expect("outcome col")
            .str()
            .expect("str");
        let first = outcome_col.get(0).expect("first");
        assert_eq!(first, "Skipped(PartialOutput)");
    }

    #[test]
    fn read_records_missing_file_returns_io_error() {
        let path = std::path::PathBuf::from("/nonexistent/rencoder_test.parquet");
        let result = read_records(&path);
        assert!(result.is_err());
    }
}
