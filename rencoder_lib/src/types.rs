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
    ///
    /// v1.2 follow-up #3: X264 / HEVC / AV1 all produce `.mkv`.
    /// VP9 stays `.webm` (VP9-in-WebM is the dominant browser-
    /// facing format; AV1/WebM is less common and we don't auto-
    /// promote). `output_path_for` adds a `_<ext>`-suffix to
    /// disambiguate when the default would collide with the
    /// input extension — see that function for the full matrix.
    pub fn default_extension(self) -> &'static str {
        match self {
            VideoFormat::Vp9 => "webm",
            VideoFormat::X264 | VideoFormat::Hevc | VideoFormat::Av1 => "mkv",
        }
    }
}

// ===== Video metadata (one probe, many fields) =====

#[derive(Clone, Debug, Default)]
pub struct VideoMetadata {
    pub codec: Option<String>,
    /// 平均帧率（ffprobe 的 `avg_frame_rate`）。
    ///
    /// VFR 素材会被大量长间隔拉低，**不能**用它判定帧率上限；
    /// 只用于展示与统计。
    pub avg_fps: Option<f64>,
    /// 瞬时帧率上限（ffprobe 的 `r_frame_rate`）。
    ///
    /// 由最短帧间隔反推，是素材真实的速度上限，**帧率上限判定用它**。
    /// 命名沿用 ffprobe 的 "real base frame rate"，语义上等于脚本里的
    /// "峰值帧率"。
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
    ///
    /// 用于**展示与统计**（分析器的 fps 列、记录表的 source_fps）。
    /// 帧率上限判定不要用它，见 [`VideoMetadata::cap_fps`]。
    pub fn best_fps(&self) -> Option<f64> {
        self.avg_fps.or(self.nominal_fps)
    }

    /// 帧率上限判定用的帧率（`encode.zsh` v10.2.0 语义）。
    ///
    /// 为什么不能直接用 [`VideoMetadata::best_fps`]：VFR 素材的
    /// `avg_frame_rate` 会被大量长间隔拉低（大部分时间 24fps、局部 60fps
    /// 的实录，平均值可能只有 30fps）。拿它跟上限比较，会把"局部超速"
    /// 误判成"未超限"，于是 60fps 段落原样留在产物里。脚本 v10.2.0 正是
    /// 为此把判定源从平均帧率换成 `r_frame_rate`（由最短帧间隔反推的瞬时
    /// 上限），平均帧率只留作日志展示。
    ///
    /// 兜底顺序与脚本一致：峰值 ?: 平均。
    ///
    /// 上限判定的**所有**调用点都必须走这个方法（跳过判定、argv 参数
    /// 映射、结果记录三处），否则会出现"记录说降了帧但命令里没有 -r"
    /// 这类自相矛盾。
    pub fn cap_fps(&self) -> Option<f64> {
        // Instantaneous ceiling first, average as the fallback — the
        // order `encode.zsh` v10.2.0 uses (`peak ?: avg`).
        self.nominal_fps.or(self.avg_fps)
    }

    pub fn is_valid(&self) -> bool {
        self.codec.is_some() || self.avg_fps.is_some() || self.nominal_fps.is_some()
    }
}

// ===== Max-FPS configuration =====
//
// Predicate lives here; parameter mapping (`-r <fps>` / `-fps_mode`)
// lives in `ffmpeg::command::build_ffmpeg_command`. Single-responsibility
// split keeps the decision testable without ffmpeg invocation.

#[derive(Clone, Debug)]
pub struct MaxFpsConfig {
    pub max_fps: f64,
    /// Tolerance band around `max_fps` (matches `encode.zsh` semantics:
    /// `MAX_FPS_EPS = 0.01`, comparison `src > cap + eps`). A 29.97
    /// source at cap=30 is NOT reduced because 29.97 < 30 + 0.01.
    pub eps: f64,
}

impl Default for MaxFpsConfig {
    fn default() -> Self {
        Self {
            max_fps: 0.0,
            eps: 0.01,
        }
    }
}

/// The frame-rate action decided for one source file.
///
/// The decision and the argv mapping must come from the same place:
/// `should_skip_encoding` (skip?), the ffmpeg argv (`-r` / `-fps_mode`?)
/// and the result record (`ReducedFps`?) all read this value. Computing
/// "is it reduced?" independently in three places is how the shell script
/// ended up with a skip predicate that silently stopped working
/// (`encode.zsh` v10.2.0: `MAX_FPS_ARGS` became non-empty as soon as
/// `-fps_mode passthrough` was added).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FpsDecision {
    /// No cap configured (`max_fps == 0`): no frame-rate options at all.
    Disabled,
    /// Cap configured but the source frame rate is unknown: no
    /// frame-rate options. We'd rather not transcode with a guessed
    /// frame rate than corrupt the output.
    Unknown,
    /// Source is within the cap: `-fps_mode passthrough` (keep source
    /// timestamps, no duplicated or dropped frames).
    Passthrough,
    /// Source exceeds the cap: `-r <target> -fps_mode cfr` (CFR output,
    /// container frame-rate tag follows the target).
    Reduce { target: f64 },
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

    /// Decide the frame-rate action for one source.
    ///
    /// `src_fps` must come from [`VideoMetadata::cap_fps`] (the source's
    /// instantaneous frame-rate ceiling), not from
    /// [`VideoMetadata::best_fps`] (the average): VFR material has an
    /// average dragged down by long intervals, which hides local
    /// over-speed — see design doc §2.6.
    pub fn decide(&self, src_fps: Option<f64>) -> FpsDecision {
        if !self.enabled() {
            return FpsDecision::Disabled;
        }
        let Some(src) = src_fps else {
            return FpsDecision::Unknown;
        };
        if self.need_reduce(src) {
            FpsDecision::Reduce {
                target: self.max_fps,
            }
        } else {
            FpsDecision::Passthrough
        }
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

/// Result of the NVENC capability probe for one encoder.
///
/// "The ffmpeg build has `av1_nvenc`" and "this machine has an NVIDIA
/// GPU" are both necessary but not sufficient: `av1_nvenc` needs compute
/// capability >= 8.0, and a mismatched driver or an exhausted NVENC
/// session quota fails the same way. The probe is the only answer that
/// covers the whole stack.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct NvencSupport {
    /// Encoder name, e.g. `"av1_nvenc"`.
    pub encoder: String,
    /// Did the 1-frame test encode succeed on this machine?
    pub usable: bool,
    /// Why it failed (an excerpt of ffmpeg's stderr). `None` when usable.
    pub reason: Option<String>,
}

#[derive(Clone, Debug)]
pub struct EncoderCache {
    /// Raw `ffmpeg -encoders` output.
    pub encoders_output: String,
    /// NVENC capability probe results, one entry per NVENC encoder that
    /// this ffmpeg build has. Empty when NVENC is disabled.
    pub nvenc: Vec<NvencSupport>,
}

impl EncoderCache {
    /// Substring match against `ffmpeg -encoders` output (matches `encode.zsh`).
    ///
    /// Known limitation: a shorter encoder name that is a substring of another
    /// could yield a false positive. Accepted to stay aligned with the script.
    pub fn has_encoder(&self, name: &str) -> bool {
        self.encoders_output.contains(name)
    }

    /// Can this NVENC encoder actually run on this machine?
    pub fn nvenc_usable(&self, name: &str) -> bool {
        self.nvenc.iter().any(|s| s.encoder == name && s.usable)
    }

    /// Why the capability probe rejected this NVENC encoder.
    pub fn nvenc_failure_reason(&self, name: &str) -> Option<&str> {
        self.nvenc
            .iter()
            .find(|s| s.encoder == name && !s.usable)
            .and_then(|s| s.reason.as_deref())
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
    /// User-controlled extra ffmpeg argv entries (CLI `--` passthrough,
    /// future `rencoder.toml [encode] extra_args`). Type-level boundary
    /// via [`crate::encoder::ffmpeg_arg::ExtraArg`]: every entry has
    /// been validated by `ExtraArg::parse` at construction, so
    /// `build_ffmpeg_command` can splice them into the argv without
    /// any runtime check.
    pub extra_args: Vec<crate::encoder::ffmpeg_arg::ExtraArg>,
    pub max_fps: MaxFpsConfig,
    // Note: NVIDIA enablement lives on `EncoderCache.nvenc` after
    // `init_encoder_cache(enable_nv)` runs the capability probe. Keeping a
    // separate `enable_nv` field here would be dead state.
}

impl EncodeParams {
    // No `validate_extra_args` method: the type-level boundary via
    // `Vec<ExtraArg>` makes it impossible to construct an
    // `EncodeParams` containing an unvalidated entry. CLI / TOML
    // parsing must call `ExtraArg::parse` per element; failing entries
    // surface as `ExtraArgParseError` and never reach this struct.

    /// Build a full `EncodeParams` from the loaded `AppConfig`'s
    /// `encode` section. Missing fields fall back to defaults.
    ///
    /// Review #123: renamed from `from_encode_config` to
    /// `from_app_config` to disambiguate from
    /// [`crate::config::Config::from_encode_config`], which has the
    /// same name but a different job (sub-config extraction).
    /// Both existed side by side; the disambiguation makes it
    /// clear at the call site which layer is being constructed.
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
    pub fn from_app_config(encode: Option<&crate::config::app::EncodeConfig>) -> Self {
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
                // Single source for the tolerance band: don't repeat the
                // constant here (the config path is the production one).
                eps: MaxFpsConfig::default().eps,
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

// ===== Input path safety boundary (review #109) =====
//
// Type-level guarantee that the input path passed to ffmpeg can't
// start with `-` (which ffmpeg would parse as an option). Pairs
// with `ExtraArg` (§3.3.7) on the argv side: both push the
// validation to the type system so a runtime check can't be
// forgotten.
//
// Status: WIRED THROUGH. `TranscodeTask.input`, `Command::*` file
// vectors, and `Event::FileStarted.source` are all `InputPath` now.
// The only raw `PathBuf` paths in ffmpeg-bound argv come from
// `TranscodeTask.output` (the wrapper writes there; ffmpeg never
// parses it as an option) and from `EncodingRecord.source_path`
// (a serialised string for Parquet, not a runtime invariant).
//
// Rejected forms:
//   - empty path
//   - any path whose final component starts with `-` — covers the
//     classic `-vf_negate.mp4` case AND any sub-path with a
//     dash-prefixed leaf (`/tmp/-secret`). POSIX-style paths are
//     always relative-or-absolute + components; we only need to
//     inspect the filename.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct InputPath(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InputPathError {
    #[error("empty input path")]
    Empty,
    #[error("input path filename starts with `-` (would be parsed by ffmpeg as an option): {0:?}")]
    DashPrefixed(String),
}

impl InputPath {
    /// Validate + wrap a path. Returns `Err` for the rejected forms
    /// listed in the type-level docs above.
    pub fn new(p: &std::path::Path) -> Result<Self, InputPathError> {
        let s = p.to_string_lossy();
        if s.is_empty() {
            return Err(InputPathError::Empty);
        }
        // Path::file_name() returns None for paths ending in `..`
        // or `/`, but those are pathological for ffmpeg input anyway.
        // Reject via filename = None (also caught by dash check on
        // the full string, but explicit is clearer).
        let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or(&s);
        if fname.starts_with('-') {
            return Err(InputPathError::DashPrefixed(s.into_owned()));
        }
        Ok(Self(s.into_owned()))
    }

    /// Borrow the underlying path string.
    pub fn as_path(&self) -> &std::path::Path {
        std::path::Path::new(&self.0)
    }

    /// Consume and return the owned `String` (lossy round-trip via
    /// `PathBuf::display()`).
    pub fn display(&self) -> std::path::Display<'_> {
        std::path::Path::new(&self.0).display()
    }
}

impl std::fmt::Display for InputPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.display().fmt(f)
    }
}

// ===== Single transcode task =====

#[derive(Clone, Debug)]
pub struct TranscodeTask {
    /// Source path the task reads. Type-level safety boundary via
    /// [`InputPath`] (review #109): dash-prefixed filenames are
    /// rejected at construction, so `build_ffmpeg_command` can
    /// pass this through to argv without any runtime check.
    pub input: InputPath,
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

impl EncodingRecord {
    /// Build a record populated from `metadata` + `input_path` +
    /// `source_size_bytes`. The 7 `source_*` fields (codec / container /
    /// width / height / fps / duration / bit_rate) are filled from
    /// `metadata`; the rest of the record is left at defaults that the
    /// caller fills in per outcome (process / result / status).
    ///
    /// Review #124: this deduplicates the source-field assignment
    /// that was repeated in `make_planned_ready_record`,
    /// `make_skipped_record`, `make_failed_probe_record`, and
    /// `build_encode_record`'s success arm — four sites building
    /// the same 7-field block from the same `&VideoMetadata`.
    pub fn from_metadata(
        input_path: &crate::types::InputPath,
        source_size_bytes: u64,
        metadata: Option<&VideoMetadata>,
    ) -> Self {
        use chrono::Utc;
        Self {
            source_path: input_path.to_string(),
            source_size_bytes,
            source_codec: metadata.and_then(|m| m.codec.clone()),
            source_container: metadata.and_then(|m| m.container.clone()),
            source_width: metadata.and_then(|m| m.width),
            source_height: metadata.and_then(|m| m.height),
            source_fps: metadata.and_then(|m| m.best_fps()),
            source_duration_secs: metadata.and_then(|m| m.duration_secs),
            source_bit_rate_bps: metadata.and_then(|m| m.bit_rate),
            // Caller fills in the process / result / status fields.
            started_at: Utc::now(),
            ended_at: None,
            encoding_duration_secs: None,
            encoder_selected: None,
            encoder_settings: EncoderSettings::default(),
            ffmpeg_argv: Vec::new(),
            ffmpeg_stderr_tail: String::new(),
            output_path: None,
            output_size_bytes: None,
            output_duration_secs: None,
            output_codec: None,
            compression_ratio: None,
            outcome: EncodingOutcome::Succeeded,
            error: None,
        }
    }
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
        // v1.2 follow-up #3: X264/HEVC/AV1 → mkv, VP9 → webm.
        assert_eq!(VideoFormat::X264.default_extension(), "mkv");
        assert_eq!(VideoFormat::Hevc.default_extension(), "mkv");
        assert_eq!(VideoFormat::Vp9.default_extension(), "webm");
        assert_eq!(VideoFormat::Av1.default_extension(), "mkv");
    }

    // Review delta #6 (delta-review): regression coverage for
    // EncodeParams::from_app_config (renamed in review #123).
    // Without this test the rename + downstream callers could
    // silently regress.
    #[test]
    fn from_app_config_uses_defaults_when_no_encode_section() {
        let params = EncodeParams::from_app_config(None);
        assert!(matches!(params.target, Target::File(p) if p.as_os_str().is_empty()));
        assert_eq!(params.extensions, Vec::<String>::new());
        assert!(params.extra_args.is_empty());
        // EncodeConfig::default() sets crf = 45 (libsvtav1 medium),
        // so is_crf is true and bitrate_or_crf carries the CRF value.
        // (crf == 0 is the bitrate-mode sentinel.)
        assert!(params.is_crf);
        assert_eq!(params.bitrate_or_crf, 45);
        assert_eq!(params.max_fps.max_fps, 0.0);
        assert!(matches!(params.format, VideoFormat::Av1));
    }

    #[test]
    fn from_app_config_picks_up_max_fps_and_format() {
        let enc = crate::config::app::EncodeConfig {
            format: Some(crate::config::app::FormatArg::Hevc),
            max_fps: Some(30),
            crf: 28,
            bitrate: Some(5000),
            nv: Some(false),
            extensions: Some(vec!["mov".into()]),
            // v1.2 follow-up #5: tests don't exercise the cap; default
            // is fine (matches the serde default).
            max_batch_size: crate::config::app::default_max_batch_size(),
            output_dir: None,
            encoders: None,
            ffmpeg_args: Default::default(),
        };
        let params = EncodeParams::from_app_config(Some(&enc));
        assert!(matches!(params.format, VideoFormat::Hevc));
        assert_eq!(params.max_fps.max_fps, 30.0);
        assert!(params.is_crf);
        assert_eq!(params.bitrate_or_crf, 28);
        assert_eq!(params.extensions, vec!["mov"]);
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

    // ---- cap_fps: 上限判定取值（design doc §2.6） ----

    #[test]
    fn cap_fps_prefers_instantaneous_ceiling_over_average() {
        // VFR 素材：平均 24fps，但局部冲到 60fps。上限判定必须看 60。
        let m = VideoMetadata {
            codec: Some("h264".into()),
            avg_fps: Some(24.0),
            nominal_fps: Some(60.0),
            ..Default::default()
        };
        assert_eq!(m.cap_fps(), Some(60.0));
    }

    #[test]
    fn cap_fps_falls_back_to_average_when_ceiling_missing() {
        let m = VideoMetadata {
            codec: Some("h264".into()),
            avg_fps: Some(24.0),
            nominal_fps: None,
            ..Default::default()
        };
        assert_eq!(m.cap_fps(), Some(24.0));
    }

    #[test]
    fn cap_fps_none_when_both_missing() {
        assert_eq!(VideoMetadata::default().cap_fps(), None);
    }

    #[test]
    fn cap_fps_differs_from_best_fps_for_vfr_material() {
        // 两个取值刻意分开：best_fps 给展示，cap_fps 给判定。
        let m = VideoMetadata {
            avg_fps: Some(24.0),
            nominal_fps: Some(60.0),
            ..Default::default()
        };
        assert_eq!(m.best_fps(), Some(24.0));
        assert_eq!(m.cap_fps(), Some(60.0));
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
        assert_eq!(c.eps, 0.01);
    }

    // ---- decide: 帧率动作表 ----

    #[test]
    fn decide_disabled_when_cap_not_configured() {
        let c = MaxFpsConfig {
            max_fps: 0.0,
            eps: 0.01,
        };
        assert_eq!(c.decide(Some(60.0)), FpsDecision::Disabled);
        assert_eq!(c.decide(None), FpsDecision::Disabled);
    }

    #[test]
    fn decide_unknown_when_source_fps_missing() {
        let c = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.01,
        };
        assert_eq!(c.decide(None), FpsDecision::Unknown);
    }

    #[test]
    fn decide_passthrough_when_source_within_cap() {
        let c = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.01,
        };
        assert_eq!(c.decide(Some(24.0)), FpsDecision::Passthrough);
        assert_eq!(c.decide(Some(30.0)), FpsDecision::Passthrough);
        assert_eq!(c.decide(Some(29.97)), FpsDecision::Passthrough);
    }

    #[test]
    fn decide_reduce_when_source_above_cap() {
        let c = MaxFpsConfig {
            max_fps: 30.0,
            eps: 0.01,
        };
        assert_eq!(c.decide(Some(60.0)), FpsDecision::Reduce { target: 30.0 });
    }

    #[test]
    fn decide_reduce_uses_the_configured_cap_as_target() {
        let c = MaxFpsConfig {
            max_fps: 24.0,
            eps: 0.01,
        };
        assert_eq!(c.decide(Some(60.0)), FpsDecision::Reduce { target: 24.0 });
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
            nvenc: Vec::new(),
        };
        assert!(cache.has_encoder("libx264"));
        assert!(cache.has_encoder("libsvtav1"));
        assert!(!cache.has_encoder("libdav1d"));
        // Empty cache returns false for any non-empty needle.
        let empty = EncoderCache {
            encoders_output: String::new(),
            nvenc: Vec::new(),
        };
        assert!(!empty.has_encoder("libx264"));
        // Empty needle matches even empty cache (str::contains("") == true).
        // This is the property the parser filters against, not has_encoder
        // itself.
        assert!(empty.has_encoder(""));
    }

    // ---- EncodeParams + ExtraArg boundary (review #15 → Option B) ----
    //
    // The runtime `validate_extra_args` method was removed because
    // the type-level boundary (`Vec<ExtraArg>`) makes it impossible
    // to construct an unvalidated entry. These tests now exercise
    // the boundary via `ExtraArg::parse`: the standard-user-tweaks
    // inputs all parse successfully, the injection variants all
    // fail at parse time.

    fn params_with_extra(extra: Vec<crate::encoder::ffmpeg_arg::ExtraArg>) -> EncodeParams {
        EncodeParams {
            extra_args: extra,
            ..EncodeParams::from_app_config(None)
        }
    }

    fn parse_all(inputs: &[&str]) -> Vec<crate::encoder::ffmpeg_arg::ExtraArg> {
        inputs
            .iter()
            .map(|s| {
                crate::encoder::ffmpeg_arg::ExtraArg::parse(s)
                    .unwrap_or_else(|e| panic!("expected {s:?} to parse, got: {e:?}"))
            })
            .collect()
    }

    #[test]
    fn standard_user_tweaks_parse_into_extra_args() {
        let p = params_with_extra(parse_all(&[
            "-ss=5",
            "-metadata",
            "title=foo",
            "-vf",
            "scale=720:-1",
        ]));
        assert_eq!(p.extra_args.len(), 5);
    }

    #[test]
    fn path_injection_variants_fail_at_parse() {
        // Each of these MUST be rejected by `ExtraArg::parse`. If
        // any slips through, the type-level boundary has a hole.
        for evil in [
            "-i",                    // -i on deny list
            "-c:v",                  // -c:v on deny list
            "-c:a",                  // -c:a on deny list
            "-o",                    // -o on deny list
            "-y",                    // -y is hardcoded
            "--preset",              // long opt
            "-vf_negate.mp4",        // flag-with-path-char as one token
            "-vf scale=720:foo bar", // whitespace in value
            "-vf=scale=720:foo;rm",  // shell metachar
            "preset",                // missing leading dash, no `=`
            "5",                     // bare value with no `=`
            "",                      // empty
            "-",                     // bare dash
            "-; rm -rf /",           // shell metachar prefix
        ] {
            assert!(
                crate::encoder::ffmpeg_arg::ExtraArg::parse(evil).is_err(),
                "expected ExtraArg::parse to reject {evil:?}"
            );
        }
    }

    // ---- InputPath (review #109) ----
    //
    // The runtime tests cover the validated-factory reject cases that
    // are the type-level guarantee's defense. A compile-fail test
    // (trybuild) would also work but adds a build dependency for
    // one newtype; not worth it here.

    #[test]
    fn input_path_accepts_normal_filename() {
        let p = InputPath::new(std::path::Path::new("/in/movie.mp4")).unwrap();
        assert_eq!(p.as_path(), std::path::Path::new("/in/movie.mp4"));
        assert_eq!(p.to_string(), "/in/movie.mp4");
    }

    #[test]
    fn input_path_accepts_dotfile() {
        // Leading `.` (not `-`) is allowed — only dash-prefixed names
        // are rejected, matching the v1 collect_dir behavior.
        let p = InputPath::new(std::path::Path::new("/in/.bashrc")).unwrap();
        assert_eq!(p.to_string(), "/in/.bashrc");
    }

    #[test]
    fn input_path_accepts_relative_path() {
        let p = InputPath::new(std::path::Path::new("in/movie.mp4")).unwrap();
        assert_eq!(p.as_path(), std::path::Path::new("in/movie.mp4"));
    }

    #[test]
    fn input_path_rejects_dash_prefixed_filename() {
        // The classic review-#109 case: a filename whose first
        // character is `-` would be parsed by ffmpeg as an option
        // when passed positionally after `-i`.
        let err = InputPath::new(std::path::Path::new("-vf_negate.mp4")).unwrap_err();
        assert!(matches!(err, InputPathError::DashPrefixed(_)));
    }

    #[test]
    fn input_path_rejects_dash_prefixed_subdirectory() {
        // Cover the deeper case: a path with a dash-prefixed leaf
        // (`/tmp/-secret`) should also be rejected, since ffmpeg
        // receives the full path string.
        let err = InputPath::new(std::path::Path::new("/tmp/-secret")).unwrap_err();
        assert!(matches!(err, InputPathError::DashPrefixed(_)));
    }

    #[test]
    fn input_path_rejects_empty_path() {
        let err = InputPath::new(std::path::Path::new("")).unwrap_err();
        assert!(matches!(err, InputPathError::Empty));
    }
}
