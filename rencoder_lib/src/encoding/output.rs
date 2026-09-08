//! EncodingRecord serialization layer.
//!
//! Three writers consume a single canonical Polars `DataFrame`:
//!
//! ```text
//! EncodingRecord -> records_to_dataframe (single canonical schema)
//!                          |
//!             +------------+------------+------------+
//!             |            |            |            |
//!       dataframe_to_csv  dataframe_to_jsonl  dataframe_to_parquet
//! ```
//!
//! Schema lives in one place (`records_to_dataframe`). Adding a new
//! column is a one-line edit; each writer is a thin Polars call.

use chrono::{DateTime, Utc};
use polars::prelude::*;

use crate::encoding::EncodingRecord;

/// Build a `Series` of `DateTime<Utc>` values, casting the underlying
/// `i64` (milliseconds since epoch) into Polars' `Datetime` type so
/// `df!` accepts the column.
fn datetime_series(name: &str, values: Vec<DateTime<Utc>>) -> Series {
    let micros: Vec<i64> = values
        .iter()
        .map(|dt| dt.timestamp_micros())
        .collect();
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
    let ended_at_dt: Vec<Option<DateTime<Utc>>> =
        records.iter().map(|r| r.ended_at).collect();
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
    let ffmpeg_argv: Vec<String> = records
        .iter()
        .map(|r| r.ffmpeg_argv.join(" "))
        .collect();
    let ffmpeg_stderr_tail: Vec<String> =
        records.iter().map(|r| r.ffmpeg_stderr_tail.clone()).collect();

    let output_path: Vec<Option<String>> =
        records.iter().map(|r| r.output_path.clone()).collect();
    let output_size_bytes: Vec<Option<u64>> =
        records.iter().map(|r| r.output_size_bytes).collect();
    let output_duration_secs: Vec<Option<f64>> =
        records.iter().map(|r| r.output_duration_secs).collect();
    let output_codec: Vec<Option<String>> =
        records.iter().map(|r| r.output_codec.clone()).collect();
    let compression_ratio: Vec<Option<f64>> =
        records.iter().map(|r| r.compression_ratio).collect();

    let outcome: Vec<String> = records
        .iter()
        .map(|r| format!("{:?}", r.outcome))
        .collect();
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
