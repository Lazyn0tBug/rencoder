//! Video metadata probing via `ffprobe`.
//!
//! One invocation per file returns codec name plus avg/nominal frame
//! rates, avoiding multiple `ffmpeg`/`ffprobe` spawns per file.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::error::EncodeError;
use crate::fps::parse::parse_fps_value;
use crate::types::VideoMetadata;

/// Probe a video file: codec name + `avg_frame_rate` + `r_frame_rate`.
///
/// Spawns one `ffprobe` process with `-show_entries` filtered to the three
/// fields we need.
pub fn probe_video_metadata(input: &Path) -> Result<VideoMetadata, EncodeError> {
    let output = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "stream=codec_name,avg_frame_rate,r_frame_rate",
            "-of", "json",
        ])
        .arg(input)
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
                log::warn!("c-probe failed for {}, falling back to ffprobe CLI: {}", input.display(), e);
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
    let v: Value = serde_json::from_str(json)
        .map_err(|e| EncodeError::Probe(format!("json parse: {e}")))?;

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
        .and_then(|s| s.parse::<f64>().ok());

    Ok(VideoMetadata {
        codec,
        avg_fps,
        nominal_fps,
        duration_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(matches!(parse_probe_output(json), Err(EncodeError::Probe(_))));
    }

    #[test]
    fn rejects_no_streams_key() {
        let json = r#"{}"#;
        assert!(matches!(parse_probe_output(json), Err(EncodeError::Probe(_))));
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(matches!(parse_probe_output("not json"), Err(EncodeError::Probe(_))));
    }

    #[test]
    fn parses_nominal_when_avg_missing() {
        // Some containers report only r_frame_rate.
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
                "-f", "lavfi",
                "-i", "color=c=black:s=320x240:d=1",
                "-r", "30",
                "-c:v", "libx264",
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
}
