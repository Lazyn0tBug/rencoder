//! CLI entry point.

mod args;
mod file_sink;
mod interact;
mod signal;
mod tracing_init;

use anyhow::{Context, Result};
use clap::Parser;
use smart_encode_lib::{
    analyzer::fps_stats::analyze_fps,
    collect::collect_files,
    types::Target,
};
use tracing::{info, instrument, warn};

fn main() -> Result<()> {
    tracing_init::init();
    let cli = args::Cli::parse();
    match cli.command {
        args::Command::Encode(encode_args) => encode_cmd(encode_args),
        args::Command::Analyze(analyze_args) => analyze_cmd(analyze_args),
    }
}

/// `smart-encode encode ...` -- Phase 11 fills in the full pipeline
/// (Config::load, init_encoder_cache, signal::install_handler,
/// collect_files, transcode_batch, per-file progress reporting).
#[instrument(skip_all, fields(format = ?_args.format))]
fn encode_cmd(_args: args::EncodeArgs) -> Result<()> {
    Ok(())
}

/// `smart-encode analyze ...` -- probes each file, groups by rounded fps,
/// prints summary, optionally writes a detailed file.
#[instrument(skip_all, fields(dir = ?args.dir, file = ?args.file, parallel = args.parallel))]
fn analyze_cmd(args: args::AnalyzeArgs) -> Result<()> {
    let target = target_from_analyze_args(&args)
        .with_context(|| "failed to determine target directory")?;

    let files = collect_files(&target, &args.ext)
        .with_context(|| format!("failed to collect files from {}", target_path(&target).display()))?;

    info!("analyzing {} file(s)", files.len());

    // Parallel analysis lands in Phase 12. For now, --parallel prints a
    // warning and falls back to serial so the CLI surface is stable.
    if args.parallel {
        warn!("--parallel not yet implemented; running serially");
    }
    let stats = analyze_fps(&files).with_context(|| "fps analysis failed")?;

    print!("{}", stats.summary());

    if args.verbose {
        std::fs::write(&args.output, stats.detailed())
            .with_context(|| format!("failed to write {}", args.output.display()))?;
        println!("Detailed stats written to {}", args.output.display());
    }

    Ok(())
}

/// Resolve the CLI args to a `Target`. `--file` wins over `--dir`; with
/// neither set, fall back to the current working directory.
fn target_from_analyze_args(args: &args::AnalyzeArgs) -> Result<Target> {
    if let Some(file) = &args.file {
        return Ok(Target::File(file.clone()));
    }
    let dir = match &args.dir {
        Some(d) => d.clone(),
        None => std::env::current_dir().context("could not determine current directory")?,
    };
    Ok(Target::Dir(dir))
}

/// Best-effort path display for a Target (used in error messages).
fn target_path(target: &Target) -> &std::path::Path {
    match target {
        Target::File(p) => p,
        Target::Dir(p) => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn args_with(file: Option<&str>, dir: Option<&str>) -> args::AnalyzeArgs {
        args::AnalyzeArgs {
            dir: dir.map(PathBuf::from),
            file: file.map(PathBuf::from),
            ext: vec![],
            parallel: false,
            verbose: false,
            output: PathBuf::from("fps_stats.txt"),
        }
    }

    #[test]
    fn target_from_file_flag_wins() {
        let args = args_with(Some("/single.mp4"), Some("/ignored"));
        let target = target_from_analyze_args(&args).unwrap();
        match target {
            Target::File(p) => assert_eq!(p, PathBuf::from("/single.mp4")),
            _ => panic!("expected Target::File"),
        }
    }

    #[test]
    fn target_from_dir_flag() {
        let args = args_with(None, Some("/some/dir"));
        let target = target_from_analyze_args(&args).unwrap();
        match target {
            Target::Dir(p) => assert_eq!(p, PathBuf::from("/some/dir")),
            _ => panic!("expected Target::Dir"),
        }
    }

    #[test]
    fn target_defaults_to_current_dir() {
        let args = args_with(None, None);
        let target = target_from_analyze_args(&args).unwrap();
        match target {
            Target::Dir(p) => {
                // Compare to std::env::current_dir() directly so the test
                // is robust to chdir during test runs.
                let cwd = std::env::current_dir().unwrap();
                assert_eq!(p, cwd);
            }
            _ => panic!("expected Target::Dir"),
        }
    }

    #[test]
    fn target_path_helper_returns_inner_path() {
        let file_target = Target::File(PathBuf::from("/x.mp4"));
        assert_eq!(target_path(&file_target), PathBuf::from("/x.mp4"));
        let dir_target = Target::Dir(PathBuf::from("/y"));
        assert_eq!(target_path(&dir_target), PathBuf::from("/y"));
    }
}
