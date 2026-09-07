//! Public data structures shared across the library.

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
    /// Stream duration in seconds (from ffprobe's `duration` field).
    /// `None` if the probe didn't report a duration.
    pub duration_secs: Option<f64>,
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

/// Aggregated analysis output. Holds a Polars `DataFrame` (built via
/// `group_by([fps, codec]).agg([count, sum(duration), sum(bytes)])`)
/// plus high-level totals for the CLI summary line and the failed-files
/// list.
#[derive(Debug)]
pub struct FpsStats {
    /// One row per `(fps, codec)` bucket; columns: `fps, codec, files,
    /// duration_secs, bytes`. Failed probes (fps=None) are excluded;
    /// `failed_files` and `failed_paths` track them.
    pub table: polars::prelude::DataFrame,
    pub total_files: usize,
    pub parsed_files: usize,
    pub failed_files: usize,
    pub total_duration_secs: f64,
    pub failed_paths: Vec<PathBuf>,
}

impl FpsStats {
    /// Build an empty `FpsStats` with the given total-file count and an
    /// empty cross-tab table. Tests and early-return paths use this;
    /// production callers should prefer `aggregate_records` to populate
    /// the table.
    pub fn new(total_files: usize) -> Self {
        use polars::prelude::*;
        let table = df! {
            "fps" => Vec::<Option<u32>>::new(),
            "codec" => Vec::<Option<&str>>::new(),
            "files" => Vec::<u32>::new(),
            "duration_secs" => Vec::<Option<f64>>::new(),
            "bytes" => Vec::<u64>::new(),
        }
        .expect("empty aggregate df");
        Self {
            table,
            total_files,
            parsed_files: 0,
            failed_files: 0,
            total_duration_secs: 0.0,
            failed_paths: Vec::new(),
        }
    }

    /// Render as a human-readable summary: a "Total: ..." line, the
    /// Polars cross-tab table (fps x codec), and the failed-files list
    /// if any. The table uses Polars' default formatting.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        if self.total_files > 0 {
            out.push_str(&format!(
                "Total: {} files ({} parsed, {} failed)",
                self.total_files, self.parsed_files, self.failed_files
            ));
            if self.total_duration_secs > 0.0 {
                out.push_str(&format!(", {}", format_duration(self.total_duration_secs)));
            }
            out.push_str("\n\n");
        }
        out.push_str(&format!("{}\n", self.table));
        if !self.failed_paths.is_empty() {
            out.push_str("\nFailed files:\n");
            for p in &self.failed_paths {
                out.push_str(&format!("  {}\n", p.display()));
            }
        }
        out
    }
}

/// Format seconds as `H:MM:SS` (always zero-padded minutes and seconds).
/// Negative inputs are clamped to 0 (defensive against bad ffprobe output).
fn format_duration(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    format!("{h}:{m:02}:{s:02}")
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
            ..Default::default()
        };
        assert_eq!(m.best_fps(), Some(60.0));
    }

    #[test]
    fn video_metadata_best_fps_falls_back_to_nominal() {
        let m = VideoMetadata {
            codec: Some("h264".into()),
            avg_fps: None,
            nominal_fps: Some(30.0),
            ..Default::default()
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

    // ---- FpsStats (Polars-backed) ----

    fn rec(fps: Option<u32>, codec: &str, dur: Option<f64>) -> crate::analyzer::fps_stats::AnalysisRecord {
        crate::analyzer::fps_stats::AnalysisRecord {
            path: PathBuf::from("/x"),
            fps,
            codec: Some(codec.into()),
            duration_secs: dur,
            file_bytes: 1024,
        }
    }

    fn fail_rec() -> crate::analyzer::fps_stats::AnalysisRecord {
        crate::analyzer::fps_stats::AnalysisRecord {
            path: PathBuf::from("/bad.mp4"),
            fps: None,
            codec: None,
            duration_secs: None,
            file_bytes: 0,
        }
    }

    #[test]
    fn fps_stats_new_initializes_total_with_empty_table() {
        let s = FpsStats::new(100);
        assert_eq!(s.total_files, 100);
        assert_eq!(s.parsed_files, 0);
        assert_eq!(s.failed_files, 0);
        assert_eq!(s.table.height(), 0);
        assert!(s.failed_paths.is_empty());
    }

    #[test]
    fn aggregate_records_groups_by_fps_and_codec() {
        let records = vec![
            rec(Some(24), "h264", Some(60.0)),
            rec(Some(30), "h264", Some(60.0)),
            rec(Some(30), "vp9", Some(60.0)),
            rec(Some(60), "av1", Some(120.0)),
        ];
        let stats = crate::analyzer::fps_stats::aggregate_records(&records);
        assert_eq!(stats.parsed_files, 4);
        assert_eq!(stats.failed_files, 0);
        assert_eq!(stats.table.height(), 4); // 4 (fps, codec) combos
        // Total duration: 60 + 60 + 60 + 120 = 300
        assert!((stats.total_duration_secs - 300.0).abs() < 1e-9);
    }

    #[test]
    fn aggregate_records_excludes_failed_from_table() {
        let records = vec![
            rec(Some(30), "h264", Some(60.0)),
            rec(Some(30), "h264", Some(60.0)),
            fail_rec(),
        ];
        let stats = crate::analyzer::fps_stats::aggregate_records(&records);
        assert_eq!(stats.total_files, 3);
        assert_eq!(stats.parsed_files, 2);
        assert_eq!(stats.failed_files, 1);
        assert_eq!(stats.table.height(), 1); // only the (30, h264) row
        assert_eq!(stats.failed_paths, vec![PathBuf::from("/bad.mp4")]);
    }

    #[test]
    fn aggregate_records_empty_input() {
        let stats = crate::analyzer::fps_stats::aggregate_records(&[]);
        assert_eq!(stats.total_files, 0);
        assert_eq!(stats.parsed_files, 0);
        assert_eq!(stats.failed_files, 0);
        assert_eq!(stats.table.height(), 0);
        assert_eq!(stats.total_duration_secs, 0.0);
    }

    #[test]
    fn fps_stats_summary_starts_with_totals_line() {
        let records = vec![
            rec(Some(30), "h264", Some(60.0)),
            rec(Some(60), "av1", Some(120.0)),
        ];
        let stats = crate::analyzer::fps_stats::aggregate_records(&records);
        let summary = stats.summary();
        assert!(summary.contains("Total: 2 files (2 parsed, 0 failed)"));
        assert!(summary.contains("0:03:00")); // 60 + 120 = 180 = 3:00
    }

    #[test]
    fn fps_stats_summary_includes_polars_table_with_fps_codec_files() {
        let records = vec![
            rec(Some(30), "h264", Some(60.0)),
            rec(Some(30), "h264", Some(60.0)),
            rec(Some(60), "av1", Some(120.0)),
        ];
        let stats = crate::analyzer::fps_stats::aggregate_records(&records);
        let summary = stats.summary();
        // Polars' default Display shows column names.
        assert!(summary.contains("fps"));
        assert!(summary.contains("codec"));
        assert!(summary.contains("files"));
        assert!(summary.contains("duration_secs"));
        // Cell values appear in the table.
        assert!(summary.contains("h264"));
        assert!(summary.contains("av1"));
    }

    #[test]
    fn fps_stats_summary_lists_failed_files_at_end() {
        let records = vec![rec(Some(30), "h264", Some(60.0)), fail_rec()];
        let stats = crate::analyzer::fps_stats::aggregate_records(&records);
        let summary = stats.summary();
        assert!(summary.contains("Failed files:"));
        assert!(summary.contains("/bad.mp4"));
    }

    #[test]
    fn fps_stats_summary_omits_total_line_for_empty_input() {
        let stats = crate::analyzer::fps_stats::aggregate_records(&[]);
        let summary = stats.summary();
        assert!(!summary.contains("Total:"));
    }

    #[test]
    fn fps_stats_summary_omits_duration_when_zero() {
        let records = vec![rec(Some(30), "h264", None)];
        let stats = crate::analyzer::fps_stats::aggregate_records(&records);
        let summary = stats.summary();
        assert!(!summary.contains("0:00:00"));
        // No duration column data displayed in the summary line.
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
