//! Record construction: helpers that fill an `EncodingRecord` from the
//! inputs a file's run produced.
//!
//! The record assembly lives next to the type definition so the two
//! cannot drift apart.
//!
//! There is no "skipped" path any more: a file in the batch is
//! transcribed, full stop (design doc §3.3.4). The `Skipped` outcome
//! still exists in the record schema, but only so old Parquet files
//! can be read back -- nothing produces it.

use crate::types::EncoderSettings;

/// Pull the encoder settings out of EncodeParams into the typed struct
/// stored on EncodingRecord. Used by every record-construction helper.
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
