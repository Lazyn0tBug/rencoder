//! FPS analysis: probe each file, group by rounded fps, count failures.
//!
//! Per-file probe failures are recorded in `FpsStats.failed_files` with a
//! `warn!` log; they do NOT abort the batch. Files with valid metadata but
//! no fps info also count as failures (the script's behavior for streams
//! without frame-rate metadata).

use std::path::{Path, PathBuf};

use crate::error::EncodeError;
use crate::ffmpeg::probe::probe_video_metadata_auto;
use crate::types::{FpsStats, VideoMetadata};

/// Probe every file and group by rounded fps. Returns Ok(stats) on
/// completion; per-file failures are recorded in the stats, not in the
/// return type.
pub fn analyze_fps(files: &[PathBuf]) -> Result<FpsStats, EncodeError> {
    analyze_fps_with(files, probe_video_metadata_auto)
}

/// Test seam: same as `analyze_fps` but with an injectable probe so tests
/// don't need real video files.
fn analyze_fps_with<F>(files: &[PathBuf], probe: F) -> Result<FpsStats, EncodeError>
where
    F: Fn(&Path) -> Result<VideoMetadata, EncodeError>,
{
    let mut stats = FpsStats::new(files.len());
    for path in files {
        match probe(path) {
            Ok(metadata) => {
                if let Some(fps) = metadata.best_fps() {
                    let rounded = fps.round() as u32;
                    stats.add(rounded, path.to_path_buf());
                    if let Some(codec) = metadata.codec {
                        stats.add_codec(codec);
                    }
                    if let Some(dur) = metadata.duration_secs {
                        stats.add_duration(dur);
                    }
                } else {
                    log::warn!("no fps reported for {}", path.display());
                    stats.add_failed();
                }
            }
            Err(e) => {
                log::warn!("probe failed for {}: {e}", path.display());
                stats.add_failed();
            }
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h264(fps: f64) -> VideoMetadata {
        VideoMetadata {
            codec: Some("h264".into()),
            avg_fps: Some(fps),
            ..Default::default()
        }
    }

    fn no_fps() -> VideoMetadata {
        VideoMetadata {
            codec: Some("h264".into()),
            ..Default::default()
        }
    }

    #[test]
    fn empty_input_yields_zeroed_stats() {
        let probe = |_: &Path| -> Result<VideoMetadata, EncodeError> {
            unreachable!("probe should not be called for empty input")
        };
        let stats = analyze_fps_with(&[], probe).unwrap();
        assert_eq!(stats.total_files, 0);
        assert_eq!(stats.parsed_files, 0);
        assert_eq!(stats.failed_files, 0);
        assert!(stats.groups.is_empty());
    }

    #[test]
    fn groups_files_by_rounded_fps() {
        let files = vec![
            PathBuf::from("/a.mp4"),
            PathBuf::from("/b.mp4"),
            PathBuf::from("/c.mp4"),
        ];
        // 23.976 -> 24, 29.97 -> 30, 59.94 -> 60 (NTSC rounding).
        let probe = |path: &Path| -> Result<VideoMetadata, EncodeError> {
            let fps = match path.file_name().unwrap().to_str().unwrap() {
                "a.mp4" => 23.976,
                "b.mp4" => 29.97,
                "c.mp4" => 59.94,
                _ => 0.0,
            };
            Ok(h264(fps))
        };
        let stats = analyze_fps_with(&files, probe).unwrap();
        assert_eq!(stats.total_files, 3);
        assert_eq!(stats.parsed_files, 3);
        assert_eq!(stats.failed_files, 0);
        assert_eq!(stats.groups.get(&24).unwrap(), &vec![PathBuf::from("/a.mp4")]);
        assert_eq!(stats.groups.get(&30).unwrap(), &vec![PathBuf::from("/b.mp4")]);
        assert_eq!(stats.groups.get(&60).unwrap(), &vec![PathBuf::from("/c.mp4")]);
    }

    #[test]
    fn integer_fps_rounds_to_itself() {
        let files = vec![PathBuf::from("/a.mp4")];
        let probe = |_: &Path| Ok(h264(30.0));
        let stats = analyze_fps_with(&files, probe).unwrap();
        assert_eq!(stats.groups.get(&30).unwrap().len(), 1);
    }

    #[test]
    fn probe_failure_increments_failed_count_does_not_panic() {
        let files = vec![
            PathBuf::from("/ok.mp4"),
            PathBuf::from("/fail.mp4"),
            PathBuf::from("/ok2.mp4"),
        ];
        let probe = |path: &Path| -> Result<VideoMetadata, EncodeError> {
            if path.file_name().unwrap().to_str().unwrap() == "fail.mp4" {
                Err(EncodeError::Probe("canned failure".into()))
            } else {
                Ok(h264(30.0))
            }
        };
        let stats = analyze_fps_with(&files, probe).unwrap();
        assert_eq!(stats.total_files, 3);
        assert_eq!(stats.parsed_files, 2);
        assert_eq!(stats.failed_files, 1);
        // The two ok files should be in the 30 fps group.
        assert_eq!(stats.groups.get(&30).unwrap().len(), 2);
    }

    #[test]
    fn missing_fps_metadata_counts_as_failure() {
        // Some files probe successfully but report no fps (e.g. audio-only
        // streams mistakenly passed in). Count them as failures, not errors.
        let files = vec![PathBuf::from("/no_fps.mp4")];
        let probe = |_: &Path| Ok(no_fps());
        let stats = analyze_fps_with(&files, probe).unwrap();
        assert_eq!(stats.failed_files, 1);
        assert_eq!(stats.parsed_files, 0);
        assert!(stats.groups.is_empty());
    }

    #[test]
    fn mixed_success_and_failure_records_correctly() {
        let files = vec![
            PathBuf::from("/24a.mp4"),
            PathBuf::from("/30a.mp4"),
            PathBuf::from("/fail.mp4"),
            PathBuf::from("/60a.mp4"),
            PathBuf::from("/no_fps.mp4"),
        ];
        let probe = |path: &Path| -> Result<VideoMetadata, EncodeError> {
            match path.file_name().unwrap().to_str().unwrap() {
                "fail.mp4" => Err(EncodeError::Probe("boom".into())),
                "no_fps.mp4" => Ok(no_fps()),
                "24a.mp4" => Ok(h264(24.0)),
                "30a.mp4" => Ok(h264(30.0)),
                "60a.mp4" => Ok(h264(60.0)),
                _ => unreachable!(),
            }
        };
        let stats = analyze_fps_with(&files, probe).unwrap();
        assert_eq!(stats.total_files, 5);
        assert_eq!(stats.parsed_files, 3);
        assert_eq!(stats.failed_files, 2);
        assert_eq!(stats.groups.get(&24).unwrap().len(), 1);
        assert_eq!(stats.groups.get(&30).unwrap().len(), 1);
        assert_eq!(stats.groups.get(&60).unwrap().len(), 1);
    }

    #[test]
    fn fps_near_boundary_rounds_correctly() {
        // 23.5 rounds to 24 (banker's rounding in Rust: away from zero).
        // 29.4 rounds to 29.
        let files = vec![
            PathBuf::from("/high.mp4"),
            PathBuf::from("/low.mp4"),
        ];
        let probe = |path: &Path| -> Result<VideoMetadata, EncodeError> {
            let fps = match path.file_name().unwrap().to_str().unwrap() {
                "high.mp4" => 23.5,
                "low.mp4" => 29.4,
                _ => 0.0,
            };
            Ok(h264(fps))
        };
        let stats = analyze_fps_with(&files, probe).unwrap();
        assert!(stats.groups.contains_key(&24), "23.5 should round to 24");
        assert!(stats.groups.contains_key(&29), "29.4 should round to 29");
    }

    #[test]
    fn tracks_codec_distribution() {
        let files = vec![
            PathBuf::from("/a.mp4"),
            PathBuf::from("/b.mp4"),
            PathBuf::from("/c.webm"),
            PathBuf::from("/d.mkv"),
        ];
        let probe = |path: &Path| -> Result<VideoMetadata, EncodeError> {
            let codec = match path.extension().and_then(|e| e.to_str()) {
                Some("mp4") => "h264",
                Some("webm") => "vp9",
                Some("mkv") => "av1",
                _ => "unknown",
            };
            Ok(VideoMetadata {
                codec: Some(codec.into()),
                avg_fps: Some(30.0),
                ..Default::default()
            })
        };
        let stats = analyze_fps_with(&files, probe).unwrap();
        assert_eq!(stats.codec_counts.get("h264").copied(), Some(2));
        assert_eq!(stats.codec_counts.get("vp9").copied(), Some(1));
        assert_eq!(stats.codec_counts.get("av1").copied(), Some(1));
    }

    #[test]
    fn accumulates_total_duration() {
        let files = vec![
            PathBuf::from("/a.mp4"),
            PathBuf::from("/b.mp4"),
            PathBuf::from("/c.mp4"),
        ];
        let probe = |path: &Path| -> Result<VideoMetadata, EncodeError> {
            let dur = match path.file_name().unwrap().to_str().unwrap() {
                "a.mp4" => 60.0,
                "b.mp4" => 30.5,
                "c.mp4" => 120.0,
                _ => 0.0,
            };
            Ok(VideoMetadata {
                codec: Some("h264".into()),
                avg_fps: Some(30.0),
                duration_secs: Some(dur),
                ..Default::default()
            })
        };
        let stats = analyze_fps_with(&files, probe).unwrap();
        assert!((stats.total_duration_secs - 210.5).abs() < 1e-9);
    }

    #[test]
    fn ignores_missing_duration_and_codec() {
        // Probe returns metadata with codec=None and duration_secs=None;
        // neither codec_counts nor total_duration_secs should change.
        let files = vec![PathBuf::from("/a.mp4")];
        let probe = |_: &Path| {
            Ok(VideoMetadata {
                avg_fps: Some(30.0),
                ..Default::default()
            })
        };
        let stats = analyze_fps_with(&files, probe).unwrap();
        assert_eq!(stats.parsed_files, 1);
        assert!(stats.codec_counts.is_empty());
        assert_eq!(stats.total_duration_secs, 0.0);
    }

    #[test]
    fn failed_probes_dont_contribute_to_codec_or_duration() {
        let files = vec![PathBuf::from("/bad.mp4")];
        let probe = |_: &Path| -> Result<VideoMetadata, EncodeError> {
            Err(EncodeError::Probe("boom".into()))
        };
        let stats = analyze_fps_with(&files, probe).unwrap();
        assert_eq!(stats.failed_files, 1);
        assert!(stats.codec_counts.is_empty());
        assert_eq!(stats.total_duration_secs, 0.0);
    }
}
