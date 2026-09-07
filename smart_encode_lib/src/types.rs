//! Public data structures shared across the library.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::error::EncodeError;

// ===== Target format =====

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoFormat {
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
    pub eps: f64,
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
    /// TODO: implement the predicate. See `CLAUDE.md` (TODO section) for
    /// design notes -- epsilon tolerance semantics, strict vs. non-strict
    /// boundary at `src_fps == max_fps`.
    #[allow(unused_variables)] // remove once the body is implemented
    pub fn need_reduce(&self, src_fps: f64) -> bool {
        todo!("implement MaxFpsConfig::need_reduce -- see CLAUDE.md")
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

#[derive(Clone, Debug)]
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

    pub fn summary(&self) -> String {
        // Phase 9: implement per `video_fps_stats.zsh` output format.
        String::new()
    }

    pub fn detailed(&self) -> String {
        // Phase 9: implement per-file listing for `-v`.
        String::new()
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
