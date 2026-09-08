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
    pipeline::{batch::transcode_batch, cache::init_encoder_cache},
    types::{
        EncodeParams, EncodingOutcome, ProcessOutput, SkippedReason, Target, VideoFormat,
    },
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
/// cache, install SIGINT handler, collect files, run the batch,
/// write per-file records to the default `./data/encoding-*.parquet`.
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

    let records = transcode_batch(
        &params,
        &files,
        &cache,
        &config,
        ProcessOutput::Inherit,
        None,
    );

    // Default output: timestamped Parquet in ./data/. Extension of -o
    // picks the format: .parquet | .jsonl | .csv.
    let output_path = match &_args.output {
        Some(p) => p.clone(),
        None => default_data_path(),
    };
    write_records(&records, &output_path)?;
    print_encode_summary(&records);

    Ok(())
}

/// `data/encoding-<UTC-ISO8601>Z.parquet` -- one file per run, named by
/// start time. ISO 8601 UTC (no localtime surprises); `:` replaced
/// with `-` for filesystem safety on macOS / Windows.
fn default_data_path() -> std::path::PathBuf {
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
    std::path::PathBuf::from("data").join(format!("encoding-{ts}.parquet"))
}

/// Write the records to the given path. Extension determines format.
fn write_records(
    records: &[rencoder_lib::types::EncodingRecord],
    path: &std::path::Path,
) -> Result<()> {
    use rencoder_lib::encoding;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
    }
    let bytes = match path.extension().and_then(|e| e.to_str()) {
        Some("parquet") => encoding::records_to_parquet(records)
            .with_context(|| "parquet encode failed")?,
        Some("jsonl") | Some("ndjson") => encoding::records_to_jsonl(records).into_bytes(),
        Some("csv") => encoding::records_to_csv(records).into_bytes(),
        other => anyhow::bail!(
            "unsupported output extension {:?}; use .parquet, .jsonl, or .csv",
            other
        ),
    };
    std::fs::write(path, bytes)
        .with_context(|| format!("failed to write {}", path.display()))?;
    eprintln!("Wrote {} record(s) to {}", records.len(), path.display());
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

    // Format-specific body, then dispatch to stdout-or-file based on -o.
    let render = |body: String| -> Result<()> {
        if args.output == Path::new("-") {
            print!("{body}");
            Ok(())
        } else {
            std::fs::write(&args.output, body)
                .with_context(|| format!("failed to write {}", args.output.display()))
        }
    };

    match args.format {
        AnalyzeFormatArg::Csv => {
            render(rencoder_lib::analyzer::fps_stats::records_to_csv(&records))?;
        }
        AnalyzeFormatArg::Table => {
            // Build a DataFrame from the records and use Polars' Display
            // for the framed box-drawing table. Per-file rows in input
            // order (rayon's par_iter().collect preserves order).
            let df = rencoder_lib::analyzer::fps_stats::records_to_dataframe(&records)
                .map_err(|e| anyhow::anyhow!("dataframe build: {e}"))?;
            render(format!("{df}"))?;
        }
        AnalyzeFormatArg::Text => {
            render(stats.summary())?;
        }
        AnalyzeFormatArg::Json => {
            render(records_to_jsonl(&records))?;
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

fn print_encode_summary(results: &[rencoder_lib::types::EncodingRecord]) {
    let total = results.len();
    let success = results
        .iter()
        .filter(|r| matches!(r.outcome, EncodingOutcome::Succeeded))
        .count();
    let reduced = results
        .iter()
        .filter(|r| matches!(r.outcome, EncodingOutcome::ReducedFps))
        .count();
    let skipped = results
        .iter()
        .filter(|r| {
            matches!(
                r.outcome,
                EncodingOutcome::Skipped(
                    SkippedReason::OutputExists | SkippedReason::SameCodec
                )
            )
        })
        .count();
    let failed = results
        .iter()
        .filter(|r| matches!(r.outcome, EncodingOutcome::Failed))
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
