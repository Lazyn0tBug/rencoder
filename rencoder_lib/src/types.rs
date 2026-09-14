//! Public data structures shared across the library.

use std::path::PathBuf;

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
    /// Container format (e.g. `"mov,mp4,m4a"`, `"matroska,webm"`).
    /// Distinguishes e.g. `.mp4` containing h264 vs `.mkv` containing h264.
    pub container: Option<String>,
    /// Frame width in pixels.
    pub width: Option<u32>,
    /// Frame height in pixels.
    pub height: Option<u32>,
    /// Video bit rate in bits per second.
    pub bit_rate: Option<u64>,
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

impl EncodeParams {
    /// Build a full `EncodeParams` from the loaded `AppConfig`'s
    /// `encode` section. Missing fields fall back to defaults.
    ///
    /// This is the single source of truth for "how a TOML `[encode]`
    /// block becomes an `EncodeParams`". Both the CLI and the GUI
    /// (`rencoder_ui::msg::build_encode_params_from_config`) call this
    /// — the GUI helper existed first and was duplicated, this commit
    /// consolidates.
    ///
    /// Note: `target` is always `Target::default()` — currently
    /// `[encode] target` is parsed but not consumed by
    /// `transcode_single`. If/when target becomes per-batch
    /// controllable, this function picks it up automatically.
    pub fn from_encode_config(
        encode: Option<&crate::config::app::EncodeConfig>,
    ) -> Self {
        use crate::config::app::EncodeConfig;
        let default_enc = EncodeConfig::default();
        let enc: &EncodeConfig = encode.unwrap_or(&default_enc);
        let format = enc
            .format
            .map(VideoFormat::from)
            .unwrap_or(VideoFormat::Av1);
        let (is_crf, bitrate_or_crf) = if enc.crf == 0 {
            (false, enc.bitrate.unwrap_or(5000))
        } else {
            (true, enc.crf)
        };
        Self {
            format,
            bitrate_or_crf,
            is_crf,
            target: Target::default(),
            extensions: enc.extensions.clone().unwrap_or_default(),
            extra_args: vec![],
            max_fps: MaxFpsConfig {
                max_fps: enc.max_fps.unwrap_or(0) as f64,
                eps: 0.05,
            },
        }
    }
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
    /// Output path the task writes to. `Some` for encode (ffmpeg
    /// output), `None` for analyze (read-only). The signal handler
    /// removes the file at this path on SIGINT when `Some`.
    pub output: Option<PathBuf>,
}

// ===== Work-analysis record (encode pipeline output) =====

/// Encoder settings used for one encode: CRF/bitrate mode + optional
/// preset (libsvtav1 takes a numeric preset, libx264 takes a string).
#[derive(Clone, Debug, Default)]
pub struct EncoderSettings {
    pub is_crf: bool,
    pub crf_or_bitrate: u32,
    pub preset: Option<String>,
    pub max_fps_target: Option<f32>,
}

/// Reason a file was skipped (no work performed).
///
/// `#[non_exhaustive]` (added U21): future variants don't break exhaustive
/// match. `PartialOutput` is reserved for the case where partial-output
/// detection succeeds but removal fails (rare); the happy path is
/// "detect → remove → reencode" which produces a real encode record,
/// not a Skipped one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkippedReason {
    SameCodec,
    OutputExists,
    /// Output exists and is detected as partial (truncated/corrupt),
    /// but removal failed (e.g. permission denied). Encode cannot
    /// proceed because the output is in the way.
    PartialOutput,
}

/// Outcome of a single encode. `Succeeded` and `ReducedFps` produced
/// output; `Skipped` did not; `Failed` did not and has an error context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodingOutcome {
    Succeeded,
    ReducedFps,
    Skipped(SkippedReason),
    Failed,
}

/// Failure categorization for the monitoring dimension (independent of
/// the source/process/result dimensions, which are about a single encode).
///
/// `#[non_exhaustive]` (added U21): symmetric with `Command`/`Event` (see
/// `docs/design.md` §3.8.3 item 33) — future variants do not break
/// exhaustive `match` on `Event::Error { category, .. }`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorCategory {
    ProbeFailed,
    EncoderSelectionFailed,
    FfmpegSpawnFailed,
    FfmpegNonzeroExit,
    ConfigError,
    Cancelled,
    IoError,
}

/// Context captured when an encode fails. Used for monitoring and
/// failure-mode analysis.
///
/// `#[non_exhaustive]` (added U21): symmetric with `ErrorCategory` and
/// `Command`/`Event` (see `docs/design.md` §3.8.3 item 33 / §8 item 36).
/// External crates (e.g. `rencoder_ui`) cannot construct this directly
/// but can still read fields on the value carried by
/// `EncodingRecord.error`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ErrorContext {
    pub category: ErrorCategory,
    pub message: String,
    pub ffmpeg_stderr_tail: String,
}

/// One row per input file, structured for work analysis (not video
/// analysis). Four dimensions:
///
/// 1. **Source** (input): what we started with
/// 2. **Process** (work): timing and encoder choice
/// 3. **Result** (output): what we produced
/// 4. **Status** (independent monitoring): outcome + error context
#[derive(Clone, Debug)]
pub struct EncodingRecord {
    // === 1. Source ===
    pub source_path: String,
    pub source_size_bytes: u64,
    pub source_codec: Option<String>,
    pub source_container: Option<String>,
    pub source_width: Option<u32>,
    pub source_height: Option<u32>,
    pub source_fps: Option<f64>,
    pub source_duration_secs: Option<f64>,
    pub source_bit_rate_bps: Option<u64>,

    // === 2. Process ===
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    pub encoding_duration_secs: Option<f64>,
    pub encoder_selected: Option<String>,
    pub encoder_settings: EncoderSettings,
    pub ffmpeg_argv: Vec<String>,
    pub ffmpeg_stderr_tail: String,

    // === 3. Result ===
    pub output_path: Option<String>,
    pub output_size_bytes: Option<u64>,
    pub output_duration_secs: Option<f64>,
    pub output_codec: Option<String>,
    pub compression_ratio: Option<f64>,

    // === 4. Status (independent monitoring) ===
    pub outcome: EncodingOutcome,
    pub error: Option<ErrorContext>,
}

// Re-export for backwards compat with existing call sites (the encode
// pipeline currently uses `TranscodeResult`).
pub use crate::types::EncodingOutcome as _EncodingOutcomeReexport;
pub type TranscodeResult = EncodingOutcome;

// ===== FPS statistics (analyzer module) =====

/// Aggregated analysis output. Holds a Polars `DataFrame` (built via
/// `group_by([fps, codec]).agg([count, sum(duration), sum(bytes)])`)
/// plus high-level totals for the CLI summary line and the failed-files
/// list.
#[derive(Debug)]
pub struct VideoStats {
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

impl VideoStats {
    /// Build an empty `VideoStats` with the given total-file count and an
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
        assert!(VideoMetadata {
            codec: Some("h264".into()),
            ..Default::default()
        }
        .is_valid());
    }

    #[test]
    fn video_metadata_is_valid_with_avg_fps() {
        assert!(VideoMetadata {
            avg_fps: Some(30.0),
            ..Default::default()
        }
        .is_valid());
    }

    #[test]
    fn video_metadata_is_valid_with_nominal_fps() {
        assert!(VideoMetadata {
            nominal_fps: Some(30.0),
            ..Default::default()
        }
        .is_valid());
    }

    #[test]
    fn max_fps_config_enabled_zero_is_false() {
        assert!(!MaxFpsConfig {
            max_fps: 0.0,
            eps: 0.05
        }
        .enabled());
    }

    #[test]
    fn max_fps_config_enabled_positive_is_true() {
        assert!(MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.05
        }
        .enabled());
    }

    #[test]
    fn max_fps_config_enabled_negative_is_false() {
        assert!(!MaxFpsConfig {
            max_fps: -30.0,
            eps: 0.05
        }
        .enabled());
    }

    // need_reduce: origin §6 matrix.
    #[test]
    fn need_reduce_25fps_does_not_reduce() {
        let c = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.05,
        };
        assert!(!c.need_reduce(25.0));
    }

    #[test]
    fn need_reduce_30fps_does_not_reduce() {
        let c = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.05,
        };
        assert!(!c.need_reduce(30.0));
    }

    #[test]
    fn need_reduce_29_97_does_not_reduce() {
        let c = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.05,
        };
        assert!(!c.need_reduce(29.97));
    }

    #[test]
    fn need_reduce_60fps_does_reduce() {
        let c = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.05,
        };
        assert!(c.need_reduce(60.0));
    }

    #[test]
    fn need_reduce_just_above_cap_eps_does_reduce() {
        let c = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.05,
        };
        assert!(c.need_reduce(30.06));
    }

    #[test]
    fn need_reduce_with_zero_eps_is_strict() {
        let c = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.0,
        };
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

    // ---- VideoStats (Polars-backed) ----

    fn rec(
        fps: Option<u32>,
        codec: &str,
        dur: Option<f64>,
    ) -> crate::analyzer::records::AnalysisRecord {
        crate::analyzer::records::AnalysisRecord {
            path: PathBuf::from("/x"),
            fps,
            codec: Some(codec.into()),
            duration_secs: dur,
            file_bytes: 1024,
        }
    }

    fn fail_rec() -> crate::analyzer::records::AnalysisRecord {
        crate::analyzer::records::AnalysisRecord {
            path: PathBuf::from("/bad.mp4"),
            fps: None,
            codec: None,
            duration_secs: None,
            file_bytes: 0,
        }
    }

    #[test]
    fn fps_stats_new_initializes_total_with_empty_table() {
        let s = VideoStats::new(100);
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
        let stats = crate::analyzer::records::aggregate_records(&records);
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
        let stats = crate::analyzer::records::aggregate_records(&records);
        assert_eq!(stats.total_files, 3);
        assert_eq!(stats.parsed_files, 2);
        assert_eq!(stats.failed_files, 1);
        assert_eq!(stats.table.height(), 1); // only the (30, h264) row
        assert_eq!(stats.failed_paths, vec![PathBuf::from("/bad.mp4")]);
    }

    #[test]
    fn aggregate_records_empty_input() {
        let stats = crate::analyzer::records::aggregate_records(&[]);
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
        let stats = crate::analyzer::records::aggregate_records(&records);
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
        let stats = crate::analyzer::records::aggregate_records(&records);
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
        let stats = crate::analyzer::records::aggregate_records(&records);
        let summary = stats.summary();
        assert!(summary.contains("Failed files:"));
        assert!(summary.contains("/bad.mp4"));
    }

    #[test]
    fn fps_stats_summary_omits_total_line_for_empty_input() {
        let stats = crate::analyzer::records::aggregate_records(&[]);
        let summary = stats.summary();
        assert!(!summary.contains("Total:"));
    }

    #[test]
    fn fps_stats_summary_omits_duration_when_zero() {
        let records = vec![rec(Some(30), "h264", None)];
        let stats = crate::analyzer::records::aggregate_records(&records);
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
