//! Encoding-record types and writers.
//!
//! The four-dimension work-analysis model (source / process / result /
//! status) is captured by `EncodingRecord`. The serialization layer in
//! `output` produces CSV / JSONL / Parquet via a single canonical
//! `DataFrame` schema.

mod output;

pub use output::{
    dataframe_to_csv, dataframe_to_jsonl, dataframe_to_parquet, records_to_csv,
    records_to_dataframe, records_to_jsonl, records_to_parquet,
};

use chrono::{DateTime, Utc};
use std::path::PathBuf;

pub use crate::types::{
    EncodingOutcome, EncodingRecord, EncoderSettings, ErrorCategory, ErrorContext, SkippedReason,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EncodingOutcome, EncoderSettings, ErrorCategory, ErrorContext};

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
        // 4-dimension columns + outcome.
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
        // One JSON object per line; non-empty objects.
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
        // Read back via Polars and verify a key column.
        let cursor = std::io::Cursor::new(bytes);
        let df = ParquetReader::new(cursor)
            .finish()
            .expect("read parquet");
        let paths = df.column("source_path").expect("source_path col");
        assert_eq!(paths.len(), 1);
        let first = paths.get(0).expect("first row");
        assert_eq!(
            first.get_str().expect("path string"),
            "/in.mp4"
        );
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
}
