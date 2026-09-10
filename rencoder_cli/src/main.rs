//! CLI entry point.
//!
//! U18: loads `AppConfig` from `--config` (explicit path) or, when not
//! given, resolves via `rencoder_lib::config::path::resolve_config()`
//! (`$RENENCODER_CONFIG` > `<app_dir>/rencoder.toml`). Merges the
//! `encode` / `analyze` sections into the CLI args. CLI keeps only
//! `target` (`-f`/`-d`), `action` (subcommand), and a few per-run
//! overrides (`-i`, `-o`, passthrough args). All other knobs (format,
//! bitrate, max-fps, crf, nv, extensions, analyze format, parallel,
//! encoder overrides) come from the TOML config file.

mod args;
mod file_sink;
mod interact;
mod signal;
mod tracing_init;

use anyhow::{Context, Result};
use clap::Parser;
use rencoder_lib::{
    collect::collect_files,
    config::{AnalyzeFormatArg, AppConfig, Config},
    pipeline::{batch::transcode_batch, cache::init_encoder_cache},
    types::{EncodeParams, EncodingOutcome, ProcessOutput, SkippedReason, Target, VideoFormat},
};
use std::path::{Path, PathBuf};
use tracing::{info, instrument, warn};

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
/// write per-file records to `<app_dir>/data/encoding-*.parquet`
/// (U18; honors `$RENENCODER_DATA_DIR`).
#[instrument(skip_all, fields(files = tracing::field::Empty))]
fn encode_cmd(args: args::EncodeArgs) -> Result<()> {
    let app_config = load_app_config(args.config.as_deref())?;

    let enc = app_config.encode.as_ref();

    let target = if let Some(file) = &args.file {
        Target::File(file.clone())
    } else if let Some(dir) = &args.dir {
        Target::Dir(dir.clone())
    } else {
        anyhow::bail!("either --file or --dir must be provided (or use --interactive)");
    };

    let extensions = enc.and_then(|e| e.extensions.clone()).unwrap_or_default();

    let files = collect_files(&target, &extensions).with_context(|| {
        format!(
            "failed to collect files from {}",
            target_path(&target).display()
        )
    })?;

    if files.is_empty() {
        warn!("no files matched; nothing to do");
        return Ok(());
    }

    let format: VideoFormat = enc
        .and_then(|e| e.format)
        .map(Into::into)
        .unwrap_or(VideoFormat::Av1);

    // CRF field semantics (U18 step 6): `u32` aligned with ffmpeg's `-crf N`.
    // Sentinel `0` switches to bitrate mode; any other value is the CRF
    // quality. Default is 45 (libsvtav1 medium) when no [encode] section
    // is present at all.
    let crf_value = enc.map(|e| e.crf).unwrap_or(45);
    let (is_crf, bitrate_or_crf) = if crf_value == 0 {
        let bitrate = enc.and_then(|e| e.bitrate).unwrap_or(5000);
        (false, bitrate)
    } else {
        (true, crf_value)
    };

    let enable_nv = enc.and_then(|e| e.nv).unwrap_or(false);

    let config = Config::from_encode_config(app_config.encode.as_ref());

    let params = EncodeParams {
        format,
        bitrate_or_crf,
        is_crf,
        target: target.clone(),
        extensions: extensions.clone(),
        extra_args: args.extra_args.clone(),
        max_fps: config.max_fps_config(),
    };

    info!(
        "collected {} file(s), target format: {:?}",
        files.len(),
        format
    );

    let cache =
        init_encoder_cache(enable_nv).with_context(|| "failed to initialize encoder cache")?;

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

    // Default output: timestamped Parquet in <app_dir>/data/ (U18; honors
    // $RENENCODER_DATA_DIR). Extension of -o picks the format: .parquet
    // | .jsonl | .csv.
    let output_path = match &args.output {
        Some(p) => p.clone(),
        None => default_data_path(),
    };
    write_records(&records, &output_path)?;
    print_encode_summary(&records);

    Ok(())
}

/// `<app_dir>/data/encoding-<UTC-ISO8601>Z.parquet` -- one file per run,
/// named by start time. Uses the canonical UTC timestamp formatter from
/// `rencoder_lib::utils::time` so the format stays in sync with the
/// library's docs. Honors `$RENENCODER_DATA_DIR` (U18 step 11).
fn default_data_path() -> PathBuf {
    let dir =
        rencoder_lib::config::path::resolve_data_dir().unwrap_or_else(|| PathBuf::from("data"));
    std::fs::create_dir_all(&dir).expect("create data dir");
    rencoder_lib::config::fs_perms::set_app_dir_mode(&dir).expect("set data dir mode");
    dir.join(format!(
        "encoding-{}.parquet",
        rencoder_lib::utils::time::utc_timestamp_filename()
    ))
}

/// Write the records to the given path. Extension determines format.
fn write_records(
    records: &[rencoder_lib::types::EncodingRecord],
    path: &std::path::Path,
) -> Result<()> {
    use rencoder_lib::data::store;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
    }
    let bytes = match path.extension().and_then(|e| e.to_str()) {
        Some("parquet") => {
            store::records_to_parquet(records).with_context(|| "parquet encode failed")?
        }
        Some("jsonl") | Some("ndjson") => store::records_to_jsonl(records).into_bytes(),
        Some("csv") => store::records_to_csv(records).into_bytes(),
        other => anyhow::bail!(
            "unsupported output extension {:?}; use .parquet, .jsonl, or .csv",
            other
        ),
    };
    std::fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
    eprintln!("Wrote {} record(s) to {}", records.len(), path.display());
    Ok(())
}

/// `rencoder analyze ...` -- probes each file, groups by rounded fps,
/// prints summary, optionally writes a detailed file.
#[instrument(skip_all, fields(dir = ?args.dir, file = ?args.file, parallel = tracing::field::Empty))]
fn analyze_cmd(args: args::AnalyzeArgs) -> Result<()> {
    let app_config = load_app_config(args.config.as_deref())?;
    let an = app_config.analyze.as_ref();

    let target =
        target_from_analyze_args(&args).with_context(|| "failed to determine target directory")?;

    let extensions = an.and_then(|a| a.extensions.clone()).unwrap_or_default();

    let files = collect_files(&target, &extensions).with_context(|| {
        format!(
            "failed to collect files from {}",
            target_path(&target).display()
        )
    })?;

    info!("analyzing {} file(s)", files.len());

    let parallel = an.and_then(|a| a.parallel).unwrap_or(false);
    tracing::Span::current().record("parallel", parallel);

    let records = if parallel {
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

    // Output precedence: explicit `-o` (any value, including `-`) wins;
    // else `[analyze].output` from config; else `-` (stdout). This works
    // because CLI parses `output: Option<PathBuf>` with no default — no `-o`
    // means `None`, distinct from explicit `-o -`.
    let output_path: PathBuf = args
        .output
        .clone()
        .or_else(|| an.and_then(|a| a.output.clone()))
        .unwrap_or_else(|| PathBuf::from("-"));

    // Format-specific body, then dispatch to stdout-or-file based on the
    // resolved output path. `output_path == "-"` means stdout.
    let render = |body: String| -> Result<()> {
        if output_path == Path::new("-") {
            print!("{body}");
            Ok(())
        } else {
            std::fs::write(&output_path, body)
                .with_context(|| format!("failed to write {}", output_path.display()))
        }
    };

    let format = an.and_then(|a| a.format).unwrap_or(AnalyzeFormatArg::Csv);

    match format {
        AnalyzeFormatArg::Csv => {
            render(rencoder_lib::analyzer::fps_stats::records_to_csv(&records))?;
        }
        AnalyzeFormatArg::Table => {
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
                EncodingOutcome::Skipped(SkippedReason::OutputExists | SkippedReason::SameCodec)
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
fn target_path(target: &Target) -> &Path {
    match target {
        Target::File(p) => p,
        Target::Dir(p) => p,
    }
}

/// Load `AppConfig` from `--config` if given, otherwise resolve via
/// `path::resolve_config()` (U18 step 5):
///   1. `$RENENCODER_CONFIG` if set
///   2. `<app_dir>/rencoder.toml` (binary's parent dir)
///
/// Missing file at the resolved path is NOT an error — `AppConfig::load`
/// auto-creates a default config with mode bits 0o644 (Unix).
/// Schema validation rejects malformed TOML.
fn load_app_config(path: Option<&Path>) -> Result<AppConfig> {
    let resolved = if let Some(p) = path {
        p.to_path_buf()
    } else {
        rencoder_lib::config::path::resolve_config().ok_or_else(|| {
            anyhow::anyhow!(
                "could not determine config location: no --config, $RENENCODER_CONFIG \
                 is unset, and binary directory is unavailable. Pass --config explicitly."
            )
        })?
    };
    AppConfig::load(&resolved)
        .with_context(|| format!("failed to load config from {}", resolved.display()))
}

// ---------------------------------------------------------------------------
// AppConfig -> Config conversion lives in `rencoder_lib::config`
// (`Config::from_encode_config`) so the inherent impl stays in the same crate
// as `Config`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rencoder_lib::config::{AnalyzeConfig, EncodeConfig, EncodersConfig, FormatArg};
    use std::path::PathBuf;

    #[test]
    fn config_from_encode_config_uses_defaults_when_no_encode_section() {
        let cfg = Config::from_encode_config(None);
        assert_eq!(cfg.max_fps, 0.0);
        assert_eq!(cfg.max_fps_eps, 0.05);
        assert!(cfg.encoders.av1.is_none());
    }

    #[test]
    fn config_from_encode_config_picks_up_max_fps_and_encoders() {
        let enc = EncodeConfig {
            max_fps: Some(30),
            encoders: Some(EncodersConfig {
                av1: Some("libaom-av1".into()),
                vp9: Some("libvpx-vp9".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cfg = Config::from_encode_config(Some(&enc));
        assert_eq!(cfg.max_fps, 30.0);
        assert_eq!(cfg.encoders.av1.as_deref(), Some("libaom-av1"));
        assert_eq!(cfg.encoders.vp9.as_deref(), Some("libvpx-vp9"));
        assert!(cfg.encoders.x264.is_none());
    }

    #[test]
    fn load_app_config_missing_file_auto_creates() {
        // U18 step 4: missing file now auto-creates a default config
        // (atomic tmp+rename; mode bits 0o644 on Unix). The previous
        // behavior of "silent Self::default() on missing" was removed.
        // Use a temp dir so the parent exists and auto-create succeeds.
        let dir = std::env::temp_dir().join(format!("rencoder_cli_missing_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rencoder.toml");
        assert!(!path.exists(), "precondition: file must not exist");

        let cfg = load_app_config(Some(&path)).expect("auto-create load");
        assert!(path.exists(), "auto-create must write the file");

        // The auto-generated content has explicit values (bitrate=5000
        // etc.), so cfg is NOT == AppConfig::default(). Just verify
        // the result is parseable.
        assert!(cfg.encode.is_some() || cfg.analyze.is_some());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn load_app_config_explicit_path_loads_existing() {
        // Verify that an explicit --config pointing at an existing file
        // is read as-is (no auto-create side effects).
        let dir =
            std::env::temp_dir().join(format!("rencoder_cli_explicit_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rencoder.toml");
        std::fs::write(&path, "[encode]\nformat = \"hevc\"\nmax_fps = 30\n").unwrap();

        let cfg = load_app_config(Some(&path)).expect("load existing");
        let enc = cfg.encode.as_ref().expect("encode section");
        assert_eq!(enc.format, Some(FormatArg::Hevc));
        assert_eq!(enc.max_fps, Some(30));

        // File unchanged (auto-create would only fire on missing).
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("hevc"));
        assert!(!after.contains("av1"), "auto-create must not overwrite");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn target_from_file_flag_wins() {
        let args = args::AnalyzeArgs {
            dir: Some(PathBuf::from("/ignored")),
            file: Some(PathBuf::from("/single.mp4")),
            config: None,
            output: None,
        };
        let target = target_from_analyze_args(&args).unwrap();
        match target {
            Target::File(p) => assert_eq!(p, PathBuf::from("/single.mp4")),
            _ => panic!("expected Target::File"),
        }
    }

    #[test]
    fn target_from_dir_flag() {
        let args = args::AnalyzeArgs {
            dir: Some(PathBuf::from("/some/dir")),
            file: None,
            config: None,
            output: None,
        };
        let target = target_from_analyze_args(&args).unwrap();
        match target {
            Target::Dir(p) => assert_eq!(p, PathBuf::from("/some/dir")),
            _ => panic!("expected Target::Dir"),
        }
    }

    #[test]
    fn target_defaults_to_current_dir() {
        let args = args::AnalyzeArgs {
            dir: None,
            file: None,
            config: None,
            output: None,
        };
        let target = target_from_analyze_args(&args).unwrap();
        match target {
            Target::Dir(p) => {
                let cwd = std::env::current_dir().unwrap();
                assert_eq!(p, cwd);
            }
            _ => panic!("expected Target::Dir"),
        }
    }

    #[test]
    fn target_path_helper_returns_inner_path() {
        assert_eq!(
            target_path(&Target::File(PathBuf::from("/x.mp4"))),
            PathBuf::from("/x.mp4")
        );
        assert_eq!(
            target_path(&Target::Dir(PathBuf::from("/y"))),
            PathBuf::from("/y")
        );
    }

    #[test]
    fn analyze_format_field_defaults_to_none() {
        // No config section -> `analyze.format` is None. main.rs then
        // resolves None -> AnalyzeFormatArg::Csv (the CLI-level default).
        let an = AnalyzeConfig::default();
        assert!(an.format.is_none());
    }
}
