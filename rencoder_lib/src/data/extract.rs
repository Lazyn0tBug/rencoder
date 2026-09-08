//! Data extraction: source metadata, process stderr, output metadata.
//!
//! Each function returns data captured at one phase of the encode
//! pipeline:
//!
//! - `probe_video_metadata` — 1.1, source video metadata (the ffmpeg
//!   probe wrapped as a public API).
//! - `output_metadata` / `output_duration` — 1.3, output file metadata
//!   captured after a successful encode.
//! - `stderr_tail` — 1.2 helper: last N non-empty lines of stderr,
//!   for failure analysis.
//! - `run_ffmpeg_with_stderr` — 1.2 process capture wrapper: runs
//!   ffmpeg with stderr piped, returns a structured failure tuple
//!   on non-zero exit.

use std::path::Path;

/// Re-export the existing ffmpeg probe so callers can import via the
/// `data` namespace without breaking the public API. The actual
/// implementation lives in `crate::ffmpeg::probe`.
pub use crate::ffmpeg::probe::{probe_video_metadata, probe_video_metadata_auto};

/// Last N non-empty lines of an ffmpeg stderr buffer. Used for
/// failure analysis (the last few lines typically show the actual
/// error, e.g. "Conversion failed!" or "Invalid argument").
pub fn stderr_tail(stderr: &str, n: usize) -> String {
    stderr
        .lines()
        .filter(|l| !l.is_empty())
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Output file size in bytes (None if the file doesn't exist or
/// can't be stat'd). Captured into EncodingRecord after a successful
/// encode so the record carries the bytes-produced dimension.
pub fn output_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).map(|m| m.len()).ok()
}

/// Output file duration in seconds (re-probes the file with ffprobe).
/// None if the file can't be probed (deleted, corrupt, etc.).
/// Captured into EncodingRecord after a successful encode.
pub fn output_duration(path: &Path) -> Option<f64> {
    probe_video_metadata(path).ok()?.duration_secs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_tail_returns_last_n_nonempty_lines() {
        let s = "line1\n\nline2\nline3\n\nline4\n";
        assert_eq!(stderr_tail(s, 2), "line3\nline4");
        assert_eq!(stderr_tail(s, 10), "line1\nline2\nline3\nline4");
    }

    #[test]
    fn stderr_tail_skips_empty_lines() {
        let s = "\n\nline1\n\n\nline2\n\n";
        assert_eq!(stderr_tail(s, 5), "line1\nline2");
    }

    #[test]
    fn stderr_tail_on_empty_string() {
        assert_eq!(stderr_tail("", 5), "");
    }

    #[test]
    fn output_size_on_missing_file_is_none() {
        let path = Path::new("/nonexistent/path/xyz.mp4");
        assert_eq!(output_size(path), None);
    }
}
