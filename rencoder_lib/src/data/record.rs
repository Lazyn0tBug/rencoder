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
    EncoderSettings, EncodingOutcome, EncodingRecord, ErrorCategory, ErrorContext, SkippedReason,
    TranscodeTask,
};

/// Build a record for the skip path: no work was performed.
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
    EncodingRecord {
        source_path: task.input.to_string(),
        source_size_bytes,
        source_codec: source_meta.as_ref().and_then(|m| m.codec.clone()),
        source_container: source_meta.as_ref().and_then(|m| m.container.clone()),
        source_width: source_meta.as_ref().and_then(|m| m.width),
        source_height: source_meta.as_ref().and_then(|m| m.height),
        source_fps: source_meta.as_ref().and_then(|m| m.best_fps()),
        source_duration_secs: source_meta.as_ref().and_then(|m| m.duration_secs),
        source_bit_rate_bps: source_meta.as_ref().and_then(|m| m.bit_rate),

        started_at,
        ended_at: None,
        encoding_duration_secs,
        encoder_selected: None,
        encoder_settings: encoder_settings_from_params(params),
        ffmpeg_argv: Vec::new(),
        ffmpeg_stderr_tail: String::new(),

        output_path: None,
        output_size_bytes: None,
        output_duration_secs: None,
        output_codec: None,
        compression_ratio: None,

        outcome: EncodingOutcome::Skipped(reason),
        error: None,
    }
}

/// Build a record for the probe-failure path: probe_video_metadata_auto
/// returned Err before any encoding work happened.
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
    EncodingRecord {
        source_path: task.input.to_string(),
        source_size_bytes,
        source_codec: None,
        source_container: None,
        source_width: None,
        source_height: None,
        source_fps: None,
        source_duration_secs: None,
        source_bit_rate_bps: None,

        started_at,
        ended_at: Some(Utc::now()),
        encoding_duration_secs,
        encoder_selected: None,
        encoder_settings: encoder_settings_from_params(params),
        ffmpeg_argv: Vec::new(),
        ffmpeg_stderr_tail: String::new(),

        output_path: None,
        output_size_bytes: None,
        output_duration_secs: None,
        output_codec: None,
        compression_ratio: None,

        outcome: EncodingOutcome::Failed,
        error: Some(ErrorContext {
            category: ErrorCategory::ProbeFailed,
            message: err.to_string(),
            ffmpeg_stderr_tail: String::new(),
        }),
    }
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
