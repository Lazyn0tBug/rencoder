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
    data::extract::probe_video_metadata,
    encoder::nvenc::{self, CapabilityVerdicts},
    pipeline::{
        batch::transcode_batch,
        cache::{cache_from_resolution, init_encoder_cache},
    },
    types::{EncodeParams, EncodingOutcome, ProcessOutput, SkippedReason, Target},
};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use tracing::{info, instrument, warn};

fn main() -> Result<()> {
    tracing_init::init().context("initialize tracing")?;
    let cli = args::Cli::parse();
    match cli.command {
        args::Command::Encode(encode_args) => encode_cmd(encode_args, cli.detect),
        args::Command::Analyze(analyze_args) => analyze_cmd(analyze_args),
        args::Command::Detect(_) => detect_cmd(),
    }
}

/// `rencoder detect` -- probe this machine's NVENC capability, store the
/// result next to the config, and print a report.
///
/// The report is the point: "which hardware encoders can this machine
/// actually run" becomes something the user can ask, instead of
/// something they only learn from a fallback warning.
fn detect_cmd() -> Result<()> {
    let cache_path = rencoder_lib::config::path::resolve_nvenc_cache();
    let resolution =
        nvenc::resolve_and_store(cache_path.as_deref(), true).context("NVENC detection failed")?;
    print!("{}", nvenc::report(&resolution.record));
    Ok(())
}

/// What the user answered when advised that the stored capability is
/// past its review date.
enum Refresh {
    /// Re-probe now.
    Yes,
    /// Keep the stored result; do not ask again for 30 days.
    No,
    /// Not interactive -- keep the stored result, ask nothing.
    Skip,
}

/// Advise the user that the stored NVENC capability may be stale.
///
/// The record stays valid until the user says otherwise -- nobody knows
/// the state of a machine better than its owner. A decline pushes the
/// next advice 30 days out so we never nag on every run.
fn advise_refresh() -> Result<Refresh> {
    if !std::io::stdin().is_terminal() {
        println!(
            "NVENC 检测结果已超过 {} 天，如需更新请运行 `rencoder detect`。",
            nvenc::REVIEW_INTERVAL_DAYS
        );
        return Ok(Refresh::Skip);
    }
    print!(
        "NVENC 检测结果已超过 {} 天，可能已过期。是否重新检测？[y/N] ",
        nvenc::REVIEW_INTERVAL_DAYS
    );
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Refresh::Yes,
        _ => Refresh::No,
    })
}

/// `rencoder encode ...` -- full pipeline: load config, init encoder
/// cache, install SIGINT handler, collect files, run the batch,
/// write per-file records to `<app_dir>/data/encoding-*.parquet`
/// (U18; honors `$RENENCODER_DATA_DIR`).
#[instrument(skip_all, fields(files = tracing::field::Empty))]
fn encode_cmd(args: args::EncodeArgs, force_detect: bool) -> Result<()> {
    let app_config = load_app_config(args.config.as_deref())?;

    let enc = app_config.encode.as_ref();

    let target = if let Some(file) = &args.file {
        Target::File(file.clone())
    } else if let Some(dir) = &args.dir {
        Target::Dir(dir.clone())
    } else {
        anyhow::bail!("either --file or --dir must be provided (or use --interactive)");
    };

    // One mapping from the config to the per-run params:
    // `EncodeParams::from_app_config` is the single entry point (design
    // doc §3.10.2) -- the UI uses the same one. Hand-rolling it here is
    // how the two frontends drifted apart (the frame-rate tolerance
    // ended up 0.05 in the CLI and 0.01 in the UI).
    let mut params = EncodeParams::from_app_config(app_config.encode.as_ref());
    params.target = target.clone();
    let extensions = params.extensions.clone();

    // v1.2 follow-up #5: batch cap. env > TOML > default.
    // `resolve_max_batch_size` handles `RENENCODER_MAX_BATCH_SIZE=0`
    // (warn + fallback) and parse errors (warn + fallback), so a
    // misconfigured env var doesn't silently allow a 0-cap run that
    // would trip on entry #1.
    let max_batch_size = rencoder_lib::config::app::resolve_max_batch_size(enc);

    let files = collect_files(&target, &extensions, max_batch_size).with_context(|| {
        format!(
            "failed to collect files from {}",
            target_path(&target).display()
        )
    })?;

    if files.is_empty() {
        warn!("no files matched; nothing to do");
        return Ok(());
    }

    // CLI > config: `--nv` / `--no-nv` override `[encode].nv`. NVENC is
    // the most machine-dependent knob there is -- the same config file
    // has to work on a workstation with an RTX and on a laptop without
    // one, so the CLI has to be able to flip it per run.
    let config_nv = enc.and_then(|e| e.nv).unwrap_or(false);
    let enable_nv = if args.nv {
        true
    } else if args.no_nv {
        false
    } else {
        config_nv
    };

    let config = Config::from_encode_config(app_config.encode.as_ref());

    // Parse each `--` passthrough entry through the type-level safety
    // boundary. A bad entry fails fast here with a precise error
    // pointing at the offending argv token — `EncodeParams` itself
    // can only be constructed with a validated `Vec<ExtraArg>`.
    let extra_args: Vec<_> = args
        .extra_args
        .iter()
        .map(|s| {
            rencoder_lib::encoder::ffmpeg_arg::ExtraArg::parse(s)
                .map_err(|e| anyhow::anyhow!("invalid ffmpeg passthrough argument {s:?}: {e}"))
        })
        .collect::<Result<_, _>>()?;

    params.extra_args = extra_args;

    info!(
        "collected {} file(s), target format: {:?}",
        files.len(),
        params.format
    );

    // NVENC capability: prefer the stored record (probing costs
    // ~200-500ms per encoder, and the answer barely changes). `--detect`
    // forces a fresh probe; a record past its review date only gets the
    // user asked about it.
    let cache = if enable_nv {
        let cache_path = rencoder_lib::config::path::resolve_nvenc_cache();
        let mut resolution = nvenc::resolve_and_store(cache_path.as_deref(), force_detect)
            .context("NVENC detection failed")?;

        if resolution.review_due && !force_detect {
            match advise_refresh()? {
                Refresh::Yes => {
                    resolution = nvenc::resolve_and_store(cache_path.as_deref(), true)
                        .context("NVENC detection failed")?;
                }
                Refresh::No => {
                    // Nothing to do: `resolve_and_store` already pushed
                    // the review date 30 days out when it reported the
                    // expired record, so the next run stays quiet.
                }
                Refresh::Skip => {}
            }
        }

        // `resolve_and_store` already persisted a freshly probed record.
        cache_from_resolution(&resolution)
    } else {
        init_encoder_cache(false).with_context(|| "failed to initialize encoder cache")?
    };

    // Install signal handler; the per-batch loop registers/unregisters
    // the current output path so a SIGINT cleans up the partial file.
    let current_output = signal::install_handler();

    // The run's NVENC verdicts: an encoder that fails at runtime is
    // not tried again for the rest of this command.
    let mut verdicts = CapabilityVerdicts::default();

    let records = transcode_batch(
        &params,
        &files,
        &cache,
        &config,
        &mut verdicts,
        ProcessOutput::Inherit,
        Some(&current_output),
        None,
    );

    // Default output: timestamped Parquet in <app_dir>/data/ (U18; honors
    // $RENENCODER_DATA_DIR). Extension of -o picks the format: .parquet
    // | .jsonl | .csv.
    let output_path = match &args.output {
        Some(p) => p.clone(),
        None => default_data_path()?,
    };
    write_records(&records, &output_path)?;
    print_encode_summary(&records);

    Ok(())
}

/// `<app_dir>/data/encoding-<UTC-ISO8601>Z.parquet` -- one file per run,
/// named by start time. Uses the canonical UTC timestamp formatter from
/// `rencoder_lib::utils::time` so the format stays in sync with the
/// library's docs. Honors `$RENENCODER_DATA_DIR` (U18 step 11).
///
/// Filesystem errors (directory creation / mode bits) propagate up via
/// `Result` so the user sees a clean error instead of a panic. When
/// `resolve_data_dir` returns `None` (app_dir undeterminable and no
/// env override), falls back to CWD-relative `data/` so legacy hosts
/// without a determinable binary dir still produce output (tracing_init
/// has the same fallback for `log/`).
fn default_data_path() -> anyhow::Result<PathBuf> {
    use anyhow::Context;

    let dir =
        rencoder_lib::config::path::resolve_data_dir().unwrap_or_else(|| PathBuf::from("data"));
    std::fs::create_dir_all(&dir).context("create data dir")?;
    rencoder_lib::config::fs_perms::set_app_dir_mode(&dir).context("set data dir mode bits")?;
    Ok(dir.join(format!(
        "encoding-{}.parquet",
        rencoder_lib::utils::time::utc_timestamp()
    )))
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
#[instrument(skip_all, fields(dir = ?args.dir, file = ?args.file))]
fn analyze_cmd(args: args::AnalyzeArgs) -> Result<()> {
    let app_config = load_app_config(args.config.as_deref())?;
    let an = app_config.analyze.as_ref();

    let target =
        target_from_analyze_args(&args).with_context(|| "failed to determine target directory")?;

    let extensions = an.and_then(|a| a.extensions.clone()).unwrap_or_default();

    // v1.2 follow-up #5: `analyze` is read-only and per-file
    // bounded by wall-clock (no ffmpeg). Same cap-resolution as
    // encode: env > TOML > default. Note: `AnalyzeConfig` has no
    // own `max_batch_size` field today — the encode config's
    // cap is shared with analyze (treating cap as a global
    // collector knob). If we ever want per-section caps, that's
    // a follow-up.
    let max_batch_size =
        rencoder_lib::config::app::resolve_max_batch_size(app_config.encode.as_ref());

    let files = collect_files(&target, &extensions, max_batch_size).with_context(|| {
        format!(
            "failed to collect files from {}",
            target_path(&target).display()
        )
    })?;

    info!("analyzing {} file(s)", files.len());

    let parallel = an.and_then(|a| a.parallel).unwrap_or(false);
    tracing::Span::current().record("parallel", parallel);

    let records = if parallel {
        // The parallel analyzer takes `&[PathBuf]` and bypasses the
        // `InputPath` boundary (probe-only, no argv injection risk).
        // Convert at the CLI boundary; the lib's signature stays
        // untouched per the review #109 plan.
        let paths: Vec<std::path::PathBuf> =
            files.iter().map(|ip| ip.as_path().to_path_buf()).collect();
        rencoder_lib::analyzer::records::analyze_records_parallel(&paths)
            .with_context(|| "fps analysis failed")?
    } else {
        // U19 step 5: serial analyze routes through TaskManager so the
        // scaffold logs (batch start/end + per-task start/end) are
        // emitted by the manager rather than baked into the probe
        // function. analyze_records_with_probe stays pure: input ->
        // records, no log calls. The parallel variant keeps its own
        // log path because rayon's worker pool has no ordering
        // guarantee.
        let tasks: Vec<rencoder_lib::types::TranscodeTask> = files
            .iter()
            .map(|input| rencoder_lib::types::TranscodeTask {
                input: input.clone(),
                output: None,
            })
            .collect();
        let manager = rencoder_lib::pipeline::task::TaskManager::new(None);
        manager
            .run(&tasks, "analyze", target_path(&target), |task, _ctx| {
                let file_bytes = std::fs::metadata(task.input.as_path())
                    .map(|m| m.len())
                    .unwrap_or(0);
                match probe_video_metadata(task.input.as_path()) {
                    Ok(metadata) => Ok(rencoder_lib::analyzer::records::AnalysisRecord::new(
                        task.input.as_path().to_path_buf(),
                        metadata.best_fps().map(|f| f.round() as u32),
                        metadata.codec,
                        metadata.duration_secs,
                        file_bytes,
                    )),
                    Err(_) => Ok(rencoder_lib::analyzer::records::AnalysisRecord::new(
                        task.input.as_path().to_path_buf(),
                        None,
                        None,
                        None,
                        file_bytes,
                    )),
                }
            })
            .unwrap_or_default()
    };
    let stats = rencoder_lib::analyzer::records::aggregate_records(&records);

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
            render(rencoder_lib::analyzer::records::records_to_csv(&records))?;
        }
        AnalyzeFormatArg::Table => {
            let df = rencoder_lib::analyzer::records::records_to_dataframe(&records)
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
fn records_to_jsonl(records: &[rencoder_lib::analyzer::records::AnalysisRecord]) -> String {
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
    // PartialOutput skips are NOT counted as `Skipped` (they leave a
    // partial file behind, not a clean skip) and NOT counted as
    // `Failed` (ffmpeg was never invoked). Surfaced separately so the
    // user notices this rare actionable state (typically permission
    // denied or file in use). See design.md §3.4 + §8 item 41.
    let partial_skip = results
        .iter()
        .filter(|r| {
            matches!(
                r.outcome,
                EncodingOutcome::Skipped(SkippedReason::PartialOutput)
            )
        })
        .count();
    let failed = results
        .iter()
        .filter(|r| matches!(r.outcome, EncodingOutcome::Failed))
        .count();

    println!();
    println!("Summary:");
    println!("  Total:       {total}");
    println!("  Success:     {success}");
    println!("  Reduced:     {reduced}");
    println!("  Skipped:     {skipped}");
    println!("  PartialSkip: {partial_skip}");
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
        assert!(cfg.encoders.av1.is_none());
    }

    #[test]
    fn config_from_encode_config_picks_up_encoders() {
        let enc = EncodeConfig {
            encoders: Some(EncodersConfig {
                av1: Some("libaom-av1".into()),
                vp9: Some("libvpx-vp9".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cfg = Config::from_encode_config(Some(&enc));
        assert_eq!(cfg.encoders.av1.as_deref(), Some("libaom-av1"));
        assert_eq!(cfg.encoders.vp9.as_deref(), Some("libvpx-vp9"));
        assert!(cfg.encoders.x264.is_none());
    }

    /// The CLI and the UI must agree on the run parameters for the same
    /// config -- design doc §3.10.2 ("one field, one conversion entry
    /// point"). The CLI builds its params through
    /// `EncodeParams::from_app_config`; this pins that it keeps doing so
    /// (the frame-rate tolerance used to differ: 0.05 here, 0.01 in the
    /// UI).
    #[test]
    fn cli_params_match_the_shared_mapping() {
        let enc = EncodeConfig {
            format: Some(FormatArg::Hevc),
            crf: 28,
            max_fps: Some(30),
            extensions: Some(vec!["mp4".into()]),
            ..Default::default()
        };
        let params = EncodeParams::from_app_config(Some(&enc));
        assert_eq!(params.format, rencoder_lib::types::VideoFormat::Hevc);
        assert!(params.is_crf);
        assert_eq!(params.bitrate_or_crf, 28);
        assert_eq!(params.extensions, vec!["mp4".to_string()]);
        assert_eq!(params.max_fps.max_fps, 30.0);
        assert_eq!(
            params.max_fps.eps,
            rencoder_lib::types::MaxFpsConfig::default().eps
        );
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
