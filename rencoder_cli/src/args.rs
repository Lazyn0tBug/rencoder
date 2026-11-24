//! CLI argument parsing via clap derive.
//!
//! Two subcommands: `encode` and `analyze`. U17 split: only `target`
//! (`-f`/`-d`) and the action (`encode`/`analyze`) plus a few per-run
//! overrides (`-i`, `--config`, `-o`, `--nv`/`--no-nv`, passthrough args)
//! remain on the CLI. Every other knob (format, bitrate, max-fps, crf,
//! extensions, analyze format, parallel, ...) lives in `rencoder.toml`.
//! See `rencoder_lib::config::project` for the schema.
//!
//! `nv` is the deliberate exception to "everything else lives in the
//! config": it is the most machine-dependent knob there is (the same
//! config file has to work on a workstation with an RTX and on a laptop
//! without one), so the CLI can override it per run. CLI wins over the
//! config file.
//!
//! `main.rs` loads the config, merges CLI overrides, and converts the
//! parsed args into library types (`EncodeParams`, `AnalyzeParams`,
//! `Config`).

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// Top-level CLI: `rencoder <subcommand> ...`
#[derive(Parser, Debug)]
#[command(
    name = "rencoder",
    version,
    about = "Smart video transcoder and analyzer"
)]
pub struct Cli {
    /// Check this machine's NVENC capability for this run -- the stored
    /// record is ignored (the machine may have changed) -- and update
    /// it. Global: it works before or after the subcommand
    /// (`rencoder --detect encode ...` == `rencoder encode --detect ...`).
    #[arg(long, global = true)]
    pub detect: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Transcode video files.
    Encode(EncodeArgs),
    /// Analyze video frame-rate distribution.
    Analyze(AnalyzeArgs),
    /// Check this machine's NVENC capability, store the result and
    /// print a report.
    Detect(DetectArgs),
}

/// `rencoder detect` -- no options of its own; the global `--detect`
/// flag is what asks for a check elsewhere.
#[derive(Args, Debug)]
pub struct DetectArgs {}

#[derive(Args, Debug)]
pub struct EncodeArgs {
    /// Single input file.
    #[arg(short = 'f', long = "file")]
    pub file: Option<PathBuf>,

    /// Input directory (recursive). Mutually exclusive with --file.
    #[arg(short = 'd', long = "dir")]
    pub dir: Option<PathBuf>,

    /// Launch the interactive wizard before transcoding.
    ///
    /// **Not implemented yet** (design doc §8 row 111): the wizard was
    /// a placeholder file, and the flag used to be accepted and
    /// ignored. The CLI now says so instead of pretending.
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Path to TOML config file. When not given, resolves via
    /// `$RENENCODER_CONFIG` (if set) or `<app_dir>/rencoder.toml` (binary's
    /// parent dir). Missing file at the resolved path triggers auto-create
    /// of a starter config with mode bits 0o644 (Unix).
    #[arg(long = "config")]
    pub config: Option<PathBuf>,

    /// Output path. Extension picks the format (.parquet, .jsonl, .csv).
    /// Default: timestamped Parquet in `<app_dir>/data/encoding-<UTC-ISO8601>Z.parquet`
    /// (U18; honors `$RENENCODER_DATA_DIR`).
    #[arg(short = 'o', long = "output")]
    pub output: Option<PathBuf>,

    /// Force NVIDIA hardware encoding on. Overrides `[encode].nv` in
    /// `rencoder.toml`. When enabled, the NVENC encoders this machine
    /// can actually run are detected by a capability probe; unavailable
    /// ones fall back to software encoding (with a warning).
    #[arg(long = "nv", conflicts_with = "no_nv")]
    pub nv: bool,

    /// Force NVIDIA hardware encoding off. Overrides `[encode].nv` in
    /// `rencoder.toml`.
    #[arg(long = "no-nv")]
    pub no_nv: bool,

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

    /// Path to TOML config file. When not given, resolves via
    /// `$RENENCODER_CONFIG` (if set) or `<app_dir>/rencoder.toml` (binary's
    /// parent dir). Missing file at the resolved path triggers auto-create
    /// of a starter config with mode bits 0o644 (Unix).
    #[arg(long = "config")]
    pub config: Option<PathBuf>,

    /// Output file path. No default — without `-o`, the value comes from
    /// `[analyze].output` in `rencoder.toml` (falling back to stdout `-`).
    /// Explicit `-o -` means "stdout, ignore any `[analyze].output` setting".
    #[arg(short = 'o', long = "output")]
    pub output: Option<PathBuf>,
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
    fn encode_defaults_to_minimal() {
        let cli = parse_ok(&["rencoder", "encode", "-f", "in.mp4"]);
        match cli.command {
            Command::Encode(args) => {
                assert_eq!(args.file, Some(PathBuf::from("in.mp4")));
                assert_eq!(args.dir, None);
                assert!(!args.interactive);
                assert_eq!(args.config, None);
                assert_eq!(args.output, None);
                assert!(args.extra_args.is_empty());
            }
            _ => panic!("expected Encode"),
        }
    }

    #[test]
    fn encode_dir_target() {
        let cli = parse_ok(&["rencoder", "encode", "-d", "/d"]);
        match cli.command {
            Command::Encode(args) => {
                assert_eq!(args.dir, Some(PathBuf::from("/d")));
                assert_eq!(args.file, None);
            }
            _ => panic!(),
        }
    }

    // ---- encode: per-run overrides kept on CLI ----

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
            "rencoder",
            "encode",
            "-f",
            "in.mp4",
            "--config",
            "/etc/rencoder.toml",
        ]);
        match cli.command {
            Command::Encode(args) => {
                assert_eq!(args.config, Some(PathBuf::from("/etc/rencoder.toml")))
            }
            _ => panic!(),
        }
    }

    #[test]
    fn encode_output_path() {
        let cli = parse_ok(&[
            "rencoder",
            "encode",
            "-f",
            "in.mp4",
            "-o",
            "/tmp/out.parquet",
        ]);
        match cli.command {
            Command::Encode(args) => {
                assert_eq!(args.output, Some(PathBuf::from("/tmp/out.parquet")))
            }
            _ => panic!(),
        }
    }

    // ---- encode: passthrough ----

    #[test]
    fn encode_extra_args_after_double_dash() {
        let cli = parse_ok(&[
            "rencoder",
            "encode",
            "-f",
            "in.mp4",
            "--",
            "-ss",
            "10",
            "-metadata",
            "title=foo",
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

    // ---- encode: moved flags are gone ----

    #[test]
    fn encode_rejects_removed_format_flag() {
        parse_err(&["rencoder", "encode", "-f", "in.mp4", "-c", "av1"]);
    }

    #[test]
    fn encode_rejects_removed_bitrate_flag() {
        parse_err(&["rencoder", "encode", "-f", "in.mp4", "-b", "5000"]);
    }

    #[test]
    fn encode_rejects_removed_max_fps_flag() {
        parse_err(&["rencoder", "encode", "-f", "in.mp4", "--max-fps", "30"]);
    }

    #[test]
    fn encode_rejects_removed_crf_flag() {
        parse_err(&["rencoder", "encode", "-f", "in.mp4", "--crf"]);
    }

    #[test]
    fn encode_rejects_removed_nv_flag() {
        // `--nv` is back: it is the most machine-dependent knob there
        // is, so the CLI has to be able to override the config file.
        let cli = parse_ok(&["rencoder", "encode", "-f", "in.mp4", "--nv"]);
        match cli.command {
            Command::Encode(args) => {
                assert!(args.nv);
                assert!(!args.no_nv);
            }
            _ => panic!("expected Encode"),
        }
    }

    #[test]
    fn encode_accepts_no_nv_flag() {
        let cli = parse_ok(&["rencoder", "encode", "-f", "in.mp4", "--no-nv"]);
        match cli.command {
            Command::Encode(args) => {
                assert!(!args.nv);
                assert!(args.no_nv);
            }
            _ => panic!("expected Encode"),
        }
    }

    #[test]
    fn encode_rejects_nv_and_no_nv_together() {
        parse_err(&["rencoder", "encode", "-f", "in.mp4", "--nv", "--no-nv"]);
    }

    #[test]
    fn encode_rejects_removed_ext_flag() {
        parse_err(&["rencoder", "encode", "-f", "in.mp4", "-e", "mp4"]);
    }

    // ---- analyze ----

    // ---- detect ----

    #[test]
    fn detect_subcommand_parses() {
        let cli = parse_ok(&["rencoder", "detect"]);
        assert!(matches!(cli.command, Command::Detect(_)));
    }

    #[test]
    fn detect_flag_is_global() {
        // Before the subcommand ...
        let cli = parse_ok(&["rencoder", "--detect", "encode", "-f", "in.mp4"]);
        assert!(cli.detect);
        // ... and after it.
        let cli = parse_ok(&["rencoder", "encode", "--detect", "-f", "in.mp4"]);
        assert!(cli.detect);
    }

    #[test]
    fn detect_flag_defaults_to_false() {
        let cli = parse_ok(&["rencoder", "encode", "-f", "in.mp4"]);
        assert!(!cli.detect);
    }

    #[test]
    fn analyze_defaults() {
        let cli = parse_ok(&["rencoder", "analyze"]);
        match cli.command {
            Command::Analyze(args) => {
                assert_eq!(args.dir, None);
                assert_eq!(args.file, None);
                assert_eq!(args.config, None);
                assert_eq!(args.output, None);
            }
            _ => panic!("expected Analyze"),
        }
    }

    #[test]
    fn analyze_with_all_kept_flags() {
        let cli = parse_ok(&[
            "rencoder",
            "analyze",
            "-d",
            "/d",
            "--config",
            "rencoder.toml",
            "-o",
            "stats.txt",
        ]);
        match cli.command {
            Command::Analyze(args) => {
                assert_eq!(args.dir, Some(PathBuf::from("/d")));
                assert_eq!(args.config, Some(PathBuf::from("rencoder.toml")));
                assert_eq!(args.output, Some(PathBuf::from("stats.txt")));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn analyze_single_file() {
        let cli = parse_ok(&["rencoder", "analyze", "-f", "/single.mp4"]);
        match cli.command {
            Command::Analyze(args) => {
                assert_eq!(args.file, Some(PathBuf::from("/single.mp4")))
            }
            _ => panic!(),
        }
    }

    // ---- analyze: moved flags are gone ----

    #[test]
    fn analyze_rejects_removed_parallel_flag() {
        parse_err(&["rencoder", "analyze", "-p"]);
    }

    #[test]
    fn analyze_rejects_removed_format_flag() {
        parse_err(&["rencoder", "analyze", "--format", "csv"]);
    }

    #[test]
    fn analyze_rejects_removed_ext_flag() {
        parse_err(&["rencoder", "analyze", "-e", "mp4"]);
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
    fn encode_bare_command_parses() {
        // Neither --file nor --dir; clap doesn't enforce this (interactive
        // mode can populate later), validation lives in main.rs.
        let cli = parse_ok(&["rencoder", "encode"]);
        assert!(matches!(cli.command, Command::Encode(_)));
    }
}
