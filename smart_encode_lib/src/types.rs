//! Public data structures shared across the library.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::error::EncodeError;

// ===== Target format =====

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoFormat {
    #[default]
    X264,
    Hevc,
    Vp9,
    Av1,
}

impl VideoFormat {
    /// Default container extension for this format.
    pub fn default_extension(self) -> &'static str {
        match self {
            VideoFormat::X264 | VideoFormat::Hevc => "mp4",
            VideoFormat::Vp9 => "webm",
            VideoFormat::Av1 => "mkv",
        }
    }
}

// ===== Video metadata (one probe, many fields) =====

#[derive(Clone, Debug, Default)]
pub struct VideoMetadata {
    pub codec: Option<String>,
    pub avg_fps: Option<f64>,
    pub nominal_fps: Option<f64>,
}

impl VideoMetadata {
    /// Prefer `avg_frame_rate`, fall back to `r_frame_rate`.
    pub fn best_fps(&self) -> Option<f64> {
        self.avg_fps.or(self.nominal_fps)
    }

    pub fn is_valid(&self) -> bool {
        self.codec.is_some() || self.avg_fps.is_some() || self.nominal_fps.is_some()
    }
}

// ===== Max-FPS configuration =====
//
// Predicate lives here; parameter mapping (`-r <fps>`) lives in
// `ffmpeg::command::build_ffmpeg_command`. Single-responsibility split
// keeps the decision testable without ffmpeg invocation.

#[derive(Clone, Debug)]
pub struct MaxFpsConfig {
    pub max_fps: f64,
    /// Tolerance band around `max_fps` (matches `encode.zsh` semantics:
    /// a 29.97 source at cap=30 is NOT reduced because 29.97 < 30 + 0.05).
    pub eps: f64,
}

impl Default for MaxFpsConfig {
    fn default() -> Self {
        Self {
            max_fps: 0.0,
            eps: 0.05,
        }
    }
}

impl MaxFpsConfig {
    pub fn enabled(&self) -> bool {
        self.max_fps > 0.0
    }

    /// Decide whether the source frame rate exceeds the cap.
    ///
    /// Preconditions (caller-enforced):
    /// - `src_fps` is positive and finite
    /// - `self.enabled()` is true (i.e. `max_fps > 0`)
    ///
    /// Semantics (per `encode.zsh` and design doc §6 test matrix):
    /// - strict `>` so `src_fps == max_fps` is NOT reduced
    /// - epsilon band `[max_fps, max_fps + eps]` is treated as "no reduce"
    ///   so a 29.97 source at cap=30 stays untouched
    pub fn need_reduce(&self, src_fps: f64) -> bool {
        src_fps > self.max_fps + self.eps
    }
}

// ===== FFmpeg process output handling =====

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessOutput {
    /// Pass through to the parent process (matches the shell-script default).
    #[default]
    Inherit,
    /// Capture stdout/stderr via pipes (GUI uses this).
    Piped,
    /// Discard output.
    Null,
}

// ===== Encoder cache =====

#[derive(Debug)]
pub struct EncoderCache {
    pub encoders_output: String,
    pub has_nvidia: bool,
}

impl EncoderCache {
    /// Substring match against `ffmpeg -encoders` output (matches `encode.zsh`).
    ///
    /// Known limitation: a shorter encoder name that is a substring of another
    /// could yield a false positive. Accepted to stay aligned with the script.
    pub fn has_encoder(&self, name: &str) -> bool {
        self.encoders_output.contains(name)
    }
}

// ===== Encode parameters =====

#[derive(Clone, Debug, Default)]
pub struct EncodeParams {
    pub format: VideoFormat,
    pub bitrate_or_crf: u32,
    pub is_crf: bool,
    pub target: Target,
    pub extensions: Vec<String>,
    pub extra_args: Vec<String>,
    pub max_fps: MaxFpsConfig,
    // Note: NVIDIA enablement lives on `EncoderCache.has_nvidia` after
    // `init_encoder_cache(enable_nv)` consumes the `--nv` flag. Keeping a
    // separate `enable_nv` field here would be dead state.
}

#[derive(Clone, Debug)]
pub enum Target {
    File(PathBuf),
    Dir(PathBuf),
}

impl Default for Target {
    fn default() -> Self {
        Target::File(PathBuf::new())
    }
}

// ===== Single transcode task =====

#[derive(Clone, Debug)]
pub struct TranscodeTask {
    pub input: PathBuf,
    pub output: PathBuf,
}

// ===== Transcode result status =====

#[derive(Debug)]
pub enum TranscodeResult {
    Success,
    SkippedSameCodec,
    SkippedExists,
    ReducedFps,
    Failed(EncodeError),
}

// ===== FPS statistics (analyzer module) =====

#[derive(Clone, Debug, Default)]
pub struct FpsStats {
    pub groups: BTreeMap<u32, Vec<PathBuf>>,
    pub total_files: usize,
    pub parsed_files: usize,
    pub failed_files: usize,
}

impl FpsStats {
    pub fn new(total_files: usize) -> Self {
        Self {
            total_files,
            ..Default::default()
        }
    }

    pub fn add(&mut self, fps: u32, path: PathBuf) {
        self.groups.entry(fps).or_default().push(path);
        self.parsed_files += 1;
    }

    pub fn add_failed(&mut self) {
        self.failed_files += 1;
    }

    /// Group counts plus a totals line. Suitable for default CLI output.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for (fps, files) in &self.groups {
            out.push_str(&format!("{} fps: {} files\n", fps, files.len()));
        }
        out.push_str(&self.totals_line());
        out
    }

    /// Per-file listing grouped by fps, plus totals. Used with `-v`.
    pub fn detailed(&self) -> String {
        let mut out = String::new();
        for (fps, files) in &self.groups {
            out.push_str(&format!("{} fps ({} files):\n", fps, files.len()));
            for path in files {
                out.push_str(&format!("  {}\n", path.display()));
            }
        }
        out.push_str(&self.totals_line());
        out
    }

    fn totals_line(&self) -> String {
        format!(
            "Total: {} files ({} parsed, {} failed)\n",
            self.total_files, self.parsed_files, self.failed_files
        )
    }
}

// ===== Analyze parameters =====

#[derive(Clone, Debug)]
pub struct AnalyzeParams {
    pub target: Target,
    pub extensions: Vec<String>,
    pub parallel: bool,
    pub verbose: bool,
    pub output_file: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_format_default_extension() {
        assert_eq!(VideoFormat::X264.default_extension(), "mp4");
        assert_eq!(VideoFormat::Hevc.default_extension(), "mp4");
        assert_eq!(VideoFormat::Vp9.default_extension(), "webm");
        assert_eq!(VideoFormat::Av1.default_extension(), "mkv");
    }

    #[test]
    fn video_metadata_best_fps_prefers_avg() {
        let m = VideoMetadata {
            codec: Some("h264".into()),
            avg_fps: Some(60.0),
            nominal_fps: Some(30.0),
        };
        assert_eq!(m.best_fps(), Some(60.0));
    }

    #[test]
    fn video_metadata_best_fps_falls_back_to_nominal() {
        let m = VideoMetadata {
            codec: Some("h264".into()),
            avg_fps: None,
            nominal_fps: Some(30.0),
        };
        assert_eq!(m.best_fps(), Some(30.0));
    }

    #[test]
    fn video_metadata_best_fps_none_when_both_missing() {
        let m = VideoMetadata::default();
        assert_eq!(m.best_fps(), None);
    }

    #[test]
    fn video_metadata_is_valid_when_empty() {
        assert!(!VideoMetadata::default().is_valid());
    }

    #[test]
    fn video_metadata_is_valid_with_codec() {
        assert!(
            VideoMetadata {
                codec: Some("h264".into()),
                ..Default::default()
            }
            .is_valid()
        );
    }

    #[test]
    fn video_metadata_is_valid_with_avg_fps() {
        assert!(
            VideoMetadata {
                avg_fps: Some(30.0),
                ..Default::default()
            }
            .is_valid()
        );
    }

    #[test]
    fn video_metadata_is_valid_with_nominal_fps() {
        assert!(
            VideoMetadata {
                nominal_fps: Some(30.0),
                ..Default::default()
            }
            .is_valid()
        );
    }

    #[test]
    fn max_fps_config_enabled_zero_is_false() {
        assert!(!MaxFpsConfig { max_fps: 0.0, eps: 0.05 }.enabled());
    }

    #[test]
    fn max_fps_config_enabled_positive_is_true() {
        assert!(MaxFpsConfig { max_fps: 30.0, eps: 0.05 }.enabled());
    }

    #[test]
    fn max_fps_config_enabled_negative_is_false() {
        assert!(!MaxFpsConfig { max_fps: -30.0, eps: 0.05 }.enabled());
    }

    // need_reduce: origin §6 matrix.
    #[test]
    fn need_reduce_25fps_does_not_reduce() {
        let c = MaxFpsConfig { max_fps: 30.0, eps: 0.05 };
        assert!(!c.need_reduce(25.0));
    }

    #[test]
    fn need_reduce_30fps_does_not_reduce() {
        let c = MaxFpsConfig { max_fps: 30.0, eps: 0.05 };
        assert!(!c.need_reduce(30.0));
    }

    #[test]
    fn need_reduce_29_97_does_not_reduce() {
        let c = MaxFpsConfig { max_fps: 30.0, eps: 0.05 };
        assert!(!c.need_reduce(29.97));
    }

    #[test]
    fn need_reduce_60fps_does_reduce() {
        let c = MaxFpsConfig { max_fps: 30.0, eps: 0.05 };
        assert!(c.need_reduce(60.0));
    }

    #[test]
    fn need_reduce_just_above_cap_eps_does_reduce() {
        let c = MaxFpsConfig { max_fps: 30.0, eps: 0.05 };
        assert!(c.need_reduce(30.06));
    }

    #[test]
    fn need_reduce_with_zero_eps_is_strict() {
        let c = MaxFpsConfig { max_fps: 30.0, eps: 0.0 };
        assert!(!c.need_reduce(30.0));
        assert!(c.need_reduce(30.01));
    }

    #[test]
    fn max_fps_config_default_is_disabled() {
        let c = MaxFpsConfig::default();
        assert!(!c.enabled());
        assert_eq!(c.eps, 0.05);
    }

    #[test]
    fn process_output_default_is_inherit() {
        assert_eq!(ProcessOutput::default(), ProcessOutput::Inherit);
    }

    #[test]
    fn fps_stats_new_initializes_total() {
        let s = FpsStats::new(100);
        assert_eq!(s.total_files, 100);
        assert_eq!(s.parsed_files, 0);
        assert_eq!(s.failed_files, 0);
        assert!(s.groups.is_empty());
    }

    #[test]
    fn fps_stats_add_groups_by_fps() {
        let mut s = FpsStats::new(3);
        s.add(30, PathBuf::from("/a.mp4"));
        s.add(30, PathBuf::from("/b.mp4"));
        s.add(60, PathBuf::from("/c.mp4"));
        assert_eq!(s.parsed_files, 3);
        assert_eq!(s.groups.get(&30).unwrap().len(), 2);
        assert_eq!(s.groups.get(&60).unwrap().len(), 1);
        assert!(s.groups.get(&24).is_none());
    }

    #[test]
    fn fps_stats_add_failed_increments_only_failed() {
        let mut s = FpsStats::new(5);
        s.add(30, PathBuf::from("/a.mp4"));
        s.add_failed();
        s.add_failed();
        assert_eq!(s.failed_files, 2);
        assert_eq!(s.parsed_files, 1);
    }

    #[test]
    fn fps_stats_summary_lists_each_group_count_and_totals() {
        let mut s = FpsStats::new(3);
        s.add(24, PathBuf::from("/a.mp4"));
        s.add(30, PathBuf::from("/b.mp4"));
        s.add(30, PathBuf::from("/c.mp4"));
        let summary = s.summary();
        assert!(summary.contains("24 fps: 1 files"));
        assert!(summary.contains("30 fps: 2 files"));
        assert!(summary.contains("Total: 3 files (3 parsed, 0 failed)"));
    }

    #[test]
    fn fps_stats_summary_empty_has_totals_line() {
        let s = FpsStats::new(0);
        let summary = s.summary();
        assert!(summary.contains("Total: 0 files (0 parsed, 0 failed)"));
        // No groups means no group lines.
        assert!(!summary.contains("fps:"));
    }

    #[test]
    fn fps_stats_summary_reflects_failures() {
        let mut s = FpsStats::new(4);
        s.add(30, PathBuf::from("/a.mp4"));
        s.add(30, PathBuf::from("/b.mp4"));
        s.add_failed();
        s.add_failed();
        let summary = s.summary();
        assert!(summary.contains("30 fps: 2 files"));
        assert!(summary.contains("Total: 4 files (2 parsed, 2 failed)"));
    }

    #[test]
    fn fps_stats_detailed_lists_every_path_under_its_group() {
        let mut s = FpsStats::new(3);
        s.add(24, PathBuf::from("/a.mp4"));
        s.add(30, PathBuf::from("/b.mp4"));
        s.add(30, PathBuf::from("/c.mp4"));
        let detailed = s.detailed();
        assert!(detailed.contains("/a.mp4"));
        assert!(detailed.contains("/b.mp4"));
        assert!(detailed.contains("/c.mp4"));
        assert!(detailed.contains("24 fps (1 files):"));
        assert!(detailed.contains("30 fps (2 files):"));
        assert!(detailed.contains("Total: 3 files (3 parsed, 0 failed)"));
    }

    #[test]
    fn fps_stats_detailed_groups_paths_under_correct_fps() {
        let mut s = FpsStats::new(2);
        s.add(24, PathBuf::from("/24a.mp4"));
        s.add(60, PathBuf::from("/60a.mp4"));
        let detailed = s.detailed();
        // Find the 24 fps section and check it only contains /24a.mp4.
        let lines: Vec<&str> = detailed.lines().collect();
        let mut in_24_section = false;
        let mut in_60_section = false;
        for line in &lines {
            if line.starts_with("24 fps") {
                in_24_section = true;
                in_60_section = false;
            } else if line.starts_with("60 fps") {
                in_60_section = true;
                in_24_section = false;
            } else if let Some(stripped) = line.strip_prefix("  ") {
                // Path lines are indented with 2 spaces.
                if in_24_section {
                    assert!(stripped.contains("/24a.mp4"));
                    assert!(!stripped.contains("/60a.mp4"));
                } else if in_60_section {
                    assert!(stripped.contains("/60a.mp4"));
                    assert!(!stripped.contains("/24a.mp4"));
                }
            }
        }
    }

    #[test]
    fn encoder_cache_has_encoder_substring() {
        let cache = EncoderCache {
            encoders_output: "libx264 libsvtav1".to_string(),
            has_nvidia: false,
        };
        assert!(cache.has_encoder("libx264"));
        assert!(cache.has_encoder("libsvtav1"));
        assert!(!cache.has_encoder("libdav1d"));
        // Empty cache returns false for any non-empty needle.
        let empty = EncoderCache {
            encoders_output: String::new(),
            has_nvidia: false,
        };
        assert!(!empty.has_encoder("libx264"));
        // Empty needle matches even empty cache (str::contains("") == true).
        // This is the property the parser filters against, not has_encoder
        // itself.
        assert!(empty.has_encoder(""));
    }
}
