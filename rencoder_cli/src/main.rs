//! CLI entry point.

mod args;
mod file_sink;
mod interact;
mod signal;
mod tracing_init;

use anyhow::{Context, Result};
use clap::Parser;
use rencoder_lib::{
    collect::collect_files,
    config::Config,
    ffmpeg::encoder_cache::init_encoder_cache,
    processor::batch::transcode_batch,
    types::{EncodeParams, ProcessOutput, Target, TranscodeResult, VideoFormat},
};
use std::path::Path;
use tracing::{info, instrument, warn};

use crate::args::AnalyzeFormatArg;

/// Map CLI-side FormatArg to library VideoFormat.
impl From<args::FormatArg> for VideoFormat {
    fn from(f: args::FormatArg) -> Self {
        match f {
            args::FormatArg::X264 => VideoFormat::X264,
            args::FormatArg::Hevc => VideoFormat::Hevc,
            args::FormatArg::Vp9 => VideoFormat::Vp9,
            args::FormatArg::Av1 => VideoFormat::Av1,
        }
    }
}

fn main() -> Result<()> {
    tracing_init::init();
    let cli = args::Cli::parse();
    match cli.command {
        args::Command::Encode(encode_args) => encode_cmd(encode_args),
        args::Command::Analyze(analyze_args) => analyze_cmd(analyze_args),
    }
}

/// `rencoder encode ...` -- full pipeline: load config, init encoder
/// cache, install SIGINT handler, collect files, run the batch, print
/// per-file + summary lines.
#[instrument(skip_all, fields(format = ?_args.format, files = tracing::field::Empty))]
fn encode_cmd(_args: args::EncodeArgs) -> Result<()> {
    let config = match &_args.config {
        Some(path) => Config::load(path)
            .with_context(|| format!("failed to load config from {}", path.display()))?,
        None => Config::default(),
    };

    let target = if let Some(file) = &_args.file {
        Target::File(file.clone())
    } else if let Some(dir) = &_args.dir {
        Target::Dir(dir.clone())
    } else {
        anyhow::bail!("either --file or --dir must be provided (or use --interactive)");
    };

    let files = collect_files(&target, &_args.ext)
        .with_context(|| format!("failed to collect files from {}", target_path(&target).display()))?;

    if files.is_empty() {
        warn!("no files matched; nothing to do");
        return Ok(());
    }

    let format: VideoFormat = _args.format.into();
    let bitrate_or_crf = _args.bitrate.unwrap_or(if _args.crf { 28 } else { 5000 });

    let params = EncodeParams {
        format,
        bitrate_or_crf,
        is_crf: _args.crf,
        target: target.clone(),
        extensions: _args.ext.clone(),
        extra_args: _args.extra_args.clone(),
        max_fps: config.max_fps_config(),
    };

    info!("collected {} file(s), target format: {:?}", files.len(), format);

    let cache = init_encoder_cache(_args.enable_nv)
        .with_context(|| "failed to initialize encoder cache")?;

    // Install signal handler; the per-batch loop registers/unregisters
    // the current output path so a SIGINT cleans up the partial file.
    let _current_output = signal::install_handler();

    let results = transcode_batch(
        &params,
        &files,
        &cache,
        &config,
        ProcessOutput::Inherit,
        None,
    );

    print_encode_summary(&results);

    Ok(())
}

/// `rencoder analyze ...` -- probes each file, groups by rounded fps,
/// prints summary, optionally writes a detailed file.
#[instrument(skip_all, fields(dir = ?args.dir, file = ?args.file, parallel = args.parallel))]
fn analyze_cmd(args: args::AnalyzeArgs) -> Result<()> {
    let target = target_from_analyze_args(&args)
        .with_context(|| "failed to determine target directory")?;

    let files = collect_files(&target, &args.ext)
        .with_context(|| format!("failed to collect files from {}", target_path(&target).display()))?;

    info!("analyzing {} file(s)", files.len());

    let records = if args.parallel {
        rencoder_lib::analyzer::fps_stats::analyze_records_parallel(&files)
            .with_context(|| "fps analysis failed")?
    } else {
        rencoder_lib::analyzer::fps_stats::analyze_records(&files)
            .with_context(|| "fps analysis failed")?
    };
    let stats = rencoder_lib::analyzer::fps_stats::aggregate_records(&records);

    // One-line summary to stderr regardless of output format.
    eprintln!(
        "Analyzed {} file(s) ({} parsed, {} failed)",
        stats.total_files, stats.parsed_files, stats.failed_files,
    );

    // Format-specific output. csv/json respect -o; text always goes to
    // stdout (the legacy behavior, where -o is ignored).
    let to_stdout = |s: &str| print!("{s}");
    let to_file = |path: &std::path::Path, s: &str| -> Result<()> {
        std::fs::write(path, s).with_context(|| format!("failed to write {}", path.display()))
    };

    match args.format {
        AnalyzeFormatArg::Csv => {
            let body = rencoder_lib::analyzer::fps_stats::records_to_csv(&records);
            if args.output == Path::new("-") {
                to_stdout(&body);
            } else {
                to_file(&args.output, &body)?;
            }
        }
        AnalyzeFormatArg::Text => {
            let body = stats.summary();
            if args.output == Path::new("-") {
                to_stdout(&body);
            } else {
                to_file(&args.output, &body)?;
            }
        }
        AnalyzeFormatArg::Json => {
            let body = records_to_jsonl(&records);
            if args.output == Path::new("-") {
                to_stdout(&body);
            } else {
                to_file(&args.output, &body)?;
            }
        }
    }

    Ok(())
}

/// Render records as JSON Lines: one `AnalysisRecord` per line.
fn records_to_jsonl(records: &[rencoder_lib::analyzer::fps_stats::AnalysisRecord]) -> String {
    let mut out = String::new();
    for r in records {
        let fps = r
            .fps
            .map(|f| f.to_string())
            .unwrap_or_else(|| "null".into());
        let codec = r.codec.clone().unwrap_or_else(|| "null".into());
        let dur = r
            .duration_secs
            .map(|d| d.to_string())
            .unwrap_or_else(|| "null".into());
        out.push_str(&format!(
            "{{\"path\":\"{}\",\"fps\":{},\"codec\":\"{}\",\"duration_secs\":{},\"file_bytes\":{}}}\n",
            r.path.display(),
            fps,
            codec,
            dur,
            r.file_bytes,
        ));
    }
    out
}

fn print_encode_summary(results: &[(std::path::PathBuf, TranscodeResult)]) {
    let total = results.len();
    let success = results
        .iter()
        .filter(|(_, r)| matches!(r, TranscodeResult::Success))
        .count();
    let reduced = results
        .iter()
        .filter(|(_, r)| matches!(r, TranscodeResult::ReducedFps))
        .count();
    let skipped = results
        .iter()
        .filter(|(_, r)| {
            matches!(
                r,
                TranscodeResult::SkippedExists | TranscodeResult::SkippedSameCodec
            )
        })
        .count();
    let failed = results
        .iter()
        .filter(|(_, r)| matches!(r, TranscodeResult::Failed(_)))
        .count();

    println!();
    println!("Summary:");
    println!("  Total:   {total}");
    println!("  Success: {success}");
    println!("  Reduced: {reduced}");
    println!("  Skipped: {skipped}");
    println!("  Failed:  {failed}");
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
            format: args::AnalyzeFormatArg::Csv,
            output: PathBuf::from("-"),
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
