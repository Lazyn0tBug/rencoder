//! CLI argument parsing via clap derive.
//!
//! Two subcommands: `encode` and `analyze`. `main.rs` converts the parsed
//! args into library types (`EncodeParams`, `AnalyzeParams`, `Config`).

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// Top-level CLI: `rencoder <subcommand> ...`
#[derive(Parser, Debug)]
#[command(
    name = "rencoder",
    version,
    about = "Smart video transcoder and analyzer"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Transcode video files.
    Encode(EncodeArgs),
    /// Analyze video frame-rate distribution.
    Analyze(AnalyzeArgs),
}

/// `encode` subcommand argument format strings, mapped 1:1 to `VideoFormat`.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatArg {
    X264,
    Hevc,
    Vp9,
    Av1,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnalyzeFormatArg {
    /// Per-file CSV to stdout (default). One row per input file with
    /// columns `path, fps, codec, duration_secs, file_bytes`. Pipe-friendly.
    Csv,
    /// Human-readable framed table (per-file rows). Built on Polars'
    /// Display; one row per input file.
    Table,
    /// Human-readable text summary (legacy behavior) -- the cross-tab
    /// (fps x codec) aggregated view.
    Text,
    /// Per-file JSON objects, one per line. For `jq` and other line-based
    /// JSON consumers.
    Json,
}

#[derive(Args, Debug)]
pub struct EncodeArgs {
    /// Target format (lowercase: x264|hevc|vp9|av1). Default: av1.
    #[arg(short = 'c', long = "format", value_enum, default_value_t = FormatArg::Av1)]
    pub format: FormatArg,

    /// Bitrate in Kbps (default mode) or CRF value when used with --crf.
    #[arg(short = 'b', long = "bitrate")]
    pub bitrate: Option<u32>,

    /// Single input file.
    #[arg(short = 'f', long = "file")]
    pub file: Option<PathBuf>,

    /// Input directory (recursive). Mutually exclusive with --file.
    #[arg(short = 'd', long = "dir")]
    pub dir: Option<PathBuf>,

    /// Extension filter (repeatable). Empty = all files.
    #[arg(short = 'e', long = "ext")]
    pub ext: Vec<String>,

    /// Launch the interactive wizard before transcoding.
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Enable NVIDIA hardware acceleration (requires nvidia-smi).
    #[arg(long = "nv")]
    pub enable_nv: bool,

    /// Max FPS cap (canonical: 24, 25, 30, 60; or 0 to disable).
    #[arg(long = "max-fps", default_value = "0")]
    pub max_fps: f64,

    /// Use CRF mode instead of bitrate.
    #[arg(long = "crf")]
    pub crf: bool,

    /// Path to JSON config file. Loaded once, values not present fall back to CLI args.
    #[arg(long = "config")]
    pub config: Option<PathBuf>,

    /// Output path. Extension picks the format (.parquet, .jsonl, .csv).
    /// Default: timestamped Parquet in `./data/encoding-<UTC-ISO8601>Z.parquet`.
    #[arg(short = 'o', long = "output")]
    pub output: Option<PathBuf>,

    /// Pass-through args to ffmpeg (everything after `--`).
    #[arg(last = true)]
    pub extra_args: Vec<String>,
}

#[derive(Args, Debug)]
pub struct AnalyzeArgs {
    /// Input directory (recursive; default: current directory).
    #[arg(short = 'd', long = "dir")]
    pub dir: Option<PathBuf>,

    /// Single input file.
    #[arg(short = 'f', long = "file")]
    pub file: Option<PathBuf>,

    /// Extension filter (repeatable). Empty = all files.
    #[arg(short = 'e', long = "ext")]
    pub ext: Vec<String>,

    /// Enable parallel analysis via rayon (lands in Phase 12).
    #[arg(short = 'p', long = "parallel")]
    pub parallel: bool,

    /// Output format. Default: csv (per-file records, pipe-friendly).
    #[arg(long = "format", value_enum, default_value_t = AnalyzeFormatArg::Csv)]
    pub format: AnalyzeFormatArg,

    /// Output file path. `-` for stdout (default). With `--format text`
    /// this still writes to stdout; the flag is mostly useful for `csv`
    /// and `json`.
    #[arg(short = 'o', long = "output", default_value = "-")]
    pub output: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse_ok(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("parse should succeed")
    }

    fn parse_err(args: &[&str]) {
        let _ = Cli::try_parse_from(args).expect_err("parse should fail");
    }

    // ---- encode: defaults ----

    #[test]
    fn encode_defaults_to_av1() {
        let cli = parse_ok(&["rencoder", "encode", "-f", "in.mp4"]);
        match cli.command {
            Command::Encode(args) => {
                assert_eq!(args.format, FormatArg::Av1);
                assert_eq!(args.bitrate, None);
                assert_eq!(args.file, Some(PathBuf::from("in.mp4")));
                assert_eq!(args.dir, None);
                assert!(args.ext.is_empty());
                assert!(!args.interactive);
                assert!(!args.enable_nv);
                assert_eq!(args.max_fps, 0.0);
                assert!(!args.crf);
                assert_eq!(args.config, None);
                assert!(args.extra_args.is_empty());
            }
            _ => panic!("expected Encode"),
        }
    }

    // ---- encode: format variants ----

    #[test]
    fn encode_all_format_variants_parse() {
        for (s, expected) in [
            ("x264", FormatArg::X264),
            ("hevc", FormatArg::Hevc),
            ("vp9", FormatArg::Vp9),
            ("av1", FormatArg::Av1),
        ] {
            let cli = parse_ok(&["rencoder", "encode", "-f", "in.mp4", "-c", s]);
            match cli.command {
                Command::Encode(args) => assert_eq!(args.format, expected, "for {s}"),
                _ => panic!("expected Encode for {s}"),
            }
        }
    }

    #[test]
    fn encode_invalid_format_rejected() {
        parse_err(&["rencoder", "encode", "-f", "in.mp4", "-c", "divx"]);
    }

    // ---- encode: bitrate / crf ----

    #[test]
    fn encode_bitrate_flag_parses_u32() {
        let cli = parse_ok(&["rencoder", "encode", "-f", "in.mp4", "-b", "5000"]);
        match cli.command {
            Command::Encode(args) => assert_eq!(args.bitrate, Some(5000)),
            _ => panic!("expected Encode"),
        }
    }

    #[test]
    fn encode_bitrate_negative_rejected() {
        // u32 parser rejects negative.
        parse_err(&["rencoder", "encode", "-f", "in.mp4", "-b", "-5"]);
    }

    #[test]
    fn encode_crf_flag_sets_bool() {
        let cli = parse_ok(&["rencoder", "encode", "-f", "in.mp4", "--crf"]);
        match cli.command {
            Command::Encode(args) => {
                assert!(args.crf);
                assert_eq!(args.bitrate, None);
            }
            _ => panic!("expected Encode"),
        }
    }

    // ---- encode: max-fps ----

    #[test]
    fn encode_max_fps_canonical_values() {
        for fps in ["24", "25", "30", "60"] {
            let cli = parse_ok(&[
                "rencoder", "encode", "-f", "in.mp4", "--max-fps", fps,
            ]);
            match cli.command {
                Command::Encode(args) => {
                    assert_eq!(args.max_fps, fps.parse::<f64>().unwrap());
                }
                _ => panic!("expected Encode"),
            }
        }
    }

    #[test]
    fn encode_max_fps_zero_means_disabled() {
        let cli = parse_ok(&["rencoder", "encode", "-f", "in.mp4", "--max-fps", "0"]);
        match cli.command {
            Command::Encode(args) => assert_eq!(args.max_fps, 0.0),
            _ => panic!(),
        }
    }

    // ---- encode: passthrough ----

    #[test]
    fn encode_extra_args_after_double_dash() {
        let cli = parse_ok(&[
            "rencoder", "encode", "-f", "in.mp4",
            "--", "-ss", "10", "-metadata", "title=foo",
        ]);
        match cli.command {
            Command::Encode(args) => {
                assert_eq!(args.extra_args, vec!["-ss", "10", "-metadata", "title=foo"]);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn encode_extra_args_empty_when_no_double_dash() {
        let cli = parse_ok(&["rencoder", "encode", "-f", "in.mp4"]);
        match cli.command {
            Command::Encode(args) => assert!(args.extra_args.is_empty()),
            _ => panic!(),
        }
    }

    // ---- encode: extensions / dir / nv ----

    #[test]
    fn encode_extensions_repeatable() {
        let cli = parse_ok(&[
            "rencoder", "encode", "-d", "/d",
            "-e", "mp4", "-e", "mkv", "-e", "avi",
        ]);
        match cli.command {
            Command::Encode(args) => {
                assert_eq!(args.dir, Some(PathBuf::from("/d")));
                assert_eq!(args.ext, vec!["mp4", "mkv", "avi"]);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn encode_nv_flag_sets_bool() {
        let cli = parse_ok(&["rencoder", "encode", "-f", "in.mp4", "--nv"]);
        match cli.command {
            Command::Encode(args) => assert!(args.enable_nv),
            _ => panic!(),
        }
    }

    #[test]
    fn encode_interactive_flag() {
        let cli = parse_ok(&["rencoder", "encode", "-f", "in.mp4", "-i"]);
        match cli.command {
            Command::Encode(args) => assert!(args.interactive),
            _ => panic!(),
        }
    }

    #[test]
    fn encode_config_path() {
        let cli = parse_ok(&[
            "rencoder", "encode", "-f", "in.mp4", "--config", "/etc/rencoder.json",
        ]);
        match cli.command {
            Command::Encode(args) => assert_eq!(args.config, Some(PathBuf::from("/etc/rencoder.json"))),
            _ => panic!(),
        }
    }

    // ---- analyze ----

    #[test]
    fn analyze_defaults() {
        let cli = parse_ok(&["rencoder", "analyze"]);
        match cli.command {
            Command::Analyze(args) => {
                assert_eq!(args.dir, None);
                assert_eq!(args.file, None);
                assert!(args.ext.is_empty());
                assert!(!args.parallel);
                assert_eq!(args.format, AnalyzeFormatArg::Csv);
                assert_eq!(args.output, PathBuf::from("-"));
            }
            _ => panic!("expected Analyze"),
        }
    }

    #[test]
    fn analyze_with_all_flags() {
        let cli = parse_ok(&[
            "rencoder", "analyze",
            "-d", "/d", "-e", "mp4", "-p", "--format", "text", "-o", "stats.txt",
        ]);
        match cli.command {
            Command::Analyze(args) => {
                assert_eq!(args.dir, Some(PathBuf::from("/d")));
                assert_eq!(args.ext, vec!["mp4"]);
                assert!(args.parallel);
                assert_eq!(args.format, AnalyzeFormatArg::Text);
                assert_eq!(args.output, PathBuf::from("stats.txt"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn analyze_extensions_repeatable() {
        let cli = parse_ok(&[
            "rencoder", "analyze", "-d", "/d", "-e", "mp4", "-e", "mkv",
        ]);
        match cli.command {
            Command::Analyze(args) => assert_eq!(args.ext, vec!["mp4", "mkv"]),
            _ => panic!(),
        }
    }

    #[test]
    fn analyze_single_file() {
        let cli = parse_ok(&["rencoder", "analyze", "-f", "/single.mp4"]);
        match cli.command {
            Command::Analyze(args) => assert_eq!(args.file, Some(PathBuf::from("/single.mp4"))),
            _ => panic!(),
        }
    }

    // ---- subcommand-level errors ----

    #[test]
    fn no_subcommand_errors() {
        parse_err(&["rencoder"]);
    }

    #[test]
    fn unknown_subcommand_errors() {
        parse_err(&["rencoder", "frobnicate"]);
    }

    #[test]
    fn encode_missing_required_input_errors() {
        // Neither --file nor --dir is provided; we don't enforce this at
        // clap level (interact mode can populate later), but clap doesn't
        // complain either -- the validation lives in main.rs (Phase 11).
        // This test just verifies clap parses the bare command.
        let cli = parse_ok(&["rencoder", "encode"]);
        assert!(matches!(cli.command, Command::Encode(_)));
    }
}
