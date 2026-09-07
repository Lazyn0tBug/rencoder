//! Library error type. CLI maps these to user-facing messages via `anyhow`.

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
}
