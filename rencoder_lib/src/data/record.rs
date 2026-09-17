//! Record construction: build an `EncodingRecord` from one of three
//! paths (success / skipped / failed).
//!
//! These helpers are factored out of `pipeline::batch` so the record
//! assembly logic lives next to the type definition. They each take
//! the partial inputs (file path, params, etc.) and produce a fully-
//! populated EncodingRecord with timestamps, settings, and outcome.

use chrono::Utc;

use crate::data::extract::probe_video_metadata_auto;
use crate::types::{
    EncoderSettings, EncodingOutcome, EncodingRecord, ErrorContext, SkippedReason, TranscodeTask,
};

/// Build a record for the skip path: no work was performed.
/// Review #124: source fields via `EncodingRecord::from_metadata`.
pub fn make_skipped_record(
    task: &TranscodeTask,
    params: &crate::types::EncodeParams,
    started_at: chrono::DateTime<Utc>,
    encoding_duration_secs: Option<f64>,
    reason: SkippedReason,
) -> EncodingRecord {
    let source_size_bytes = std::fs::metadata(task.input.as_path())
        .map(|m| m.len())
        .unwrap_or(0);
    let source_meta = probe_video_metadata_auto(task.input.as_path()).ok();
    let mut record =
        EncodingRecord::from_metadata(&task.input, source_size_bytes, source_meta.as_ref());
    record.started_at = started_at;
    record.encoding_duration_secs = encoding_duration_secs;
    record.encoder_settings = encoder_settings_from_params(params);
    record.outcome = EncodingOutcome::Skipped(reason);
    record
}

/// Build a record for the probe-failure path: probe_video_metadata_auto
/// returned Err before any encoding work happened.
/// Review #124: source fields via `EncodingRecord::from_metadata`
/// (with `None` — no metadata available on probe failure).
pub fn make_failed_probe_record(
    task: &TranscodeTask,
    params: &crate::types::EncodeParams,
    started_at: chrono::DateTime<Utc>,
    encoding_duration_secs: Option<f64>,
    err: &crate::error::EncodeError,
) -> EncodingRecord {
    let source_size_bytes = std::fs::metadata(task.input.as_path())
        .map(|m| m.len())
        .unwrap_or(0);
    let mut record = EncodingRecord::from_metadata(&task.input, source_size_bytes, None);
    record.started_at = started_at;
    record.ended_at = Some(Utc::now());
    record.encoding_duration_secs = encoding_duration_secs;
    record.encoder_settings = encoder_settings_from_params(params);
    record.outcome = EncodingOutcome::Failed;
    record.error = Some(ErrorContext {
        category: err.category(),
        message: err.to_string(),
        ffmpeg_stderr_tail: String::new(),
    });
    record
}

/// Pull the encoder settings out of EncodeParams into the typed struct
/// stored on EncodingRecord. Used by every record-construction helper
/// (success / skipped / failed).
pub fn encoder_settings_from_params(params: &crate::types::EncodeParams) -> EncoderSettings {
    EncoderSettings {
        is_crf: params.is_crf,
        crf_or_bitrate: params.bitrate_or_crf,
        preset: None,
        max_fps_target: if params.max_fps.enabled() {
            Some(params.max_fps.max_fps as f32)
        } else {
            None
        },
    }
}
