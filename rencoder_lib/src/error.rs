//! Library error type. CLI maps these to user-facing messages via `anyhow`.

use crate::types::ErrorCategory;

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum EncodeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("FFmpeg error: {0}")]
    Ffmpeg(String),

    #[error("Probe error: {0}")]
    Probe(String),

    #[error("Invalid parameter: {0}")]
    InvalidParam(String),

    #[error("Encoder not found: {0}")]
    EncoderNotFound(String),

    #[error("FPS parse error: {0}")]
    FpsParse(String),

    #[error("User cancelled")]
    UserCancel,

    /// v1.2 follow-up #5: `collect_files` exceeded
    /// `[encode] max_batch_size`. `count` is the partial count
    /// when the cap tripped (may exceed `max` by 1 — the loop
    /// checks before pushing). Caller renders as
    /// "batch too large: {count} files (max {max})".
    #[error("batch too large: {count} files (max {max})")]
    BatchTooLarge { count: usize, max: u32 },
}

impl EncodeError {
    /// Map to the [`ErrorCategory`] the UI displays. Review #121:
    /// the mapping was previously a `pub(crate) fn encode_error_category`
    /// in `job.rs`, which forced `pipeline::task` (a lower layer)
    /// to depend on `job` (a higher layer). Moving it onto
    /// `EncodeError` itself keeps the dependency direction correct:
    /// `pipeline::*` depends on `error`, not the other way around.
    ///
    /// | `EncodeError`       | `ErrorCategory`         |
    /// |---------------------|-------------------------|
    /// | `Io(_)`             | `IoError`               |
    /// | `Ffmpeg(_)`         | `FfmpegSpawnFailed`    |
    /// | `Probe(_)`          | `ProbeFailed`           |
    /// | `InvalidParam(_)`   | `ConfigError`           |
    /// | `EncoderNotFound(_)`| `EncoderSelectionFailed`|
    /// | `FpsParse(_)`       | `ConfigError`           |
    /// | `UserCancel`        | `Cancelled`             |
    /// | `BatchTooLarge {..}`| `ConfigError`           |
    pub fn category(&self) -> ErrorCategory {
        match self {
            EncodeError::Io(_) => ErrorCategory::IoError,
            EncodeError::Ffmpeg(_) => ErrorCategory::FfmpegSpawnFailed,
            EncodeError::Probe(_) => ErrorCategory::ProbeFailed,
            EncodeError::InvalidParam(_) => ErrorCategory::ConfigError,
            EncodeError::EncoderNotFound(_) => ErrorCategory::EncoderSelectionFailed,
            EncodeError::FpsParse(_) => ErrorCategory::ConfigError,
            EncodeError::UserCancel => ErrorCategory::Cancelled,
            EncodeError::BatchTooLarge { .. } => ErrorCategory::ConfigError,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_via_from_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: EncodeError = io_err.into();
        assert!(matches!(err, EncodeError::Io(_)));
        assert!(err.to_string().contains("I/O error"));
    }

    #[test]
    fn ffmpeg_error_display() {
        let err = EncodeError::Ffmpeg("crashed".into());
        assert_eq!(err.to_string(), "FFmpeg error: crashed");
    }

    #[test]
    fn probe_error_display() {
        let err = EncodeError::Probe("bad json".into());
        assert_eq!(err.to_string(), "Probe error: bad json");
    }

    #[test]
    fn invalid_param_error_display() {
        let err = EncodeError::InvalidParam("missing field".into());
        assert_eq!(err.to_string(), "Invalid parameter: missing field");
    }

    #[test]
    fn encoder_not_found_display() {
        let err = EncodeError::EncoderNotFound("h264".into());
        assert_eq!(err.to_string(), "Encoder not found: h264");
    }

    #[test]
    fn fps_parse_error_display() {
        let err = EncodeError::FpsParse("bad input".into());
        assert_eq!(err.to_string(), "FPS parse error: bad input");
    }

    #[test]
    fn user_cancel_display() {
        let err = EncodeError::UserCancel;
        assert_eq!(err.to_string(), "User cancelled");
    }

    // Review #121: category mapping table.
    #[test]
    fn category_mapping_covers_all_variants() {
        use crate::types::ErrorCategory;
        let cases: Vec<(EncodeError, ErrorCategory)> = vec![
            (
                EncodeError::Io(std::io::Error::other("x")),
                ErrorCategory::IoError,
            ),
            (
                EncodeError::Ffmpeg("x".into()),
                ErrorCategory::FfmpegSpawnFailed,
            ),
            (EncodeError::Probe("x".into()), ErrorCategory::ProbeFailed),
            (
                EncodeError::InvalidParam("x".into()),
                ErrorCategory::ConfigError,
            ),
            (
                EncodeError::EncoderNotFound("x".into()),
                ErrorCategory::EncoderSelectionFailed,
            ),
            (
                EncodeError::FpsParse("x".into()),
                ErrorCategory::ConfigError,
            ),
            (EncodeError::UserCancel, ErrorCategory::Cancelled),
        ];
        for (err, expected) in cases {
            assert_eq!(err.category(), expected, "wrong category for {err:?}");
        }
    }
}
