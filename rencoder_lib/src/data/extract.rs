//! Data extraction: source metadata, process stderr, output metadata.
//!
//! Each function returns data captured at one phase of the encode
//! pipeline:
//!
//! - `probe_video_metadata` / `probe_video_metadata_auto` — 1.1, source
//!   video metadata (one `ffprobe` per file, optionally via C FFI when
//!   the `c-probe` feature is enabled). Pure JSON parsing happens in
//!   `parse_probe_output`, which is unit-testable without ffmpeg.
//! - `parse_fps_value` — fps string parser ("30000/1001", "25",
//!   "29.97"). Used by the probe to convert `avg_frame_rate` /
//!   `r_frame_rate` strings into `f64`.
//! - `output_metadata` / `output_duration` — 1.3, output file metadata
//!   captured after a successful encode.
//! - `stderr_tail` — 1.2 helper: last N non-empty lines of stderr,
//!   for failure analysis.
//! - `run_ffmpeg_with_stderr` — 1.2 process capture wrapper: runs
//!   ffmpeg with stderr piped, returns a structured failure tuple
//!   on non-zero exit.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::error::EncodeError;
use crate::types::VideoMetadata;

// ===== Probe (source video metadata) =====

/// Probe a video file: codec name + `avg_frame_rate` + `r_frame_rate`.
///
/// Spawns one `ffprobe` process with `-show_entries` filtered to the three
/// fields we need.
pub fn probe_video_metadata(input: &Path) -> Result<VideoMetadata, EncodeError> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=codec_name,avg_frame_rate,r_frame_rate",
        "-show_entries",
        "format=duration",
        "-of",
        "json",
    ])
    .arg(input);
    // The probe must not die from the platform's "stop everything"
    // event: a killed probe would record a *failed* file instead of
    // cancelling the run (platform::process).
    crate::platform::process::isolate_from_console_events(&mut cmd);
    let output = cmd
        .output()
        .map_err(|e| EncodeError::Probe(format!("ffprobe spawn: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(EncodeError::Probe(format!(
            "ffprobe exited {}: {}",
            output.status,
            stderr.trim()
        )));
    }

    let json = String::from_utf8_lossy(&output.stdout);
    parse_probe_output(&json)
}

/// Auto-dispatch: `c-probe` feature (Phase 13) prefers C FFI but falls
/// back to the CLI probe if the FFI call fails. Without `c-probe`, the
/// CLI probe is the only path.
pub fn probe_video_metadata_auto(input: &Path) -> Result<VideoMetadata, EncodeError> {
    #[cfg(feature = "c-probe")]
    {
        match crate::ffi::probe_video_metadata_fast(input) {
            Ok(m) => Ok(m),
            Err(e) => {
                tracing::warn!(
                    input = %input.display(),
                    error = %e,
                    "c-probe failed; falling back to ffprobe CLI"
                );
                probe_video_metadata(input)
            }
        }
    }
    #[cfg(not(feature = "c-probe"))]
    {
        probe_video_metadata(input)
    }
}

/// Parse the JSON output of `ffprobe -of json -show_entries stream=...`
/// into a `VideoMetadata`. Pure function — unit-testable without ffmpeg.
fn parse_probe_output(json: &str) -> Result<VideoMetadata, EncodeError> {
    let v: Value =
        serde_json::from_str(json).map_err(|e| EncodeError::Probe(format!("json parse: {e}")))?;

    let stream = v
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .ok_or_else(|| EncodeError::Probe("no streams in ffprobe output".into()))?;

    let codec = stream
        .get("codec_name")
        .and_then(Value::as_str)
        .map(String::from);

    let avg_fps = stream
        .get("avg_frame_rate")
        .and_then(Value::as_str)
        .and_then(|s| parse_fps_value(s).ok());

    let nominal_fps = stream
        .get("r_frame_rate")
        .and_then(Value::as_str)
        .and_then(|s| parse_fps_value(s).ok());

    let duration_secs = stream
        .get("duration")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
        // Some fixtures (e.g. lavfi-generated) report duration at the
        // format level rather than per-stream. Fall back to format.duration.
        .or_else(|| {
            v.get("format")
                .and_then(|f| f.get("duration"))
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<f64>().ok())
        });

    let container = v
        .get("format")
        .and_then(|f| f.get("format_name"))
        .and_then(Value::as_str)
        .map(String::from);

    let width = stream
        .get("width")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    let height = stream
        .get("height")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());

    let bit_rate = stream
        .get("bit_rate")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<u64>().ok())
        // Stream-level bit_rate missing on some fixtures; fall back to
        // format-level bit_rate (overall file bitrate).
        .or_else(|| {
            v.get("format")
                .and_then(|f| f.get("bit_rate"))
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<u64>().ok())
        });

    Ok(VideoMetadata {
        codec,
        avg_fps,
        nominal_fps,
        duration_secs,
        container,
        width,
        height,
        bit_rate,
    })
}

// ===== parse_fps_value (moved from fps/parse.rs) =====

/// Parse a raw fps string into a `f64`.
///
/// Accepts fractions ("30000/1001"), integers ("25"), and decimals
/// ("29.97"). Mirrors `encode.zsh`'s `parse_fps_value` semantics.
///
/// Returns `EncodeError::FpsParse` on:
/// - empty input
/// - non-numeric input
/// - zero or negative result
/// - NaN / infinity
/// - zero denominator in `num/den` form
pub fn parse_fps_value(raw: &str) -> Result<f64, EncodeError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(EncodeError::FpsParse("empty fps string".into()));
    }

    if let Some((num, den)) = raw.split_once('/') {
        return parse_fraction(num.trim(), den.trim());
    }

    let value: f64 = raw
        .parse()
        .map_err(|e| EncodeError::FpsParse(format!("cannot parse fps `{raw}`: {e}")))?;
    validate_fps(value)
}

/// Parse `"<numerator>/<denominator>"` into a single `f64`.
///
/// Decisions (per user):
/// - both sides parsed as `f64` (rejects nonsense like `"29.97/1"` cleanly
///   only via `validate_fps`; the explicit types keep the parse path uniform)
/// - zero denominator is rejected explicitly with a specific message,
///   not deferred to `validate_fps` (which would surface `"invalid fps: inf"`)
/// - bad numerator and bad denominator produce distinct error messages
fn parse_fraction(num: &str, den: &str) -> Result<f64, EncodeError> {
    let n: f64 = num
        .parse()
        .map_err(|e| EncodeError::FpsParse(format!("bad numerator `{num}`: {e}")))?;
    let d: f64 = den
        .parse()
        .map_err(|e| EncodeError::FpsParse(format!("bad denominator `{den}`: {e}")))?;
    if d == 0.0 {
        return Err(EncodeError::FpsParse("zero denominator".into()));
    }
    validate_fps(n / d)
}

/// Guard: an fps value must be positive and finite.
fn validate_fps(value: f64) -> Result<f64, EncodeError> {
    if !value.is_finite() || value <= 0.0 {
        return Err(EncodeError::FpsParse(format!("invalid fps: {value}")));
    }
    Ok(value)
}

// ===== stderr / output capture =====

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

    // ---- stderr_tail ----

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

    // ---- parse_probe_output ----

    #[test]
    fn parses_full_output() {
        let json = r#"{
            "streams": [
                {
                    "codec_name": "h264",
                    "avg_frame_rate": "30000/1001",
                    "r_frame_rate": "30/1"
                }
            ]
        }"#;
        let m = parse_probe_output(json).unwrap();
        assert_eq!(m.codec.as_deref(), Some("h264"));
        let v = m.best_fps().unwrap();
        assert!((v - 29.97002997002997).abs() < 1e-6, "got {v}");
    }

    #[test]
    fn parses_minimal_output() {
        let json = r#"{"streams": [{"codec_name": "vp9"}]}"#;
        let m = parse_probe_output(json).unwrap();
        assert_eq!(m.codec.as_deref(), Some("vp9"));
        assert!(m.best_fps().is_none());
    }

    #[test]
    fn rejects_empty_streams() {
        let json = r#"{"streams": []}"#;
        assert!(matches!(
            parse_probe_output(json),
            Err(EncodeError::Probe(_))
        ));
    }

    #[test]
    fn rejects_no_streams_key() {
        let json = r#"{}"#;
        assert!(matches!(
            parse_probe_output(json),
            Err(EncodeError::Probe(_))
        ));
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(matches!(
            parse_probe_output("not json"),
            Err(EncodeError::Probe(_))
        ));
    }

    #[test]
    fn parses_nominal_when_avg_missing() {
        let json = r#"{
            "streams": [
                {"codec_name": "hevc", "r_frame_rate": "25/1"}
            ]
        }"#;
        let m = parse_probe_output(json).unwrap();
        assert_eq!(m.codec.as_deref(), Some("hevc"));
        assert!(m.avg_fps.is_none());
        assert_eq!(m.nominal_fps, Some(25.0));
    }

    /// End-to-end: generate a tiny 30 fps H.264 fixture with ffmpeg, then
    /// probe it. Skipped silently if ffmpeg or libx264 is unavailable.
    #[test]
    fn real_ffprobe_against_fixture() {
        let dir = match std::env::var("CARGO_TARGET_TMPDIR") {
            Ok(d) => std::path::PathBuf::from(d),
            Err(_) => std::env::temp_dir(),
        };
        std::fs::create_dir_all(&dir).unwrap();
        let fixture = dir.join("rencoder_probe_fixture.mp4");

        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=320x240:d=1",
                "-r",
                "30",
                "-c:v",
                "libx264",
            ])
            .arg(&fixture)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        match status {
            Ok(s) if s.success() => {
                let m = probe_video_metadata(&fixture).unwrap();
                assert_eq!(m.codec.as_deref(), Some("h264"));
                let fps = m.best_fps().expect("expected fps");
                assert!((fps - 30.0).abs() < 0.1, "expected ~30 fps, got {fps}");
                std::fs::remove_file(&fixture).ok();
            }
            _ => {
                eprintln!("skipping real_ffprobe_against_fixture: ffmpeg/libx264 unavailable");
            }
        }
    }

    // ---- parse_fps_value ----

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn parses_fraction() {
        let v = parse_fps_value("30000/1001").unwrap();
        assert!(approx_eq(v, 29.97002997002997), "got {v}");
    }

    #[test]
    fn parses_integer() {
        assert_eq!(parse_fps_value("25").unwrap(), 25.0);
    }

    #[test]
    fn parses_decimal() {
        assert_eq!(parse_fps_value("29.97").unwrap(), 29.97);
    }

    #[test]
    fn parses_decimal_with_trailing_zero() {
        assert_eq!(parse_fps_value("30.0").unwrap(), 30.0);
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(parse_fps_value("  30  ").unwrap(), 30.0);
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(parse_fps_value(""), Err(EncodeError::FpsParse(_))));
        assert!(matches!(
            parse_fps_value("   "),
            Err(EncodeError::FpsParse(_))
        ));
    }

    #[test]
    fn rejects_zero_fraction_numerator() {
        assert!(matches!(
            parse_fps_value("0/1"),
            Err(EncodeError::FpsParse(_))
        ));
    }

    #[test]
    fn rejects_zero_denominator() {
        assert!(matches!(
            parse_fps_value("30/0"),
            Err(EncodeError::FpsParse(_))
        ));
    }

    #[test]
    fn rejects_negative_integer() {
        assert!(matches!(
            parse_fps_value("-30"),
            Err(EncodeError::FpsParse(_))
        ));
    }

    #[test]
    fn rejects_negative_decimal() {
        assert!(matches!(
            parse_fps_value("-29.97"),
            Err(EncodeError::FpsParse(_))
        ));
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            parse_fps_value("abc"),
            Err(EncodeError::FpsParse(_))
        ));
        assert!(matches!(
            parse_fps_value("30abc"),
            Err(EncodeError::FpsParse(_))
        ));
    }

    #[test]
    fn rejects_nan() {
        assert!(matches!(
            parse_fps_value("nan"),
            Err(EncodeError::FpsParse(_))
        ));
    }

    #[test]
    fn rejects_infinity() {
        assert!(matches!(
            parse_fps_value("inf"),
            Err(EncodeError::FpsParse(_))
        ));
    }
}
